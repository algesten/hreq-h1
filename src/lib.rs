#![warn(clippy::all)]

#[macro_use]
extern crate log;

mod error;
mod limit;
mod share;

#[doc(hidden)]
pub mod http11;

#[doc(hidden)]
pub mod chunked;

pub(crate) use futures_io::{AsyncRead, AsyncWrite};

pub mod client;
pub mod server;

pub use error::Error;
pub use share::{RecvStream, SendStream};

pub(crate) fn err_closed<T>() -> Result<T, Error> {
    use std::io;
    Err(io::Error::new(io::ErrorKind::NotConnected, "Connection is closed").into())
}
