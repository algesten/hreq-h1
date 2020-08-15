#![warn(missing_docs, missing_debug_implementations)]
#![warn(clippy::all)]

//! An asynchronous HTTP/1 server and client implemenation.
//!
//! This library provides the lower level parts of the HTTP/1.1 (and 1.0) spec.
//! Specifically it concerns sending/receiving http requests and bodies over
//! some unnamed transport. Which async runtime to use, TCP and TLS are handled
//! outside this library.
//!
//! Since HTTP/1.1 has no multiplexing, the HTTP headers `Content-Length` and
//! `Transfer-Encoding` are handled/enforced by this library to be able to correctly
//! delinate where a request starts/ends over the transport.
//!
//! ## In scope
//!
//! * `Content-Length` for known body sizes and ensuring the body size is correct.
//! * `Transfer-Encoding: chunked` when body size not known.
//! * `Connection: keep-alive` or `close` to handle HTTP/1.0 response delineation.
//!
//! ## Out of scope
//!
//! Basically everything which isn't about HTTP as "transport", i.e. application
//! level logic.
//!
//! * Following redirects
//! * Cookie handling
//! * `Content-Type` characters sets, mime types.
//! * `Content-Encoding` compression, gzip.
//! * `Expect: 100-Continue`
//!
//! # Layout and API
//!
//! The API tries to closely follow that of [h2] so that a user of the library can make
//! uniform handling of both HTTP/1.1 and HTTP/2.0.
//!
//! There are separate [client] and [server] modules and code that is shared between
//! them lives in the crate root.
//!
//! # Handshake
//!
//! Some connection must already have been established between client and server, this library
//! does not perform socket connection.
//!
//! In HTTP/1.1 there is no "handshake" like in HTTP/2.0, but
//! to be congruent with [h2], the entry points for starting a connection are [`client::handshake`]
//! and [`server::handshake`].
//!
//! [h2]: https://crates.io/crates/h2
//! [client]: client/index.html
//! [server]: server/index.html
//! [`client::handshake`]: client/fn.handshake.html
//! [`server::handshake`]: server/fn.handshake.html

#[macro_use]
extern crate tracing;

mod error;
mod limit;
mod share;
mod try_write;

#[doc(hidden)]
pub mod buf_reader;

#[doc(hidden)]
pub mod http11;

#[doc(hidden)]
pub mod chunked;

pub(crate) use futures_io::{AsyncBufRead, AsyncRead, AsyncWrite};

pub mod client;
pub mod server;

pub use error::Error;
pub use share::{RecvStream, SendStream};

pub(crate) fn err_closed<T>() -> Result<T, Error> {
    use std::io;
    Err(io::Error::new(io::ErrorKind::NotConnected, "Connection is closed").into())
}
