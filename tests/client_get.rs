use async_std::net::{TcpListener, TcpStream};
use futures_util::future::poll_fn;
use futures_util::io::AsyncReadExt;
use futures_util::io::AsyncWriteExt;
use hreq_h1::buf_reader::BufIo;
use hreq_h1::http11::poll_for_crlfcrlf;
use hreq_h1::Error;
use tracing::trace_span;
use tracing_futures::Instrument;

mod common;

#[async_std::test]
async fn client_get_200_ok() -> Result<(), Error> {
    common::setup_logger();

    let l = TcpListener::bind("127.0.0.1:0").await?;
    let c = TcpStream::connect(l.local_addr()?);

    let server = async move {
        let (s, _) = l.accept().await.unwrap();
        let mut s_buf = BufIo::with_capacity(16_384, s);

        let req_s = poll_fn(|cx| {
            poll_for_crlfcrlf(cx, &mut s_buf, |buf| {
                String::from_utf8_lossy(buf).to_string()
            })
        })
        .await
        .unwrap();

        assert_eq!(req_s, "GET / HTTP/1.1\r\n\r\n");

        s_buf.write_all(b"HTTP/1.1 200 OK\r\n\r\nOK").await.unwrap();
    }
    .instrument(trace_span!("server_task"));

    async_std::task::spawn(server);

    let c = c.await?;
    let (mut send_req, c_conn) = hreq_h1::client::handshake(c);

    // drive client
    async_std::task::spawn(async move { c_conn.await.ok() });

    let req = http::Request::get("/").body(())?;

    let (res_fut, _) = send_req.send_request(req, true)?;

    let res = res_fut.await?;

    let (_, mut recv_body) = res.into_parts();

    let mut body = String::new();
    recv_body.read_to_string(&mut body).await?;

    assert_eq!(body, "OK");

    Ok(())
}
