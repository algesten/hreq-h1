#![allow(dead_code)]

use async_std::net::{TcpListener, TcpStream};
use futures_io::AsyncRead;
use futures_util::future::poll_fn;
use futures_util::{AsyncReadExt, AsyncWriteExt};
use hreq_h1::Error;
use std::future::Future;
use std::io;
use std::str::FromStr;
use std::sync::Once;

pub async fn serve<F, R>(mut f: F) -> Result<Connector, io::Error>
where
    F: Send + 'static,
    F: FnMut(String, TcpStream, usize) -> R,
    R: Future<Output = Result<(TcpStream, bool), Error>>,
    R: Send,
{
    setup_logger();

    let l = TcpListener::bind("127.0.0.1:0").await?;
    let p = l.local_addr()?.port();

    async_std::task::spawn(async move {
        let mut call_count = 1;
        let (mut tcp, _) = l.accept().await.expect("Accept failed");

        loop {
            let head = match read_header(&mut tcp).await {
                Ok(v) => v,
                Err(_e) => {
                    // client closed the connection
                    return;
                }
            };

            let fut = f(head, tcp, call_count);

            let (tcp_ret, again) = fut.await.expect("Handler fail");
            tcp = tcp_ret;

            tcp.flush().await.expect("Flush fail");

            call_count += 1;

            if !again {
                break;
            }
        }
    });

    let addr = format!("127.0.0.1:{}", p);

    Ok(Connector(addr))
}

pub struct Connector(String);

impl Connector {
    pub async fn connect(&self) -> Result<TcpStream, Error> {
        Ok(TcpStream::connect(&self.0).await?)
    }
}

pub async fn run<B: AsRef<[u8]>>(
    tcp: TcpStream,
    req: http::Request<B>,
) -> Result<(http::response::Parts, Vec<u8>), Error> {
    let (mut send, conn) = hreq_h1::client::handshake(tcp);

    async_std::task::spawn(async move {
        if let Err(_e) = conn.await {
            // println!("{:?}", _e);
        }
    });

    let (parts, body) = req.into_parts();
    let req = http::Request::from_parts(parts, ());
    let body: &[u8] = body.as_ref();

    let (fut, mut body_send) = send.send_request(req, body.is_empty())?;

    if !body.is_empty() {
        // send body in reasonable chunks
        const MAX: usize = 11_111; // odd number just to force weird offsets

        let mut i = 0;

        while i < body.len() {
            let max = (body.len() - i).min(MAX);
            body_send = body_send.ready().await?;
            body_send.send_data(&body[i..(i + max)], false).await?;
            i += max;
        }

        body_send.send_data(&[], true).await?;
    }

    let res = fut.await?;

    let (parts, mut body) = res.into_parts();

    let mut v = vec![];
    body.read_to_end(&mut v).await?;

    Ok((parts, v))
}

pub async fn read_header<S: AsyncRead + Unpin>(io: &mut S) -> Result<String, Error> {
    let mut buf = vec![];
    poll_fn(|cx| hreq_h1::http11::poll_for_crlfcrlf(cx, &mut buf, io)).await?;
    Ok(String::from_utf8(buf).unwrap())
}

pub fn setup_logger() {
    static START: Once = Once::new();
    START.call_once(|| {
        let test_log = std::env::var("TEST_LOG")
            .map(|x| x != "0" && x.to_lowercase() != "false")
            .unwrap_or(false);
        let level = if test_log {
            log::LevelFilter::Trace
        } else {
            log::LevelFilter::Info
        };
        pretty_env_logger::formatted_builder()
            .filter_level(log::LevelFilter::Warn)
            .filter_module("hreq_h1", level)
            .target(env_logger::Target::Stdout)
            .init();
    });
}

/// Internal extension of `HeaderMap`.
pub(crate) trait HeaderMapExt {
    /// Get a header, ignore incorrect header values.
    fn get_str(&self, key: &str) -> Option<&str>;

    fn get_as<T: FromStr>(&self, key: &str) -> Option<T>;

    fn set<T: Into<String>>(&mut self, key: &'static str, key: T);
}

impl HeaderMapExt for http::HeaderMap {
    //
    fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|v| v.to_str().ok())
    }

    fn get_as<T: FromStr>(&self, key: &str) -> Option<T> {
        self.get_str(key).and_then(|v| v.parse().ok())
    }

    fn set<T: Into<String>>(&mut self, key: &'static str, value: T) {
        let s: String = value.into();
        let header = s.parse().unwrap();

        self.insert(key, header);
    }
}
