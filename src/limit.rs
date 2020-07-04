use super::chunked::{ChunkedDecoder, ChunkedEncoder};
use super::AsyncRead;
use super::Error;
use futures_util::ready;
use std::fmt;
use std::io;
use std::pin::Pin;
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
        let transfer_enc_chunk = headers
            .get("transfer-encoding")
            .map(|h| h == "chunked")
            .unwrap_or(false);

        let content_length = get_as::<u64>(headers, "content-length");

        if transfer_enc_chunk {
            LimitRead::ChunkedDecoder(ChunkedDecoder::new())
        } else if let Some(size) = content_length {
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
    reached_end: bool,
}

impl ContentLengthRead {
    fn new(limit: u64) -> Self {
        ContentLengthRead {
            limit,
            total: 0,
            reached_end: false,
        }
    }
    fn poll_read<R: AsyncRead + Unpin>(
        &mut self,
        cx: &mut Context,
        recv: &mut R,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let left = (self.limit - self.total).min(usize::max_value() as u64) as usize;

        if left == 0 {
            // we need to put the underlying connection in the right
            // state to receive another request.
            if !self.reached_end {
                let mut end = [];
                ready!(Pin::new(&mut *recv).poll_read(cx, &mut end[..]))?;
                self.reached_end = true;
            }
            return Ok(0).into();
        }

        let max = buf.len().min(left);
        let amount = ready!(Pin::new(&mut *recv).poll_read(cx, &mut buf[0..max]))?;

        if amount == 0 {
            self.reached_end = true;
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
    /// 3. Otherwise use chunked (will do nothing unless written to).
    pub fn from_headers(headers: &http::HeaderMap<http::HeaderValue>) -> Self {
        let transfer_enc_chunk = headers
            .get("transfer-encoding")
            .map(|h| h == "chunked")
            .unwrap_or(false);

        let content_length = get_as::<u64>(headers, "content-length");

        // also use chunked when we don't know.
        let use_chunked = transfer_enc_chunk || content_length.is_none();

        if use_chunked {
            if content_length.is_some() {
                // this is technically an error
                warn!("Ignoring content-length in favor of transfer-encoding: chunked");
            }
            LimitWrite::ChunkedEncoder
        } else if let Some(limit) = content_length {
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

use std::str::FromStr;

fn get_str<'a>(headers: &'a http::HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key).and_then(|v| v.to_str().ok())
}

fn get_as<T: FromStr>(headers: &http::HeaderMap, key: &str) -> Option<T> {
    get_str(headers, key).and_then(|v| v.parse().ok())
}
