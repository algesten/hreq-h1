#![no_main]
use libfuzzer_sys::fuzz_target;

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_std::io::Cursor;
use futures_io::{AsyncRead, AsyncWrite};

#[derive(Clone, Debug)]
struct RwWrapper(Arc<Mutex<Cursor<Vec<u8>>>>);

impl RwWrapper {
    fn new(input: Vec<u8>) -> Self {
        Self(Arc::new(Mutex::new(Cursor::new(input))))
    }
}

impl AsyncRead for RwWrapper {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.0.lock().unwrap()).poll_read(cx, buf)
    }
}

impl AsyncWrite for RwWrapper {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 14 {
        return;
    }

    let mut req = http::Request::builder();

    req = if data[0] < 85 {
        req.method(http::Method::GET)
    } else if data[0] < 170 {
        req.method(http::Method::POST)
    } else {
        req.method(http::Method::HEAD)
    };

    req = if data[1] < 128 {
        req.version(http::Version::HTTP_10)
    } else {
        req.version(http::Version::HTTP_11)
    };

    req = if data[2] < 128 {
        req.header("transfer-encoding", "chunked")
    } else {
        let mut arr = [0_u8; 8];
        (&mut arr[..]).copy_from_slice(&data[3..(3 + 8)]);
        let size = usize::from_be_bytes(arr);
        req.header("content-length", size.to_string())
    };

    let mut arr = [0_u8; 2];
    arr[0] = data[12];
    arr[1] = data[13];
    let urllen = u16::from_be_bytes(arr) as usize;

    let rest = 14 + urllen;

    if data.len() < 14 + urllen {
        return;
    }

    let url = String::from_utf8_lossy(&data[14..(14 + urllen)]).to_string();
    // let uri = url.parse::<http::uri::Uri>();
    // if uri.is_err() {
    //     return;
    // }
    // req = req.uri(uri.unwrap());
    req = req.uri("http://foo/");

    let req = req.body(());

    if req.is_err() {
        return;
    }
    let req = req.unwrap();

    let mut header = vec![0_u8; 16_384];

    let amount = hreq_h1::http11::write_http1x_req(&req, &mut header[..]);
    if amount.is_err() {
        return;
    }
    let amount = amount.unwrap();

    unsafe { header.set_len(amount) };

    header.extend_from_slice(&data[rest..]);

    let stream = RwWrapper::new(header);

    async_std::task::block_on(async move {
        let mut conn = hreq_h1::server::handshake(stream);
        if let Some(req) = conn.accept().await {
            if let Ok((req, respond)) = req {
                let (parts, mut recv) = req.into_parts();
                let mut buf = vec![0_u8; 1024 * 1024];
                loop {
                    let amount = recv.read(&mut buf[..]).await;
                    if let Ok(amount) = amount {
                        if amount == 0 {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
    });
});
