use futures_util::{AsyncReadExt, AsyncWriteExt};
use hreq_h1::Error;

mod common;
use common::HeaderMapExt;

#[tokio::test]
async fn request_200_ok() -> Result<(), Error> {
    common::setup_logger();

    let conn = common::serve_once(|mut tcp| async move {
        let head = common::read_header(&mut tcp).await.unwrap();
        assert_eq!(head, "GET /path HTTP/1.1\r\naccept: */*\r\n\r\n");

        let res = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
        tcp.write_all(res).await.unwrap();

        Ok(())
    })
    .await?;

    let req = http::Request::get("/path")
        .header("accept", "*/*")
        .body("")
        .unwrap();

    let (parts, body) = common::run(conn, req).await?;

    assert_eq!(parts.status, 200);
    assert_eq!(parts.headers.get_as("content-length"), Some(2));

    assert_eq!(&body, &b"OK");

    Ok(())
}

#[tokio::test]
async fn post_body() -> Result<(), Error> {
    common::setup_logger();

    let conn = common::serve_once(|mut tcp| async move {
        let head = common::read_header(&mut tcp).await.unwrap();
        assert_eq!(head, "POST /path HTTP/1.1\r\ncontent-length: 4\r\n\r\n");

        let mut buf = [0, 0, 0, 0];
        tcp.read_exact(&mut buf).await?;

        assert_eq!(&buf, b"data");

        let res = b"HTTP/1.1 200 OK\r\n\r\n";
        tcp.write_all(res).await.unwrap();

        Ok(())
    })
    .await?;

    let req = http::Request::post("/path")
        .header("content-length", 4)
        .body("data")
        .unwrap();

    let (parts, _) = common::run(conn, req).await?;

    assert_eq!(parts.status, 200);

    Ok(())
}

#[tokio::test]
async fn post_big_body_clen() -> Result<(), Error> {
    common::setup_logger();

    let conn = common::serve_once(|mut tcp| async move {
        let head = common::read_header(&mut tcp).await.unwrap();
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

        Ok(())
    })
    .await?;

    // 10 MB
    let big = vec![42_u8; 10 * 1024 * 1024];

    let req = http::Request::post("/path")
        .header("content-length", big.len())
        .body(big)
        .unwrap();

    let (parts, _) = common::run(conn, req).await?;

    assert_eq!(parts.status, 200);

    Ok(())
}

#[tokio::test]
async fn post_big_body_chunked() -> Result<(), Error> {
    common::setup_logger();

    let conn = common::serve_once(|mut tcp| async move {
        let head = common::read_header(&mut tcp).await.unwrap();
        assert_eq!(
            head,
            "POST /path HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n"
        );

        let mut decode = hreq_h1::chunked::ChunkedDecoder::new();

        let mut total = 0;
        let mut buf = vec![0_u8; 8192];
        let cmp = vec![42_u8; 8192];
        while total < 10485760 {
            let amount = decode.read(&mut tcp, &mut buf).await?;

            assert_eq!(&buf[0..amount], &cmp[0..amount]);

            total += amount;
        }

        let res = b"HTTP/1.1 200 OK\r\n\r\n";
        tcp.write_all(res).await.unwrap();

        Ok(())
    })
    .await?;

    // 10 MB
    let big = vec![42_u8; 10 * 1024 * 1024];

    let req = http::Request::post("/path")
        .header("transfer-encoding", "chunked")
        .body(big)
        .unwrap();

    let (parts, _) = common::run(conn, req).await?;

    assert_eq!(parts.status, 200);

    Ok(())
}
