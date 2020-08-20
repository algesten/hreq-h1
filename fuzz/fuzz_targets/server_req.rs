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
    let stream = RwWrapper::new(data.to_vec());

    async_std::task::block_on(async move {
        let mut conn = hreq_h1::server::handshake(stream);
        if let Some(req) = conn.accept().await {
            if let Ok((req, respond)) = req {
                //
            }
        }
    });
});
