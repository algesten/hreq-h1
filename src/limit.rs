use crate::chunked::{ChunkedDecoder, ChunkedEncoder};
use crate::AsyncRead;
use crate::Error;
use futures_util::ready;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};

/// Limit reading data given configuration from request headers.
pub(crate) enum LimitRead {
    /// Read from a chunked decoder. The decoder will know when there is no more
    /// data to be read.
    ChunkedDecoder(ChunkedDecoder),
    /// Body data is limited by a `content-length` header.
    ContenLength(ContentLengthRead),
    /// No expected body.
    NoBody,
}

impl LimitRead {
    /// Create an instance from request headers.
    ///
    /// 1. If header `transfer-encoding: chunked` use chunked decoder regardless of other headers.
    /// 2. If header `content-length: <number>` use a reader limited by length
    /// 3. Otherwise consider there being no body.
    pub fn from_headers(headers: &http::HeaderMap<http::HeaderValue>) -> Self {
        // https://tools.ietf.org/html/rfc7230#page-31
        // If a message is received with both a Transfer-Encoding and a
        // Content-Length header field, the Transfer-Encoding overrides the
        // Content-Length.
        if is_chunked(headers) {
            LimitRead::ChunkedDecoder(ChunkedDecoder::new())
        } else if let Some(size) = get_as::<u64>(headers, "content-length") {
            LimitRead::ContenLength(ContentLengthRead::new(size))
        } else {
            LimitRead::NoBody
        }
    }

    pub fn is_no_body(&self) -> bool {
        if let LimitRead::NoBody = self {
            return true;
        }
        false
    }

    pub fn is_complete(&self) -> bool {
        match self {
            LimitRead::ChunkedDecoder(v) => v.is_end(),
            LimitRead::ContenLength(v) => v.is_end(),
            LimitRead::NoBody => true,
        }
    }

    /// Try read some data.
    pub fn poll_read<R: AsyncRead + Unpin>(
        &mut self,
        cx: &mut Context,
        recv: &mut R,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match self {
            LimitRead::ChunkedDecoder(v) => v.poll_read(cx, recv, buf),
            LimitRead::ContenLength(v) => v.poll_read(cx, recv, buf),
            LimitRead::NoBody => Ok(0).into(),
        }
    }
}

/// Reader limited by a set length.
#[derive(Debug)]
pub struct ContentLengthRead {
    limit: u64,
    total: u64,
}

impl ContentLengthRead {
    fn new(limit: u64) -> Self {
        ContentLengthRead { limit, total: 0 }
    }

    fn is_end(&self) -> bool {
        self.total == self.limit
    }

    fn poll_read<R: AsyncRead + Unpin>(
        &mut self,
        cx: &mut Context,
        recv: &mut R,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        assert!(!buf.is_empty(), "poll_read with len 0 buf");

        let left = (self.limit - self.total).min(usize::max_value() as u64) as usize;

        let max = buf.len().min(left);
        let amount = ready!(Pin::new(&mut *recv).poll_read(cx, &mut buf[0..max]))?;

        if left > 0 && amount == 0 {
            // https://tools.ietf.org/html/rfc7230#page-33
            // A client that receives an incomplete response message, which can
            // occur when a connection is closed prematurely or when decoding a
            // supposedly chunked transfer coding fails, MUST record the message as
            // incomplete.
            let msg = format!("Partial body {}/{}", self.total, self.limit);
            trace!("{}", msg);
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, msg)).into();
        }
        self.total += amount as u64;

        Ok(amount).into()
    }
}

/// Limit writing data by a strategy configured by request headers.
///
/// This is to ensure we don't write more data than "promised" by request/response
/// header configuration.
pub(crate) enum LimitWrite {
    /// Write data using a chunked encoder.
    ChunkedEncoder,
    /// Limit the write by the `content-length` header.
    ContentLength(ContentLengthWrite),
    /// There should be no body.
    NoBody,
}

impl LimitWrite {
    /// Create an instance from request headers.
    ///
    /// NB. This allows breaking the spec by allowing both `content-length` and `chunked` at the
    /// same time. It will warn, but not disallow.
    ///
    /// 1. If header `transfer-encoding: chunked` use chunked encoder regardless of other headers.
    /// 2. If header `content-length: <number>` use a reader limited by length
    /// 3. Otherwise expect no body.
    pub fn from_headers(headers: &http::HeaderMap<http::HeaderValue>) -> Self {
        // https://tools.ietf.org/html/rfc7230#page-31
        // If a message is received with both a Transfer-Encoding and a
        // Content-Length header field, the Transfer-Encoding overrides the
        // Content-Length.
        if is_chunked(headers) {
            LimitWrite::ChunkedEncoder
        } else if let Some(limit) = get_as::<u64>(headers, "content-length") {
            LimitWrite::ContentLength(ContentLengthWrite::new(limit))
        } else {
            LimitWrite::NoBody
        }
    }

    /// Extra overhead bytes per send_data() call.
    pub fn overhead(&self) -> usize {
        match self {
            LimitWrite::ChunkedEncoder => 32,
            LimitWrite::ContentLength(_) => 0,
            LimitWrite::NoBody => 0,
        }
    }

    pub fn is_no_body(&self) -> bool {
        if let LimitWrite::NoBody = self {
            return true;
        }
        false
    }

    /// Write some data using this limiter.
    pub fn write(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            LimitWrite::ChunkedEncoder => ChunkedEncoder::write_chunk(data, out),
            LimitWrite::ContentLength(v) => v.write(data, out),
            LimitWrite::NoBody => Ok(()),
        }
    }

    /// Finish up writing, called once after the all `write()` calls are done.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            LimitWrite::ChunkedEncoder => ChunkedEncoder::write_finish(out),
            LimitWrite::ContentLength(_) => Ok(()),
            LimitWrite::NoBody => Ok(()),
        }
    }
}

/// Limit write by length.
#[derive(Debug)]
pub struct ContentLengthWrite {
    limit: u64,
    total: u64,
}

impl ContentLengthWrite {
    fn new(limit: u64) -> Self {
        ContentLengthWrite { limit, total: 0 }
    }

    fn write(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        self.total += data.len() as u64;
        if self.total > self.limit {
            let m = format!(
                "Body data longer than content-length header: {} > {}",
                self.total, self.limit
            );
            return Err(Error::Proto(m));
        }
        let cur_len = out.len();
        out.resize(cur_len + data.len(), 0);
        (&mut out[cur_len..]).copy_from_slice(data);
        Ok(())
    }
}

impl fmt::Debug for LimitRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitRead::ChunkedDecoder(_) => write!(f, "ChunkedDecoder")?,
            LimitRead::ContenLength(l) => write!(f, "ContenLength({})", l.limit)?,
            LimitRead::NoBody => write!(f, "NoBody")?,
        }
        Ok(())
    }
}

impl fmt::Debug for LimitWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitWrite::ChunkedEncoder => write!(f, "ChunkedEncoder")?,
            LimitWrite::ContentLength(l) => write!(f, "ContentLength({})", l.limit)?,
            LimitWrite::NoBody => write!(f, "NoBody")?,
        }
        Ok(())
    }
}

fn is_chunked(headers: &http::HeaderMap<http::HeaderValue>) -> bool {
    headers
        .get("transfer-encoding")
        .and_then(|h| h.to_str().ok())
        .map(|h| h.contains("chunked"))
        .unwrap_or(false)
}

fn get_str<'a>(headers: &'a http::HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key).and_then(|v| v.to_str().ok())
}

fn get_as<T: FromStr>(headers: &http::HeaderMap, key: &str) -> Option<T> {
    get_str(headers, key).and_then(|v| v.parse().ok())
}
