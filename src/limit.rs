use crate::chunked::{ChunkedDecoder, ChunkedEncoder};
use crate::fast_buf::FastBuf;
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
        is_server_response: bool,
    ) -> Self {
        // NB: We're not enforcing 1.0 compliance. it's possible to mix
        // transfer-encoding: chunked with HTTP/1.0, which should
        // not be a possible combo.

        // https://tools.ietf.org/html/rfc7230#page-31
        // If a message is received with both a Transfer-Encoding and a
        // Content-Length header field, the Transfer-Encoding overrides the
        // Content-Length.

        let ret = if is_chunked(headers) {
            LimitRead::ChunkedDecoder(ChunkedDecoder::new())
        } else if let Some(size) = get_as::<u64>(headers, "content-length") {
            LimitRead::ContentLength(ContentLengthRead::new(size))
        } else {
            if is_server_response {
                // https://tools.ietf.org/html/rfc1945#section-7.2.2
                // When an Entity-Body is included with a message, the length of that
                // body may be determined in one of two ways. If a Content-Length header
                // field is present, its value in bytes represents the length of the
                // Entity-Body. Otherwise, the body length is determined by the closing
                // of the connection by the server.

                // Closing the connection cannot be used to indicate the end of a
                // request body, since it leaves no possibility for the server to send
                // back a response.
                LimitRead::ReadToEnd(ReadToEnd::new())
            } else {
                // For request, with content-length, and not chunked, for 1.1 we don't expect a body.
                LimitRead::NoBody
            }
        };

        trace!("LimitRead from headers: {:?}", ret);

        ret
    }

    pub fn is_no_body(&self) -> bool {
        match &self {
            LimitRead::ContentLength(r) => r.limit == 0,
            LimitRead::NoBody => true,
            _ => false,
        }
    }

    pub fn is_complete(&self) -> bool {
        match &self {
            LimitRead::ChunkedDecoder(v) => v.is_end(),
            LimitRead::ContentLength(v) => v.is_end(),
            LimitRead::ReadToEnd(v) => v.is_end(),
            LimitRead::NoBody => true,
        }
    }

    pub fn body_size(&self) -> Option<u64> {
        if let LimitRead::ContentLength(v) = &self {
            return Some(v.limit);
        }
        None
    }

    pub fn is_reusable(&self) -> bool {
        self.is_complete() && !self.is_read_to_end()
    }

    fn is_read_to_end(&self) -> bool {
        if let LimitRead::ReadToEnd(_) = self {
            return true;
        }
        false
    }

    /// Tests if the encapsulated reader can accept an entire vec in one big read.
    pub fn can_read_entire_vec(&self) -> bool {
        if let LimitRead::ContentLength(_) = self {
            return true;
        }
        false
    }

    pub fn accept_entire_vec(&mut self, buf: &Vec<u8>) {
        if let LimitRead::ContentLength(v) = self {
            v.total += buf.len() as u64;
        } else {
            panic!("accept_entire_vec with wrong type of writer");
        }
    }

    /// Try read some data.
    pub fn poll_read<S: AsyncRead + Unpin>(
        &mut self,
        cx: &mut Context,
        recv: &mut BufIo<S>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match self {
            LimitRead::ChunkedDecoder(v) => v.poll_read(cx, recv, buf),
            LimitRead::ContentLength(v) => v.poll_read(cx, recv, buf),
            LimitRead::ReadToEnd(v) => v.poll_read(cx, recv, buf),
            LimitRead::NoBody => Ok(0).into(),
        }
    }
}

use crate::buf_reader::BufIo;

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
        recv: &mut BufIo<R>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        assert!(!buf.is_empty(), "poll_read with len 0 buf");

        let left = (self.limit - self.total).min(usize::max_value() as u64) as usize;

        if left == 0 {
            // Nothing more should be read.
            return Ok(0).into();
        }

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

pub(crate) struct ReadToEnd {
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
        recv: &mut BufIo<R>,
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
        let ret = if is_chunked(headers) {
            LimitWrite::ChunkedEncoder
        } else if let Some(limit) = get_as::<u64>(headers, "content-length") {
            LimitWrite::ContentLength(ContentLengthWrite::new(limit))
        } else {
            LimitWrite::NoBody
        };

        trace!("LimitWrite from headers: {:?}", ret);

        ret
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
        match self {
            LimitWrite::ContentLength(w) => w.limit == 0,
            LimitWrite::NoBody => true,
            _ => false,
        }
    }

    /// Tests if the encapsulated writer can accept an entire vec in one big write.
    pub fn can_write_entire_vec(&self) -> bool {
        if let LimitWrite::ContentLength(_) = self {
            return true;
        }
        false
    }

    pub fn accept_entire_vec(&mut self, buf: &Vec<u8>) {
        if let LimitWrite::ContentLength(v) = self {
            v.total += buf.len() as u64;
        } else {
            panic!("accept_entire_vec with wrong type of writer");
        }
    }

    /// Write some data using this limiter.
    pub fn write(&mut self, data: &[u8], out: &mut FastBuf) -> Result<(), Error> {
        match self {
            LimitWrite::ChunkedEncoder => ChunkedEncoder::write_chunk(data, out),
            LimitWrite::ContentLength(v) => v.write(data, out),
            LimitWrite::NoBody => Ok(()),
        }
    }

    /// Finish up writing, called once after the all `write()` calls are done.
    pub fn finish(&mut self, out: &mut FastBuf) -> Result<(), Error> {
        match self {
            LimitWrite::ChunkedEncoder => ChunkedEncoder::write_finish(out),
            LimitWrite::ContentLength(_) => Ok(()),
            LimitWrite::NoBody => Ok(()),
        }
    }
}

/// Limit write by length.
#[derive(Debug)]
pub(crate) struct ContentLengthWrite {
    limit: u64,
    total: u64,
}

impl ContentLengthWrite {
    fn new(limit: u64) -> Self {
        ContentLengthWrite { limit, total: 0 }
    }

    fn write(&mut self, data: &[u8], out: &mut FastBuf) -> Result<(), Error> {
        if data.is_empty() {
            return Ok(());
        }
        self.total += data.len() as u64;

        if self.total > self.limit {
            let m = format!(
                "Body data longer than content-length header: {} > {}",
                self.total, self.limit
            );
            return Err(Error::User(m));
        }

        let mut into = out.borrow();

        into.extend_from_slice(data);

        Ok(())
    }
}

impl fmt::Debug for LimitRead {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self {
            LimitRead::ChunkedDecoder(_) => write!(f, "ChunkedDecoder")?,
            LimitRead::ContentLength(l) => write!(f, "ContenLength({})", l.limit)?,
            LimitRead::ReadToEnd(_) => write!(f, "ReadToEnd")?,
            LimitRead::NoBody => write!(f, "NoBody")?,
        }
        Ok(())
    }
}

impl fmt::Debug for LimitWrite {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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
        // https://tools.ietf.org/html/rfc2616#section-4.4
        //
        // If a Transfer-Encoding header field (section 14.41) is present and
        // has any value other than "identity", then the transfer-length is
        // defined by use of the "chunked" transfer-coding
        .map(|h| !h.contains("identity"))
        .unwrap_or(false)
}

pub fn allow_reuse(headers: &http::HeaderMap<http::HeaderValue>, version: http::Version) -> bool {
    if version == http::Version::HTTP_11 {
        is_keep_alive(headers, true)
    } else {
        is_keep_alive(headers, false)
    }
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
