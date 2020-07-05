use futures_util::AsyncWriteExt;
use hreq_h1::Error;

mod common;

#[async_std::test]
async fn broken_chunked() -> Result<(), Error> {
    let conn = common::serve_once(|head, mut tcp| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        // NB: Malformed chunked.
        let res = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\nHELLO";
        tcp.write_all(res).await.unwrap();

        Ok(tcp)
    })
    .await?;

    let req = http::Request::get("/path").body("").unwrap();

    let err = common::run(conn, req).await.expect_err("partial response");

    assert_eq!(err.to_string(), "Unexpected char in chunk size: \'H\'");

    Ok(())
}

#[async_std::test]
async fn partial_response_header() -> Result<(), Error> {
    let conn = common::serve_once(|head, mut tcp| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        let res = b"HTTP/1.1 200 OK\r\nContent-Len";
        tcp.write_all(res).await.unwrap();

        Ok(tcp)
    })
    .await?;

    let req = http::Request::get("/path").body("").unwrap();

    let err = common::run(conn, req).await.expect_err("partial response");

    assert_eq!(err.to_string(), "EOF before complete http11 header");

    Ok(())
}

#[async_std::test]
async fn partial_response_clen() -> Result<(), Error> {
    let conn = common::serve_once(|head, mut tcp| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        // NB: content-length 10 and we send just "OK", then drop connection.
        let res = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nOK";
        tcp.write_all(res).await.unwrap();

        Ok(tcp)
    })
    .await?;

    let req = http::Request::get("/path").body("").unwrap();

    let err = common::run(conn, req).await.expect_err("partial response");

    assert_eq!(
        err.to_string(),
        "Partial body received 2 bytes and expected 10"
    );

    Ok(())
}

#[async_std::test]
async fn partial_response_chunked() -> Result<(), Error> {
    let conn = common::serve_once(|head, mut tcp| async move {
        assert_eq!(head, "GET /path HTTP/1.1\r\n\r\n");

        // NB: 1f in chunk size, write "nHELLO" then drop.
        let res = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1f\r\nHELLO";
        tcp.write_all(res).await.unwrap();

        Ok(tcp)
    })
    .await?;

    let req = http::Request::get("/path").body("").unwrap();

    let err = common::run(conn, req).await.expect_err("partial response");

    assert_eq!(err.to_string(), "Partial body");

    Ok(())
}

#[async_std::test]
async fn post_larger_than_clen() -> Result<(), Error> {
    let conn = common::serve_once(|_, tcp| async move { Ok(tcp) }).await?;

    let req = http::Request::post("/path")
        .header("content-length", 2)
        .body("HELLO")
        .unwrap();

    let err = common::run(conn, req).await.expect_err("partial response");

    assert_eq!(
        err.to_string(),
        "Body data longer than content-length header: 5 > 2"
    );

    Ok(())
}
