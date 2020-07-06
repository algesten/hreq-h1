use futures_util::{AsyncReadExt, AsyncWriteExt};
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

        let mut send = respond.send_response(res, false).unwrap();

        send.send_data(b"OK", true).await.unwrap();

        Ok(false)
    })
    .await?;

    let mut tcp = conn.connect().await?;

    tcp.write_all(b"GET /path HTTP/1.1\r\n\r\n").await?;

    let head = common::read_header(&mut tcp).await?;
    assert_eq!(head, "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n");

    let mut buf = [0; 2];
    tcp.read_exact(&mut buf).await?;

    assert_eq!(&buf, b"OK");

    Ok(())
}
