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
pub struct LimitRead {
    limiter: ReadLimiter,
    allow_reuse: bool,
}

enum ReadLimiter {
    /// Read from a chunked decoder. The decoder will know when there is no more
    /// data to be read.
    ChunkedDecoder(ChunkedDecoder),
    /// Body data is limited by a `content-length` header.
    ContentLength(ContentLengthRead),
    /// Read until the connection closes (HTTP/1.0).
    ReadToEnd(ReadToEnd),
    /// No expected body.
    NoBody,
}

impl LimitRead {
    /// Create an instance from request headers.
    ///
    /// 1. If header `transfer-encoding: chunked` use chunked decoder regardless of other headers.
    /// 2. If header `content-length: <number>` use a reader limited by length
    /// 3. Otherwise consider there being no body.
    pub fn from_headers(
        headers: &http::HeaderMap<http::HeaderValue>,
        version: http::Version,
        is_server_response: bool,
    ) -> Self {
        // NB: We're not enforcing 1.0 compliance. it's possible to mix
        // transfer-encoding: chunked with HTTP/1.0, which should
        // not be a possible combo.

        // https://tools.ietf.org/html/rfc7230#page-31
        // If a message is received with both a Transfer-Encoding and a
        // Content-Length header field, the Transfer-Encoding overrides the
        // Content-Length.

        let limiter = if is_chunked(headers) {
            ReadLimiter::ChunkedDecoder(ChunkedDecoder::new())
        } else if let Some(size) = get_as::<u64>(headers, "content-length") {
            ReadLimiter::ContentLength(ContentLengthRead::new(size))
        } else {
            if version == http::Version::HTTP_10 && is_server_response {
                // https://tools.ietf.org/html/rfc1945#section-7.2.2
                // When an Entity-Body is included with a message, the length of that
                // body may be determined in one of two ways. If a Content-Length header
                // field is present, its value in bytes represents the length of the
                // Entity-Body. Otherwise, the body length is determined by the closing
                // of the connection by the server.

                // Closing the connection cannot be used to indicate the end of a
                // request body, since it leaves no possibility for the server to send
                // back a response.
                ReadLimiter::ReadToEnd(ReadToEnd::new())
            } else {
                // no content-length, and no chunked, for 1.1 we don't expect a body.
                ReadLimiter::NoBody
            }
        };

        let allow_reuse = if version == http::Version::HTTP_11 {
            is_keep_alive(headers, true)
        } else {
            is_keep_alive(headers, false)
        };

        LimitRead {
            limiter,
            allow_reuse,
        }
    }

    pub fn is_no_body(&self) -> bool {
        if let ReadLimiter::NoBody = &self.limiter {
            return true;
        }
        false
    }

    pub fn is_complete(&self) -> bool {
        match &self.limiter {
            ReadLimiter::ChunkedDecoder(v) => v.is_end(),
            ReadLimiter::ContentLength(v) => v.is_end(),
            ReadLimiter::ReadToEnd(v) => v.is_end(),
            ReadLimiter::NoBody => true,
        }
    }

    pub fn is_reusable(&self) -> bool {
        self.allow_reuse && self.is_complete() && !self.is_read_to_end()
    }

    fn is_read_to_end(&self) -> bool {
        if let ReadLimiter::ReadToEnd(_) = self.limiter {
            return true;
        }
        false
    }

    /// Try read some data.
    pub fn poll_read<R: AsyncRead + Unpin>(
        &mut self,
        cx: &mut Context,
        recv: &mut R,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.limiter {
            ReadLimiter::ChunkedDecoder(v) => v.poll_read(cx, recv, buf),
            ReadLimiter::ContentLength(v) => v.poll_read(cx, recv, buf),
            ReadLimiter::ReadToEnd(v) => v.poll_read(cx, recv, buf),
            ReadLimiter::NoBody => Ok(0).into(),
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
            // https://tools.ietf.org/html/rfc7230#page-32
            // If a valid Content-Length header field is present without
            // Transfer-Encoding, its decimal value defines the expected message
            // body length in octets.  If the sender closes the connection or
            // the recipient times out before the indicated number of octets are
            // received, the recipient MUST consider the message to be
            // incomplete and close the connection.
            let msg = format!(
                "Partial body received {} bytes and expected {}",
                self.total, self.limit
            );
            trace!("{}", msg);
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, msg)).into();
        }
        self.total += amount as u64;

        Ok(amount).into()
    }
}

struct ReadToEnd {
    reached_end: bool,
}

impl ReadToEnd {
    fn new() -> Self {
        ReadToEnd { reached_end: false }
    }

    fn is_end(&self) -> bool {
        self.reached_end
    }

    fn poll_read<R: AsyncRead + Unpin>(
        &mut self,
        cx: &mut Context,
        recv: &mut R,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        assert!(!buf.is_empty(), "poll_read with len 0 buf");

        let amount = ready!(Pin::new(&mut *recv).poll_read(cx, buf))?;

        if amount == 0 {
            self.reached_end = true;
        }

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
            return Err(Error::User(m));
        }
        let cur_len = out.len();
        out.resize(cur_len + data.len(), 0);
        (&mut out[cur_len..]).copy_from_slice(data);
        Ok(())
    }
}

impl fmt::Debug for LimitRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.limiter {
            ReadLimiter::ChunkedDecoder(_) => write!(f, "ChunkedDecoder")?,
            ReadLimiter::ContentLength(l) => write!(f, "ContenLength({})", l.limit)?,
            ReadLimiter::ReadToEnd(_) => write!(f, "ReadToEnd")?,
            ReadLimiter::NoBody => write!(f, "NoBody")?,
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

fn is_keep_alive(headers: &http::HeaderMap<http::HeaderValue>, default: bool) -> bool {
    headers
        .get("connection")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| {
            if h == "keep-alive" {
                Some(true)
            } else if h == "close" {
                Some(false)
            } else {
                None
            }
        })
        .unwrap_or(default)
}

fn get_str<'a>(headers: &'a http::HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key).and_then(|v| v.to_str().ok())
}

fn get_as<T: FromStr>(headers: &http::HeaderMap, key: &str) -> Option<T> {
    get_str(headers, key).and_then(|v| v.parse().ok())
}
