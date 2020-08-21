use async_std::net::{TcpListener, TcpStream};
use tracing_futures::Instrument;

mod common;

#[async_std::test]
async fn throughput_server_to_client() -> Result<(), hreq_h1::Error> {
    common::setup_logger();

    let l = TcpListener::bind("127.0.0.1:0").await?;
    let c = TcpStream::connect(l.local_addr()?);

    const PER_CHUNK: usize = 100 * 1024 * 1024; // 100 MB
    const CHUNKS: usize = 10 * 10; // 10GB in total

    let span = tracing::info_span!("server_task");

    async_std::task::spawn(
        async move {
            let (s, _) = l.accept().await.unwrap();
            let mut s_conn = hreq_h1::server::handshake(s);

            let (_, send_res) = s_conn.accept().await.unwrap().unwrap();

            let res = http::Response::builder()
                .header("content-length", (PER_CHUNK * CHUNKS).to_string())
                .body(())
                .unwrap();

            let mut send_body = send_res.send_response(res, false).await.unwrap();

            for _ in 0..CHUNKS {
                let chunk = vec![42_u8; PER_CHUNK];

                send_body.send_data_owned(chunk, false).await.unwrap();
            }
            send_body.send_data(&[], true).await.unwrap();
        }
        .instrument(span),
    );

    let c = c.await?;
    let (mut send_req, c_conn) = hreq_h1::client::handshake(c);

    // drive client
    async_std::task::spawn(async move { c_conn.await.ok() });

    let req = http::Request::get("/").body(()).unwrap();

    let (fut, _) = send_req.send_request(req, true)?;

    let res = fut.await?;

    let (_, mut recv_body) = res.into_parts();

    let mut into = vec![0_u8; PER_CHUNK];
    let mut total = 0;

    while total < PER_CHUNK * CHUNKS {
        let mut pos = 0;
        loop {
            let amt = recv_body.read(&mut into[pos..]).await?;
            pos += amt;
            total += amt;
            println!(
                "{:.2} add {}",
                (total as f64 / (PER_CHUNK * CHUNKS) as f64),
                amt
            );
            if pos == into.len() {
                pos = 0;
            }
            if amt == 0 {
                break;
            }
        }
    }

    Ok(())
}
