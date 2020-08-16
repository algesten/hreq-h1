use futures_util::{AsyncReadExt, AsyncWriteExt};
use hreq_h1::buf_reader::BufIo;
use hreq_h1::Error;

mod common;

#[async_std::test]
async fn server_request_200_ok() -> Result<(), Error> {
    let conn = common::run_server(|parts, body, respond, _| async move {
        assert_eq!(parts.method, "GET");
        assert_eq!(parts.uri.path(), "/path");

        let res = http::Response::builder()
            .header("content-length", 2)
            .body(())
            .unwrap();

        assert_eq!(body.unwrap(), b"");

        let body_send = respond.send_response(res, false).unwrap();

        common::send_body_chunks(body_send, b"OK", 1).await.unwrap();

        Ok(false)
    })
    .await?;

    let tcp = conn.connect().await?;
    let mut brd = BufIo::with_capacity(8192, tcp);

    brd.write_all(b"GET /path HTTP/1.1\r\n\r\n").await?;

    let head = common::read_header(&mut brd).await?;
    assert_eq!(head, "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n");

    let mut buf = [0; 2];
    brd.read_exact(&mut buf).await?;

    assert_eq!(&buf, b"OK");

    Ok(())
}

#[async_std::test]
async fn server_big_body_clen() -> Result<(), Error> {
    let conn = common::run_server(|parts, body, respond, _| async move {
        assert_eq!(parts.method, "GET");
        assert_eq!(parts.uri.path(), "/path");

        let res = http::Response::builder()
            .header("content-length", 10 * 1024 * 1024)
            .body(())
            .unwrap();

        assert_eq!(body.unwrap(), b"");

        let body_send = respond.send_response(res, false).unwrap();

        // 10 MB
        let big = vec![42_u8; 10 * 1024 * 1024];

        common::send_body_chunks(body_send, &big, 12_347)
            .await
            .unwrap();

        Ok(false)
    })
    .await?;

    let tcp = conn.connect().await?;
    let mut brd = BufIo::with_capacity(8192, tcp);

    brd.write_all(b"GET /path HTTP/1.1\r\n\r\n").await?;

    let head = common::read_header(&mut brd).await?;
    assert_eq!(head, "HTTP/1.1 200 OK\r\ncontent-length: 10485760\r\n\r\n");

    let mut total = 0;
    let mut buf = vec![0_u8; 8192];
    let cmp = vec![42_u8; 8192];
    while total < 10485760 {
        let amount = brd.read(&mut buf).await?;

        assert_eq!(&buf[0..amount], &cmp[0..amount]);

        total += amount;
    }

    Ok(())
}
