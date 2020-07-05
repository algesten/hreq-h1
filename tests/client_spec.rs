use futures_util::{AsyncReadExt, AsyncWriteExt};
use hreq_h1::Error;

mod common;
use common::HeaderMapExt;

#[async_std::test]
async fn post_clen_and_chunked() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(
            head,
            "POST /path HTTP/1.1\r\ncontent-length: 5\r\ntransfer-encoding: chunked\r\n\r\n"
        );

        let mut buf = [0; 10];
        tcp.read_exact(&mut buf).await?;

        // NB: spec says that when both content-length and transfer-encoding chunked, use chunked.
        assert_eq!(&buf, b"5\r\nHELLO\r\n");

        let res = b"HTTP/1.1 200 OK\r\n\r\n";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    let req = http::Request::post("/path")
        .header("content-length", 5)
        .header("transfer-encoding", "chunked")
        .body("HELLO")
        .unwrap();

    common::run(conn.connect().await?, req).await?;

    Ok(())
}

#[async_std::test]
async fn expect_no_body_on_head() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(head, "HEAD /path HTTP/1.1\r\n\r\n");

        // NB: Sending body on HEAD is against spec. Here we send body,
        // which break the connection, but we mustn't see the body
        // client side.
        //
        // The problem will surface on a keep-alive/reuse connection where
        // the next response header will be broken.
        let res = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    let req = http::Request::head("/path").body("").unwrap();

    let (res, body) = common::run(conn.connect().await?, req).await?;

    assert_eq!(res.status, 200);
    assert_eq!(res.headers.get_as("content-length"), Some(2));
    assert_eq!(body, b""); // this is important

    Ok(())
}

async fn expect_no_body_on_status(status: u16) -> Result<(), Error> {
    let conn = common::serve(move |head, mut tcp, _| async move {
        assert_eq!(head, "HEAD /path HTTP/1.1\r\n\r\n");

        // NB: Sending body on HEAD is against spec. Here we send body,
        // which break the connection, but we mustn't see the body
        // client side.
        //
        // The problem will surface on a keep-alive/reuse connection where
        // the next response header will be broken.
        let res = format!("HTTP/1.1 {} OK\r\nContent-Length: 2\r\n\r\nOK", status);
        tcp.write_all(res.as_bytes()).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    let req = http::Request::head("/path").body("").unwrap();

    let (res, body) = common::run(conn.connect().await?, req).await?;

    assert_eq!(res.status, status);
    assert_eq!(res.headers.get_as("content-length"), Some(2));
    assert_eq!(body, b""); // this is important

    Ok(())
}

#[async_std::test]
async fn expect_no_body_on_102() -> Result<(), Error> {
    Ok(expect_no_body_on_status(102).await?)
}

#[async_std::test]
async fn expect_no_body_on_204() -> Result<(), Error> {
    Ok(expect_no_body_on_status(204).await?)
}

#[async_std::test]
async fn expect_no_body_on_304() -> Result<(), Error> {
    Ok(expect_no_body_on_status(304).await?)
}
