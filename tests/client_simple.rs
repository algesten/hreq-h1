use futures_util::{AsyncReadExt, AsyncWriteExt};
use hreq_h1::Error;

mod common;
use common::HeaderMapExt;

#[async_std::test]
async fn request_200_ok() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\naccept: */*\r\n\r\n");

        let res = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    let req = http::Request::get("/path")
        .header("accept", "*/*")
        .body("")
        .unwrap();

    let (parts, body) = common::run(conn.connect().await?, req).await?;

    assert_eq!(parts.status, 200);
    assert_eq!(parts.headers.get_as("content-length"), Some(2));

    assert_eq!(&body, &b"OK");

    Ok(())
}

#[async_std::test]
async fn post_body() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(head, "POST /path HTTP/1.1\r\ncontent-length: 4\r\n\r\n");

        let mut buf = [0, 0, 0, 0];
        tcp.read_exact(&mut buf).await?;

        assert_eq!(&buf, b"data");

        let res = b"HTTP/1.1 200 OK\r\n\r\n";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    let req = http::Request::post("/path")
        .header("content-length", 4)
        .body("data")
        .unwrap();

    let (parts, _) = common::run(conn.connect().await?, req).await?;

    assert_eq!(parts.status, 200);

    Ok(())
}

#[async_std::test]
async fn post_body_no_len() -> Result<(), Error> {
    let conn = common::serve(|head, tcp, _| async move {
        assert_eq!(head, "PUT /path HTTP/1.1\r\n\r\n");

        Ok((tcp, false))
    })
    .await?;

    // no declared length and yet sending a body, it's an error
    let req = http::Request::put("/path").body("data").unwrap();

    let ret = common::run(conn.connect().await?, req).await;

    assert!(ret.is_err());

    let err = ret.unwrap_err();

    assert_eq!(err.to_string(), "Body data is not expected");

    Ok(())
}

#[async_std::test]
async fn post_big_body_clen() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(
            head,
            "POST /path HTTP/1.1\r\ncontent-length: 10485760\r\n\r\n"
        );

        let mut total = 0;
        let mut buf = vec![0_u8; 8192];
        let cmp = vec![42_u8; 8192];
        while total < 10485760 {
            let amount = tcp.read(&mut buf).await?;

            assert_eq!(&buf[0..amount], &cmp[0..amount]);

            total += amount;
        }

        let res = b"HTTP/1.1 200 OK\r\n\r\n";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    // 10 MB
    let big = vec![42_u8; 10 * 1024 * 1024];

    let req = http::Request::post("/path")
        .header("content-length", big.len())
        .body(big)
        .unwrap();

    let (parts, _) = common::run(conn.connect().await?, req).await?;

    assert_eq!(parts.status, 200);

    Ok(())
}

#[async_std::test]
async fn post_big_body_chunked() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(
            head,
            "POST /path HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n"
        );

        let mut decode = hreq_h1::chunked::ChunkedDecoder::new();

        let mut total = 0;
        let mut buf = vec![0_u8; 8192];
        let cmp = vec![42_u8; 8192];
        loop {
            let amount = decode.read(&mut tcp, &mut buf).await?;

            if amount == 0 {
                break;
            }

            assert_eq!(&buf[0..amount], &cmp[0..amount]);

            total += amount;

            assert!(total <= 10 * 1024 * 1024);
        }

        let res = b"HTTP/1.1 200 OK\r\n\r\n";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    // 10 MB
    let big = vec![42_u8; 10 * 1024 * 1024];

    let req = http::Request::post("/path")
        .header("transfer-encoding", "chunked")
        .body(big)
        .unwrap();

    let (parts, _) = common::run(conn.connect().await?, req).await?;

    assert_eq!(parts.status, 200);

    Ok(())
}
