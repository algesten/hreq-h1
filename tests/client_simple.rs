use futures_util::AsyncWriteExt;
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
