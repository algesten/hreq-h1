use futures_io::{AsyncRead, AsyncWrite};
use futures_util::future::poll_fn;
use hreq_h1::Error;
use std::io;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Once;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite};

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

pub fn from_tokio<Z>(adapted: Z) -> FromAdapter<Z>
where
    Z: TokioAsyncRead + TokioAsyncWrite + Unpin + Send + 'static,
{
    FromAdapter { adapted }
}

pub struct FromAdapter<Z> {
    adapted: Z,
}

impl<Z: TokioAsyncRead + Unpin> AsyncRead for FromAdapter<Z> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().adapted).poll_read(cx, buf)
    }
}

impl<Z: TokioAsyncWrite + Unpin> AsyncWrite for FromAdapter<Z> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().adapted).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().adapted).poll_flush(cx)
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().adapted).poll_shutdown(cx)
    }
}
