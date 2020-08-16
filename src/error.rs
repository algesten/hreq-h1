use std::fmt;
use std::io;

/// Possible errors from this crate.
#[derive(Debug)]
pub enum Error {
    /// A user/usage problem such as sending more bytes than a content-length header specifies.
    User(String),
    /// A wrapped std::io::Error from the underlying transport (socket).
    Io(io::Error),
    /// HTTP/1.1 parse errors from the `httparse` crate.
    Http11Parser(httparse::Error),
    /// Http errors from the `http` crate.
    Http(http::Error),
}

// impl Error {
//     pub(crate) fn into_io(self) -> io::Error {
//         match self {
//             Error::Io(i) => i,
//             Error::User(e) => io::Error::new(io::ErrorKind::Other, e),
//             Error::Http11Parser(e) => io::Error::new(io::ErrorKind::Other, e),
//             Error::Http(e) => io::Error::new(io::ErrorKind::Other, e),
//         }
//     }
// }

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::User(v) => write!(f, "{}", v),
            Error::Io(v) => fmt::Display::fmt(v, f),
            Error::Http11Parser(v) => write!(f, "http11 parser: {}", v),
            Error::Http(v) => write!(f, "http api: {}", v),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<httparse::Error> for Error {
    fn from(e: httparse::Error) -> Self {
        Error::Http11Parser(e)
    }
}

impl From<http::Error> for Error {
    fn from(e: http::Error) -> Self {
        Error::Http(e)
    }
}
