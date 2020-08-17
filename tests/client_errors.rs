use futures_util::AsyncWriteExt;
use hreq_h1::Error;

mod common;

#[async_std::test]
async fn broken_chunked() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        // NB: Malformed chunked.
        let res = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\nHELLO";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    let req = http::Request::get("/path").body("").unwrap();

    let err = common::run(conn.connect().await?, req)
        .await
        .expect_err("partial response");

    assert_eq!(err.to_string(), "Unexpected char in chunk size: \'H\'");

    Ok(())
}

#[async_std::test]
async fn partial_response_header() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        let res = b"HTTP/1.1 200 OK\r\nContent-Len";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    let req = http::Request::get("/path").body("").unwrap();

    let err = common::run(conn.connect().await?, req)
        .await
        .expect_err("partial response");

    assert_eq!(err.to_string(), "EOF before complete http11 header");

    Ok(())
}

#[async_std::test]
async fn partial_response_clen() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        // NB: content-length 10 and we send just "OK", then drop connection.
        let res = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nOK";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    let req = http::Request::get("/path").body("").unwrap();

    let err = common::run(conn.connect().await?, req)
        .await
        .expect_err("partial response");

    assert_eq!(
        err.to_string(),
        "Partial body received 2 bytes and expected 10"
    );

    Ok(())
}

#[async_std::test]
async fn partial_response_chunked() -> Result<(), Error> {
    let conn = common::serve(|head, mut tcp, _| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        // NB: 1f in chunk size, write "nHELLO" then drop.
        let res = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1f\r\nHELLO";
        tcp.write_all(res).await.unwrap();

        Ok((tcp, false))
    })
    .await?;

    let req = http::Request::get("/path").body("").unwrap();

    let err = common::run(conn.connect().await?, req)
        .await
        .expect_err("partial response");

    assert_eq!(err.to_string(), "Partial body");

    Ok(())
}

#[async_std::test]
async fn post_larger_than_clen() -> Result<(), Error> {
    let conn = common::serve(|_, tcp, _| async move { Ok((tcp, false)) }).await?;

    let req = http::Request::post("/path")
        .header("content-length", 2)
        .body("HELLO")
        .unwrap();

    let err = common::run(conn.connect().await?, req)
        .await
        .expect_err("partial response");

    assert_eq!(
        err.to_string(),
        "Body data longer than content-length header: 5 > 2"
    );

    Ok(())
}

#[async_std::test]
async fn get_response_never_comes() -> Result<(), Error> {
    use futures_util::future::poll_fn;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::Poll;

    common::setup_logger();

    let conn = common::serve(|_, tcp, _| async move {
        poll_fn(|_| Poll::<()>::Pending).await;
        Ok((tcp, false))
    })
    .await?;

    let switch = Arc::new(AtomicBool::new(true));
    let switch2 = switch.clone();

    async_std::task::spawn(async move {
        let req = http::Request::get("/path")
            .header("content-length", 0)
            .body(b"")
            .unwrap();

        let _ = common::run(conn.connect().await.unwrap(), req).await;

        // this should never happen, because the response headers are never sent.
        switch2.store(false, Ordering::SeqCst);
    });

    async_std::task::sleep(std::time::Duration::from_millis(500)).await;

    assert!(switch.load(Ordering::SeqCst));

    Ok(())
}

#[async_std::test]
async fn get_body_never_comes() -> Result<(), Error> {
    use async_std::net::TcpListener;
    use futures_util::future::poll_fn;
    use hreq_h1::buf_reader::BufIo;
    use std::task::Poll;

    common::setup_logger();

    let l = TcpListener::bind("127.0.0.1:0").await?;
    let p = l.local_addr()?.port();

    async_std::task::spawn(async move {
        let (tcp, _) = l.accept().await.expect("Accept failed");
        let mut brd = BufIo::with_capacity(16_384, tcp);

        let _ = match common::test_read_header(&mut brd).await {
            Ok(v) => v,
            Err(_e) => panic!("Error while reading header"),
        };

        // we say we're sending 1 byte body, but then don't. teehee :)
        let res = b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n";
        brd.write_all(res).await.unwrap();

        brd.flush().await.expect("Flush fail");

        // wait forever
        poll_fn(|_| Poll::<()>::Pending).await;
    });

    let addr = format!("127.0.0.1:{}", p);
    let conn = common::Connector(addr);

    let tcp = conn.connect().await?;

    let (mut send, conn) = hreq_h1::client::handshake(tcp);

    async_std::task::spawn(async move {
        if let Err(_e) = conn.await {
            // println!("{:?}", _e);
        }
    });

    let req = http::Request::get("/path")
        .header("content-length", 0)
        .body(())
        .unwrap();

    let (fut, _) = send.send_request(req, true)?;

    let res = fut.await;

    let res = res.expect("Response headers");

    assert_eq!(res.status(), 200);

    Ok(())
}
