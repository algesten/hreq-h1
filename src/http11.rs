use crate::AsyncRead;
use crate::Error;
use futures_util::ready;
use http::header::{HeaderName, HeaderValue};
use std::io;
use std::io::Write;
use std::pin::Pin;
use std::task::{Context, Poll};

// Request headers today vary in size from ~200 bytes to over 2KB.
// As applications use more cookies and user agents expand features,
// typical header sizes of 700-800 bytes is common.
// http://dev.chromium.org/spdy/spdy-whitepaper

/// Write an http/1.1 request to a buffer.
#[allow(clippy::write_with_newline)]
pub fn write_http1x_req(req: &http::Request<()>, buf: &mut [u8]) -> Result<usize, io::Error> {
    // Write http request into a buffer
    let mut w = io::Cursor::new(buf);

    // Path and query
    let pq = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let ver = match req.version() {
        http::Version::HTTP_10 => "1.0",
        http::Version::HTTP_11 => "1.1",
        _ => panic!("Unsupported http version: {:?}", req.version()),
    };

    write!(w, "{} {} HTTP/{}\r\n", req.method(), pq, ver)?;

    let mut host = None;
    for (name, value) in req.headers() {
        if name.as_str() == "host" {
            host = Some(value);
        }
    }
    if host.is_none() {
        let default_port: u16 = match req.uri().scheme_str() {
            Some("https") => 443,
            Some("http") => 80,
            _ => 0,
        };
        let port = match req.uri().port_u16() {
            Some(p) => {
                if p == default_port {
                    0
                } else {
                    p
                }
            }
            _ => 0,
        };
        // fall back on uri host
        if let Some(h) = req.uri().host() {
            write!(w, "host: {}", h)?;
            if port != 0 {
                write!(w, ":{}", port)?;
            }
            write!(w, "\r\n")?;
        }
    }

    // the rest of the headers.
    for (name, value) in req.headers() {
        write!(w, "{}: ", name)?;
        w.write_all(value.as_bytes())?;
        write!(w, "\r\n")?;
    }
    write!(w, "\r\n")?;

    let len = w.position() as usize;

    // write buffer to connection
    let buf = w.into_inner();
    debug!(
        "write_http11_req: {:?}",
        String::from_utf8_lossy(&buf[0..len])
    );

    Ok(len)
}

/// Write an http/1.x response to a buffer.
#[allow(clippy::write_with_newline)]
pub fn write_http1x_res(res: &http::Response<()>, buf: &mut [u8]) -> Result<usize, Error> {
    // Write http request into a buffer
    let mut w = io::Cursor::new(buf);

    let ver = match res.version() {
        http::Version::HTTP_10 => "1.0",
        http::Version::HTTP_11 => "1.1",
        _ => panic!("Unsupported http version: {:?}", res.version()),
    };

    write!(
        w,
        "HTTP/{} {} {}\r\n",
        ver,
        res.status().as_u16(),
        res.status().canonical_reason().unwrap_or("Unknown")
    )?;

    // the rest of the headers.
    for (name, value) in res.headers() {
        write!(w, "{}: ", name)?;
        w.write_all(value.as_bytes())?;
        write!(w, "\r\n")?;
    }
    write!(w, "\r\n")?;

    let len = w.position() as usize;

    // write buffer to connection
    let buf = w.into_inner();
    debug!(
        "write_http11_res: {:?}",
        String::from_utf8_lossy(&buf[0..len])
    );

    Ok(len)
}

fn version_of(v: Option<u8>) -> http::Version {
    match v {
        Some(0) => http::Version::HTTP_10,
        Some(1) => http::Version::HTTP_11,
        _ => panic!("Unhandled http version: {:?}", v),
    }
}

/// Attempt to parse an http/1.1 response.
pub fn try_parse_res(buf: &[u8]) -> Result<Option<(http::Response<()>, usize)>, io::Error> {
    trace!("try_parse_res: {:?}", String::from_utf8_lossy(buf));

    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut parser = httparse::Response::new(&mut headers);

    let status = parser
        .parse(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if status.is_partial() {
        return Ok(None);
    }

    let mut bld = http::Response::builder().version(version_of(parser.version));

    if let Some(code) = parser.code {
        bld = bld.status(code);
    }

    for head in parser.headers.iter() {
        let name = HeaderName::from_bytes(head.name.as_bytes());
        let value = HeaderValue::from_bytes(head.value);
        match (name, value) {
            (Ok(name), Ok(value)) => bld = bld.header(name, value),
            (Err(e), _) => {
                debug!("Dropping bad header name: {}", e);
            }
            (Ok(name), Err(e)) => {
                debug!("Dropping bad header value ({}): {}", name, e);
            }
        }
    }

    let built = bld
        .body(())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let len = status.unwrap();

    debug!("try_parse_http11 success: {:?}", built);

    Ok(Some((built, len)))
}

/// Attempt to parse an http/1.1 request.
pub fn try_parse_req(buf: &[u8]) -> Result<Option<(http::Request<()>, usize)>, io::Error> {
    trace!("try_parse_req: {:?}", String::from_utf8_lossy(buf));

    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut parser = httparse::Request::new(&mut headers);

    let status = parser
        .parse(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if status.is_partial() {
        return Ok(None);
    }

    let mut uri = http::Uri::builder();

    if let Some(path) = parser.path {
        uri = uri.path_and_query(path);
    }

    let mut bld = http::Request::builder().version(if parser.version == Some(1) {
        http::Version::HTTP_11
    } else {
        http::Version::HTTP_10
    });

    bld = bld.uri(uri.build().unwrap());

    if let Some(method) = parser.method {
        bld = bld.method(method);
    }

    for head in parser.headers.iter() {
        let name = http::header::HeaderName::from_bytes(head.name.as_bytes());
        let value = http::header::HeaderValue::from_bytes(head.value);
        match (name, value) {
            (Ok(name), Ok(value)) => bld = bld.header(name, value),
            (Err(e), _) => {
                debug!("Dropping bad header name: {}", e);
            }
            (Ok(name), Err(e)) => {
                debug!("Dropping bad header value ({}): {}", name, e);
            }
        }
    }

    let built = bld
        .body(())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let len = status.unwrap();

    debug!("try_parse_http11 success: {:?}", built);

    Ok(Some((built, len)))
}

/// Helper to poll for request or response.
///
/// It looks out for \r\n\r\n, which indicates the end of the headers and body begins.
pub fn poll_for_crlfcrlf<S>(
    cx: &mut Context<'_>,
    buf: &mut Vec<u8>,
    io: &mut S,
) -> Poll<io::Result<()>>
where
    S: AsyncRead + Unpin,
{
    const END_OF_HEADER: &[u8] = &[b'\r', b'\n', b'\r', b'\n'];
    let mut end_index = 0;
    let mut buf_index = 0;
    let mut one = [0_u8; 1];

    // fix so end_index is where it needs to be
    loop {
        if buf_index == buf.len() {
            break;
        }
        if buf[buf_index] == END_OF_HEADER[end_index] {
            end_index += 1;
        } else if end_index > 0 {
            end_index = 0;
        }
        buf_index += 1;
    }

    loop {
        if buf_index == buf.len() {
            // read one more char
            let amount = ready!(Pin::new(&mut &mut *io).poll_read(cx, &mut one[..]))?;
            if amount == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF before complete http11 header",
                ))
                .into();
            }
            buf.push(one[0]);
        }

        if buf[buf_index] == END_OF_HEADER[end_index] {
            end_index += 1;
        } else if end_index > 0 {
            end_index = 0;
        }

        if end_index == END_OF_HEADER.len() {
            // we found the end of header sequence
            break;
        }
        buf_index += 1;
    }

    Ok(()).into()
}
