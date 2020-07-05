use futures_util::{AsyncReadExt, AsyncWriteExt};
use hreq_h1::Error;

mod common;

#[async_std::test]
async fn reuse_connection() -> Result<(), Error> {
    let conn = common::serve(move |head, mut tcp, count| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        let res = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
        tcp.write_all(res).await.unwrap();

        // reuse 5 times.
        Ok((tcp, count <= 4))
    })
    .await?;

    let tcp = conn.connect().await?;
    let (mut send, drive) = hreq_h1::client::handshake(tcp);
    async_std::task::spawn(async move {
        match drive.await {
            Ok(_) => {
                // println!("drive clean exit");
            }
            Err(_e) => {
                // println!("drive exit: {:?}", _e);
            }
        }
    });

    // send 5 requests over the same connetion.
    for _i in 0..5 {
        let req = http::Request::get("/path").body(())?;
        let (fut, _) = send.send_request(req, true)?;
        let res = fut.await?;
        assert_eq!(res.status(), 200);
    }

    // 6th should fail
    conn.connect()
        .await
        .expect_err("Connection should be refused");

    Ok(())
}

#[async_std::test]
async fn http11_connection_close() -> Result<(), Error> {
    let conn = common::serve(move |head, mut tcp, _| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        // send connection: close.
        let res = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nconnection: close\r\n\r\nOK";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, true))
    })
    .await?;

    let tcp = conn.connect().await?;
    let (mut send, drive) = hreq_h1::client::handshake(tcp);
    async_std::task::spawn(async move {
        match drive.await {
            Ok(_) => {
                // println!("drive clean exit");
            }
            Err(_e) => {
                // println!("drive exit: {:?}", _e);
            }
        }
    });

    let req = http::Request::get("/path").body(())?;
    let (fut, _) = send.send_request(req, true)?;
    let res = fut.await?;
    assert_eq!(res.status(), 200);

    // connection should close after we finished reading the body.
    let (_, mut body) = res.into_parts();
    let mut v = vec![];
    body.read_to_end(&mut v).await?;

    // 6th should fail
    conn.connect()
        .await
        .expect_err("Connection should be refused");

    Ok(())
}
