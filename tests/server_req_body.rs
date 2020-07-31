use futures_util::AsyncWriteExt;
use hreq_h1::Error;

mod common;

#[async_std::test]
async fn server_request_with_body_clen() -> Result<(), Error> {
    let conn = common::run_server(|parts, body, respond, _| async move {
        assert_eq!(parts.method, "GET");
        assert_eq!(parts.uri.path(), "/path");

        assert_eq!(&body.unwrap(), b"OK\n");

        let res = http::Response::builder().body(()).unwrap();

        respond.send_response(res, true).unwrap();

        Ok(false)
    })
    .await?;

    let mut tcp = conn.connect().await?;

    tcp.write_all(b"GET /path HTTP/1.1\r\ncontent-length: 3\r\n\r\nOK\n")
        .await?;

    let head = common::read_header(&mut tcp).await?;
    assert_eq!(head, "HTTP/1.1 200 OK\r\n\r\n");

    Ok(())
}

#[async_std::test]
async fn server_request_with_body_chunked() -> Result<(), Error> {
    let conn = common::run_server(|parts, body, respond, _| async move {
        assert_eq!(parts.method, "GET");
        assert_eq!(parts.uri.path(), "/path");

        assert_eq!(&body.unwrap(), b"OK\n");

        let res = http::Response::builder().body(()).unwrap();

        respond.send_response(res, true).unwrap();

        Ok(false)
    })
    .await?;

    let mut tcp = conn.connect().await?;

    tcp.write_all(
        b"GET /path HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n3\r\nOK\n\r\n0\r\n\r\n",
    )
    .await?;

    let head = common::read_header(&mut tcp).await?;
    assert_eq!(head, "HTTP/1.1 200 OK\r\n\r\n");

    Ok(())
}
