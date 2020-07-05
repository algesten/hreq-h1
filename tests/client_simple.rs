use futures_util::{AsyncReadExt, AsyncWriteExt};
use hreq_h1::Error;
use tokio::net::{TcpListener, TcpStream};

mod common;

use common::HeaderMapExt;

#[tokio::test]
async fn request_200_ok() -> Result<(), Error> {
    common::setup_logger();

    let mut l = TcpListener::bind("127.0.0.1:0").await?;
    let p = l.local_addr()?.port();

    const RESP: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";

    tokio::spawn(async move {
        let (inc, _) = l.accept().await.unwrap();
        let mut inc = common::from_tokio(inc);
        let s = common::read_header(&mut inc).await.unwrap();
        inc.write_all(RESP.as_bytes()).await.unwrap();
    });

    let conn = TcpStream::connect(format!("127.0.0.1:{}", p)).await?;

    let (mut send, conn) = hreq_h1::client::handshake(common::from_tokio(conn));

    tokio::spawn(async move {
        conn.await.unwrap();
    });

    let req = http::Request::get("/path").body(()).unwrap();

    let (fut, _) = send.send_request(req, true)?;
    let res = fut.await?;

    let (parts, mut body) = res.into_parts();

    assert_eq!(parts.status, 200);
    assert_eq!(parts.headers.get_as("content-length"), Some(2));

    let mut s = String::new();
    body.read_to_string(&mut s).await?;

    assert_eq!("OK", s);

    Ok(())
}
