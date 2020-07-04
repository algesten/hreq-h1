#[macro_use]
extern crate log;

mod chunked;
mod error;
mod http11;
mod limit;
mod share;

pub(crate) use futures_io::{AsyncRead, AsyncWrite};

pub mod client;
pub mod server;

pub use error::Error;
pub use share::{RecvStream, SendStream};

pub(crate) fn err_closed<T>() -> Result<T, Error> {
    use std::io;
    Err(io::Error::new(io::ErrorKind::NotConnected, "Connection is closed").into())
}
