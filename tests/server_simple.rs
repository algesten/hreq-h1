use futures_util::{AsyncReadExt, AsyncWriteExt};
use hreq_h1::Error;

mod common;
use common::HeaderMapExt;

use async_std::net::{TcpListener, TcpStream};

use common::Connector;
use hreq_h1::server::SendResponse;
use std::future::Future;
use std::io;

pub async fn run_server<F, R>(mut f: F) -> Result<Connector, io::Error>
where
    F: Send + 'static,
    F: FnMut(http::request::Parts, io::Result<Vec<u8>>, SendResponse, usize) -> R,
    R: Future<Output = Result<bool, Error>>,
    R: Send,
{
    common::setup_logger();

    let l = TcpListener::bind("127.0.0.1:0").await?;
    let p = l.local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{}", p);

    async_std::task::spawn(async move {
        let mut call_count = 1;
        let mut keep_going = true;

        loop {
            if !keep_going {
                break;
            }
            let (tcp, _) = l.accept().await.expect("Accept incoming");

            let mut conn = hreq_h1::server::handshake(tcp);

            for x in conn.accept().await {
                let (req, respond) = x.expect("Handshaken");

                let (parts, mut body) = req.into_parts();

                let mut v = vec![];
                let bod_res = body.read_to_end(&mut v).await.map(|_| v);

                let again = f(parts, bod_res, respond, call_count)
                    .await
                    .expect("Handler");

                call_count += 1;

                keep_going = again;

                if !again {
                    break;
                }
            }
        }
    });

    Ok(Connector(addr))
}

#[async_std::test]
async fn server_request_200_ok() -> Result<(), Error> {
    let conn = run_server(|parts, body, respond, _| async move {
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

    let mut buf = [0; 2];

    tcp.read_exact(&mut buf).await?;

    Ok(())
}
