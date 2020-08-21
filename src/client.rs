//! Client implementation of the HTTP/1.1 protocol.
//!
//! The client connection is split into two parts, one [`Connection`], which
//! encapsulates the actual transport, and a [`SendRequest`] which is used
//! to send (multiple) requests over the connection.
//!
//! # Example
//!
//! ```rust, no_run
//! use hreq_h1::client;
//! use std::error::Error;
//! use async_std::net::TcpStream;
//! use http::Request;
//!
//! #[async_std::main]
//! async fn main() -> Result<(), Box<dyn Error>> {
//!   // Establish TCP connection to the server.
//!   let tcp = TcpStream::connect("127.0.0.1:5928").await?;
//!
//!   // h1 is the API handle to send requests
//!   let (mut h1, connection) = client::handshake(tcp);
//!
//!   // Drive the connection independently of the API handle
//!   async_std::task::spawn(async move {
//!     if let Err(e) = connection.await {
//!       println!("Connection closed: {:?}", e);
//!     }
//!   });
//!
//!   // POST request to. Note that body is sent below.
//!   let req = Request::post("http://myspecial.server/recv")
//!     .body(())?;
//!
//!   let (res, mut send_body) = h1.send_request(req, false)?;
//!
//!   send_body.send_data(b"This is the request body data", true).await?;
//!
//!   let (head, mut body) = res.await?.into_parts();
//!
//!   println!("Received response: {:?}", head);
//!
//!   // Read response body into this buffer.
//!   let mut buf = [0_u8; 1024];
//!   loop {
//!      let amount = body.read(&mut buf).await?;
//!
//!      println!("RX: {:?}", &buf[0..amount]);
//!
//!      if amount == 0 {
//!        break;
//!      }
//!   }
//!
//!   Ok(())
//! }
//! ```
//!
//! [`Connection`]: struct.Connection.html
//! [`SendRequest`]: struct.SendRequest.html

use crate::buf_reader::BufIo;
use crate::err_closed;
use crate::fast_buf::FastBuf;
use crate::http11::{poll_for_crlfcrlf, try_parse_res, write_http1x_req, READ_BUF_INIT_SIZE};
use crate::limit::allow_reuse;
use crate::limit::{LimitRead, LimitWrite};
use crate::mpsc::{Receiver, Sender};
use crate::Error;
use crate::{AsyncRead, AsyncWrite};
use crate::{RecvStream, SendStream};
use futures_util::ready;
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Buffer size when writing a request.
const MAX_REQUEST_SIZE: usize = 8192;

/// Max buffer size when reading a body.
const MAX_BODY_READ_SIZE: u64 = 8 * 1024 * 1024;

/// Creates a new HTTP/1 client backed by some async `io` connection.
///
/// Returns a handle to send requests and a connection tuple. The connection
/// is a future that must be polled to "drive" the client forward.
///
/// See [module level doc](index.html) for an example.
pub fn handshake<S>(io: S) -> (SendRequest, Connection<S>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (req_tx, req_rx) = Receiver::new(100);

    let send_req = SendRequest::new(req_tx);

    let conn = Connection(Codec::new(io, req_rx));

    (send_req, conn)
}

/// Sender of new requests.
///
/// See [module level doc](index.html) for an example.
#[derive(Clone)]
pub struct SendRequest {
    req_tx: Sender<Handle>,
}

impl SendRequest {
    fn new(req_tx: Sender<Handle>) -> Self {
        SendRequest { req_tx }
    }

    /// Send a new request.
    ///
    /// The nature of HTTP/1 means only one request can be sent at a time (no multiplexing).
    /// Each request sent before the next has finished will be queued.
    ///
    /// The `no_body` argument indiciates there is no body to be sent. The returned `SendStream`
    /// will not accept data if `no_body` is true.
    ///
    /// Errors if the connection is closed.
    #[instrument(skip(self, req, no_body))]
    pub fn send_request(
        &mut self,
        req: http::Request<()>,
        no_body: bool,
    ) -> Result<(ResponseFuture, SendStream), Error> {
        if req.method() == http::Method::CONNECT {
            return Err(Error::User("hreq-h1 does not support CONNECT".into()));
        }

        trace!("Send request: {:?}", req);

        // Channel to send response back.
        let (res_tx, res_rx) = Receiver::new(1);

        // bounded so we provide backpressure if socket is full.
        let (body_tx, body_rx) = Receiver::new(1);

        let limit = LimitWrite::from_headers(req.headers());

        let no_send_body = no_body || limit.is_no_body();

        // Don't provide an body_rx if headers or no_body flag indicates there is no body.
        let body_rx = if no_send_body { None } else { Some(body_rx) };

        // The handle for the codec/connection.
        let next = Handle {
            req,
            body_rx,
            res_tx: Some(res_tx),
        };

        if !self.req_tx.send(next) {
            // errors on full or closed, and since it's unbound...
            return err_closed();
        }

        let fut = ResponseFuture(res_rx);
        let send = SendStream::new(body_tx, limit, no_send_body, None);

        Ok((fut, send))
    }
}

/// Holder of all details for a new request.
///
/// This internally communicates with the `Connection`.
struct Handle {
    req: http::Request<()>,
    body_rx: Option<Receiver<(Vec<u8>, bool)>>,
    res_tx: Option<Sender<io::Result<http::Response<RecvStream>>>>,
}

/// Future for a `http::Response<RecvStream>>`
pub struct ResponseFuture(Receiver<io::Result<http::Response<RecvStream>>>);

impl Future for ResponseFuture {
    type Output = Result<http::Response<RecvStream>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();

        let res = ready!(Pin::new(&mut this.0).poll_recv(cx, true));

        if let Some(v) = res {
            // nested io::Error
            let v = v?;

            Ok(v).into()
        } else {
            err_closed().into()
        }
    }
}

/// Future that manages the actual connection. Must be awaited to "drive" the connection.
///
/// See [module level doc](index.html) for an example.
pub struct Connection<S>(Codec<S>);

impl<S> Future for Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.0.poll_client(cx)
    }
}

enum State {
    /// Send next request.
    SendReq(SendReq),
    /// Receive response and (if appropriate), send request body.
    RecvRes(Bidirect),
    /// Receive response body.
    RecvBody(BodyReceiver),
}

impl State {
    fn try_forward_error(&mut self, e: io::Error) -> io::Error {
        match self {
            State::SendReq(_) => e,
            State::RecvRes(h) => {
                if let Some(res_tx) = &mut h.handle.res_tx {
                    let c = clone_error(&e);
                    res_tx.send(Err(e));
                    c
                } else {
                    e
                }
            }
            State::RecvBody(h) => {
                let c = clone_error(&e);
                h.body_tx.send(Err(e));
                c
            }
        }
    }
}

fn clone_error(e: &io::Error) -> io::Error {
    io::Error::new(e.kind(), e.to_string())
}

struct Codec<S> {
    io: BufIo<S>,
    state: State,
    req_rx: Receiver<Handle>,
}

impl<S> Codec<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn new(io: S, req_rx: Receiver<Handle>) -> Self {
        Codec {
            io: BufIo::with_capacity(READ_BUF_INIT_SIZE, io),
            state: State::SendReq(SendReq),
            req_rx,
        }
    }

    #[instrument(skip(self, cx))]
    fn poll_client(&mut self, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        // Any error bubbling up closes the connection.
        match self.drive(cx) {
            Poll::Ready(Err(e)) => {
                debug!("Close on error: {:?}", e);

                // Attempt to forward the error to the client side. This is only
                // possible in some states. We either get the original or a cloned
                // error back to bubble up to the connection.
                let e = self.state.try_forward_error(e);

                trace!("{:?} => Closed", self.state);

                Err(e).into()
            }
            r @ _ => r,
        }
    }

    fn drive(&mut self, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        loop {
            ready!(Pin::new(&mut self.io).poll_finish_pending_write(cx))?;

            match &mut self.state {
                State::SendReq(h) => {
                    let next_state = ready!(h.poll_send_req(cx, &mut self.io, &mut self.req_rx))?;

                    if let Some(next_state) = next_state {
                        trace!("SendReq => {:?}", next_state);
                        self.state = next_state;
                    } else {
                        // No more requests to send
                        return Ok(()).into();
                    }
                }
                State::RecvRes(h) => {
                    let next_state = ready!(h.poll_bidirect(cx, &mut self.io))?;

                    if let Some(next_state) = next_state {
                        trace!("RecvRes => {:?}", next_state);
                        self.state = next_state;
                    } else {
                        // No more requests to send
                        return Ok(()).into();
                    }
                }
                State::RecvBody(h) => {
                    let next_state = ready!(h.poll_read_body(cx, &mut self.io))?;

                    if let Some(next_state) = next_state {
                        trace!("RecvBody => {:?}", next_state);
                        self.state = next_state;
                    } else {
                        // No more requests to send
                        return Ok(()).into();
                    }
                }
            }
        }
    }
}

struct SendReq;

impl SendReq {
    #[instrument(skip(self, cx, io, req_rx))]
    fn poll_send_req<S>(
        &mut self,
        cx: &mut Context,
        io: &mut BufIo<S>,
        req_rx: &mut Receiver<Handle>,
    ) -> Poll<io::Result<Option<State>>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let handle = match ready!(Pin::new(req_rx).poll_recv(cx, true)) {
            Some(v) => v,
            None => {
                return Ok(None).into();
            }
        };

        let mut buf = FastBuf::with_capacity(MAX_REQUEST_SIZE);

        let mut write_to = buf.borrow();

        let amount = write_http1x_req(&handle.req, &mut write_to)?;

        write_to.add_len(amount);

        // invariant: Can't have any pending bytes to write now.
        assert!(io.can_poll_write());

        let mut to_send = Some(&buf[..]);

        match Pin::new(io).poll_write_all(cx, &mut to_send, true) {
            Poll::Pending => {
                // invariant: BufIo must have taken control of to_send buf.
                assert!(to_send.is_none());
                // Fall through do state change. The Pending will be caught
                // when looping in drive() and doing poll_finish_pending_write.
            }
            Poll::Ready(v) => v?,
        }

        let next_state = State::RecvRes(Bidirect {
            handle,
            response_allows_reuse: false, // set later in poll_response()
            holder: None,
        });

        Ok(Some(next_state)).into()
    }
}

/// State where we both wait for a server response as well as sending a request body.
struct Bidirect {
    // The request and means to communicate with the user.
    handle: Handle,
    /// Tells whether the response headers/version allows reuse of the connection.
    /// Set by Bidirect::poll_response() when response is received.
    response_allows_reuse: bool,
    /// Holds the received a response whle we are not finished sending the request body.
    holder: Option<(Sender<io::Result<Vec<u8>>>, LimitRead)>,
}

impl Bidirect {
    #[instrument(skip(self, cx, io))]
    fn poll_bidirect<S>(
        &mut self,
        cx: &mut Context,
        io: &mut BufIo<S>,
    ) -> Poll<io::Result<Option<State>>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            if self.handle.res_tx.is_none() && self.handle.body_rx.is_none() {
                break;
            }

            let mut res_tx_pending = false;
            let mut body_tx_pending = false;

            // The order of these two polls matter. We can only register one Waker
            // for this poll. The incoming response might not come before we sent
            // the entire request body. Sending the request body is also within the
            // control of the user of the library. poll_send_body needs to be the
            // latter of these two.

            if self.handle.res_tx.is_some() {
                match self.poll_response(cx, io) {
                    Poll::Pending => {
                        res_tx_pending = true;
                    }
                    Poll::Ready(v) => v?,
                }
            }

            if self.handle.body_rx.is_some() {
                match self.poll_send_body(cx, io) {
                    Poll::Pending => {
                        body_tx_pending = true;
                    }
                    Poll::Ready(v) => v?,
                }
            }

            if res_tx_pending && (body_tx_pending || self.handle.body_rx.is_none())
                || body_tx_pending && (res_tx_pending || self.handle.res_tx.is_none())
            {
                return Poll::Pending;
            }
        }

        let request_allows_reuse =
            allow_reuse(self.handle.req.headers(), self.handle.req.version());

        let next_state = if let Some(holder) = self.holder.take() {
            let (body_tx, limit) = holder;

            let cur_read_size = limit.body_size().unwrap_or(8_192).min(MAX_BODY_READ_SIZE) as usize;

            let brec = BodyReceiver {
                request_allows_reuse,
                response_allows_reuse: self.response_allows_reuse,
                cur_read_size,
                limit,
                body_tx,
            };

            Some(State::RecvBody(brec))
        } else {
            if request_allows_reuse && self.response_allows_reuse {
                trace!("No response body, reuse connection");
                Some(State::SendReq(SendReq))
            } else {
                trace!("No response body, reuse not allowed");
                None
            }
        };

        Ok(next_state).into()
    }

    #[instrument(skip(self, cx, io))]
    fn poll_response<S>(&mut self, cx: &mut Context, io: &mut BufIo<S>) -> Poll<io::Result<()>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let res = ready!(poll_for_crlfcrlf(cx, io, try_parse_res))??;

        // invariant: poll_for_crlfcrlf should provide a full header and
        //            try_parse_res should not be able to get a partial response.
        let res = res.expect("Parsed partial response");

        self.response_allows_reuse = allow_reuse(res.headers(), res.version());

        let limit = LimitRead::from_headers(res.headers(), res.version(), true);

        // https://tools.ietf.org/html/rfc7230#page-31
        // Any response to a HEAD request and any response with a 1xx
        // (Informational), 204 (No Content), or 304 (Not Modified) status
        // code is always terminated by the first empty line after the
        // header fields, regardless of the header fields present in the
        // message, and thus cannot contain a message body.
        let status = res.status();
        let is_no_body = limit.is_no_body()
            || self.handle.req.method() == http::Method::HEAD
            || status.is_informational()
            || status == http::StatusCode::NO_CONTENT
            || status == http::StatusCode::NOT_MODIFIED;

        // TODO: handle CONNECT with a special state where connection becomes a tunnel

        // bounded to have backpressure if client is reading slowly.
        let (body_tx, body_rx) = Receiver::new(1);

        // If there isn't a body, don't sent a holder. This is picked up in poll_bidirect to know
        // which state is the next.
        self.holder = if is_no_body {
            None
        } else {
            Some((body_tx, limit))
        };

        let recv = RecvStream::new(body_rx, is_no_body, None);

        let (parts, _) = res.into_parts();
        let res = http::Response::from_parts(parts, recv);

        // Taking the res_tx indicates to poll_bidirect that response is received.
        let res_tx = self.handle.res_tx.take().expect("Missing res_tx");

        if !res_tx.send(Ok(res)) {
            // res_tx is unbounded, the only error possible is that the
            // response future is dropped and client is not interested in response.
            // This is not an error, we continue to drive the connection.
            trace!("Failed to send http::Response to ResponseFuture");
        }

        Ok(()).into()
    }

    #[instrument(skip(self, cx, io))]
    fn poll_send_body<S>(&mut self, cx: &mut Context, io: &mut BufIo<S>) -> Poll<io::Result<()>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let body_rx = self.handle.body_rx.as_mut().unwrap();

        let (chunk, end) = match ready!(Pin::new(body_rx).poll_recv(cx, true)) {
            Some(v) => v,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "SendStream dropped before sending entire body",
                ))
                .into();
            }
        };

        // invariant: io must not be blocked now.
        assert!(io.can_poll_write());

        let mut to_send = Some(&chunk[..]);

        if end {
            // By removing this we both signal to SendStream that no more body can
            // be sent, as well as poll_bidirect() that we're done sending body.
            self.handle.body_rx = None;
        }

        match Pin::new(io).poll_write_all(cx, &mut to_send, end) {
            Poll::Pending => {
                // invariant: BufIo must have taken the buffer
                assert!(to_send.is_none());
                return Poll::Pending;
            }
            Poll::Ready(v) => v?,
        }

        Ok(()).into()
    }
}

struct BodyReceiver {
    request_allows_reuse: bool,
    response_allows_reuse: bool,
    cur_read_size: usize,
    limit: LimitRead,
    body_tx: Sender<io::Result<Vec<u8>>>,
}

impl BodyReceiver {
    #[instrument(skip(self, cx, io))]
    fn poll_read_body<S>(
        &mut self,
        cx: &mut Context,
        io: &mut BufIo<S>,
    ) -> Poll<io::Result<Option<State>>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            if self.limit.is_complete() {
                break;
            }

            if !ready!(Pin::new(&self.body_tx).poll_ready(cx, true)) {
                // RecvStream is dropped, that's ok we will receive and drop entire body.
            }

            let mut buf = FastBuf::with_capacity(self.cur_read_size);

            let mut read_into = buf.borrow();

            let amount = ready!(self.limit.poll_read(cx, io, &mut read_into))?;

            if amount > 0 {
                read_into.add_len(amount);

                if !self.body_tx.send(Ok(buf.into_vec())) {
                    // RecvStream is dropped, that's ok we will receive and drop entire body.
                }
            } else if !self.limit.is_complete() {
                // https://tools.ietf.org/html/rfc7230#page-32
                // If the sender closes the connection or
                // the recipient times out before the indicated number of octets are
                // received, the recipient MUST consider the message to be
                // incomplete and close the connection.
                //
                // https://tools.ietf.org/html/rfc7230#page-33
                // A client that receives an incomplete response message, which can
                // occur when a connection is closed prematurely or when decoding a
                // supposedly chunked transfer coding fails, MUST record the message as
                // incomplete.

                trace!("Close because read body is not complete");
                const EOF: io::ErrorKind = io::ErrorKind::UnexpectedEof;
                return Err(io::Error::new(EOF, "Partial body")).into();
            }
        }

        let next_state = if self.request_allows_reuse
            && self.response_allows_reuse
            && self.limit.is_reusable()
        {
            trace!("Reuse connection");
            Some(State::SendReq(SendReq))
        } else {
            trace!("Connection is not reusable");
            None
        };

        Ok(next_state).into()
    }
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            State::SendReq(_) => write!(f, "SendReq"),
            State::RecvRes(_) => write!(f, "RecvRes"),
            State::RecvBody(_) => write!(f, "RecvBody"),
        }
    }
}

impl fmt::Debug for SendRequest {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SendRequest")
    }
}

impl fmt::Debug for ResponseFuture {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ResponseFuture")
    }
}

impl<S> fmt::Debug for Connection<S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Connection")
    }
}
