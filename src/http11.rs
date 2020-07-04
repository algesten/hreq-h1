use super::Error;
use std::io;
use std::io::Write;

// Request headers today vary in size from ~200 bytes to over 2KB.
// As applications use more cookies and user agents expand features,
// typical header sizes of 700-800 bytes is common.
// http://dev.chromium.org/spdy/spdy-whitepaper

/// Write an http/1.1 request to a buffer.
#[allow(clippy::write_with_newline)]
pub fn write_http11_req<X>(req: &http::Request<X>, buf: &mut [u8]) -> Result<usize, Error> {
    // Write http request into a buffer
    let mut w = io::Cursor::new(buf);

    // Path and query
    let pq = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    write!(w, "{} {} HTTP/1.1\r\n", req.method(), pq)?;

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
    if log_enabled!(log::Level::Debug) {
        debug!(
            "write_http11_req: {:?}",
            String::from_utf8_lossy(&buf[0..len])
        );
    }

    Ok(len)
}

/// Write an http/1.1 response to a buffer.
#[allow(clippy::write_with_newline)]
pub fn write_http11_res<X>(req: &http::Response<X>, buf: &mut [u8]) -> Result<usize, Error> {
    // Write http request into a buffer
    let mut w = io::Cursor::new(buf);

    write!(
        w,
        "HTTP/1.1 {} {}\r\n",
        req.status().as_u16(),
        req.status().canonical_reason().unwrap_or("Unknown")
    )?;

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
    if log_enabled!(log::Level::Debug) {
        debug!(
            "write_http11_res: {:?}",
            String::from_utf8_lossy(&buf[0..len])
        );
    }

    Ok(len)
}

/// Attempt to parse an http/1.1 response.
pub fn try_parse_res(buf: &[u8]) -> Result<Option<(http::Response<()>, usize)>, Error> {
    if log_enabled!(log::Level::Trace) {
        trace!("try_parse_res: {:?}", String::from_utf8_lossy(buf));
    }

    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut parser = httparse::Response::new(&mut headers);

    let status = parser.parse(&buf)?;
    if status.is_partial() {
        return Ok(None);
    }

    let mut bld = http::Response::builder();

    if let Some(code) = parser.code {
        bld = bld.status(code);
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

    let built = bld.body(())?;
    let len = status.unwrap();

    debug!("try_parse_http11 success: {:?}", built);

    Ok(Some((built, len)))
}

/// Attempt to parse an http/1.1 request.
pub fn try_parse_req(buf: &[u8]) -> Result<Option<(http::Request<()>, usize)>, Error> {
    if log_enabled!(log::Level::Trace) {
        trace!("try_parse_req: {:?}", String::from_utf8_lossy(buf));
    }

    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut parser = httparse::Request::new(&mut headers);

    let status = parser.parse(&buf)?;
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

    let built = bld.body(())?;
    let len = status.unwrap();

    debug!("try_parse_http11 success: {:?}", built);

    Ok(Some((built, len)))
}
