use futures_util::{AsyncReadExt, AsyncWriteExt};
use hreq_h1::Error;

mod common;

#[async_std::test]
async fn server_request_with_body_clen() -> Result<(), Error> {
    let conn = common::run_server(|parts, body, respond, _| async move {
        assert_eq!(parts.method, "POST");
        assert_eq!(parts.uri.path(), "/path");

        assert_eq!(&body.unwrap(), b"OK\n");

        let res = http::Response::builder()
            .header("content-length", "0")
            .header("connection", "close")
            .body(())
            .unwrap();

        respond.send_response(res, true).unwrap();

        Ok(false)
    })
    .await?;

    let mut tcp = conn.connect().await?;

    tcp.write_all(b"POST /path HTTP/1.1\r\ncontent-length: 3\r\n\r\nOK\n")
        .await?;

    let head = common::read_header(&mut tcp).await?;
    assert_eq!(
        head,
        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );

    let mut buf = [0_u8; 1];
    if let Some(read) = tcp.read(&mut buf).await.ok() {
        assert_eq!(read, 0);
    }

    Ok(())
}

#[async_std::test]
async fn server_request_with_body_chunked() -> Result<(), Error> {
    let conn = common::run_server(|parts, body, respond, _| async move {
        assert_eq!(parts.method, "POST");
        assert_eq!(parts.uri.path(), "/path");

        assert_eq!(&body.unwrap(), b"OK\n");

        let res = http::Response::builder()
            .header("content-length", "0")
            .header("connection", "close")
            .body(())
            .unwrap();

        respond.send_response(res, true).unwrap();

        Ok(false)
    })
    .await?;

    let mut tcp = conn.connect().await?;

    tcp.write_all(
        b"POST /path HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n3\r\nOK\n\r\n0\r\n\r\n",
    )
    .await?;

    let head = common::read_header(&mut tcp).await?;
    assert_eq!(
        head,
        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );

    let mut buf = [0_u8; 1];
    if let Some(read) = tcp.read(&mut buf).await.ok() {
        assert_eq!(read, 0);
    }

    Ok(())
}

#[async_std::test]
async fn server_request_with_body_dropped() -> Result<(), Error> {
    common::setup_logger();

    use async_std::net::TcpListener;
    use common::Connector;

    let l = TcpListener::bind("127.0.0.1:0").await?;
    let p = l.local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{}", p);

    async_std::task::spawn(async move {
        let (tcp, _) = l.accept().await.expect("Accept incoming");

        let mut conn = hreq_h1::server::handshake(tcp);

        let (req, respond) = conn.accept().await.unwrap().expect("Handshaken");

        let (_, recv_body) = req.into_parts();

        // this is what we're testing, dropping the recv_body, ignoring the incoming
        // request body and then send a response anyway.
        drop(recv_body);

        let mut send_body = respond
            .send_response(
                http::Response::builder()
                    .header("transfer-encoding", "chunked")
                    .body(())
                    .unwrap(),
                false,
            )
            .expect("send_response");

        send_body.send_data(&[], true).await.unwrap();
    });

    let conn = Connector(addr);
    let mut tcp = conn.connect().await?;

    tcp.write_all(b"POST /path HTTP/1.1\r\ncontent-length: 0\r\n\r\n")
        .await?;

    let head = common::read_header(&mut tcp).await?;
    assert_eq!(
        head,
        "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n"
    );

    let mut buf = [0_u8; 5];
    tcp.read(&mut buf).await?;

    assert_eq!(&buf, b"0\r\n\r\n");

    let mut buf = [0_u8; 1];
    if let Some(read) = tcp.read(&mut buf).await.ok() {
        assert_eq!(read, 0);
    }

    Ok(())
}
