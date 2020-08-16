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
use crate::try_write::try_write;
use crate::Error;
use crate::{AsyncRead, AsyncWrite};
use crate::{RecvStream, SendStream};
use futures_util::ready;
use std::fmt;
use std::future::Future;
use std::io;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Buffer size when writing a request.
const MAX_REQUEST_SIZE: usize = 8192;

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

/// Holder of all details for a new request.
///
/// This internally communicates with the `Connection`.
struct Handle {
    req: http::Request<()>,
    no_send_body: bool,
    rx_body: Receiver<(Vec<u8>, bool)>,
    res_tx: Option<Sender<io::Result<http::Response<RecvStream>>>>,
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
        let (tx_body, rx_body) = Receiver::new(1);

        let limit = LimitWrite::from_headers(req.headers());

        let no_send_body = no_body || limit.is_no_body();

        // The handle for the codec/connection.
        let next = Handle {
            req,
            no_send_body,
            rx_body,
            res_tx: Some(res_tx),
        };

        if !self.req_tx.send(next) {
            // errors on full or closed, and since it's unbound...
            return err_closed();
        }

        let fut = ResponseFuture(res_rx);
        let send = SendStream::new(tx_body, limit, no_send_body, None);

        Ok((fut, send))
    }
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
        this.0.poll_drive(cx)
    }
}

struct Codec<S> {
    io: BufIo<S>,
    req_rx: Receiver<Handle>,
    to_write: Vec<u8>,
    to_write_flush_after: bool,
    state: State,
}

enum State {
    /// Waiting for the next request.
    Waiting,
    /// Send request.
    SendReq(Handle),
    /// Receive response and (if appropriate), send request body.
    RecvRes(Bidirect),
    /// Receive response body.
    RecvBody(BodyReceiver),
    /// Placeholder
    Empty,
}

impl State {
    /// Take the ReqHandle from the state, leave placeholder State::Empty in place.
    fn take_handle(&mut self) -> Handle {
        // Replace reference with placeholder.
        let state = mem::replace(self, State::Empty);

        // Take the handle
        match state {
            State::SendReq(h) => h,
            State::RecvRes(b) => b.handle,
            State::RecvBody(r) => r.handle,
            _ => panic!("take_handle in incorrect state"),
        }
    }

    /// "bubble" an error to API side.
    ///
    /// Depending on state the io error will surface in different places. If
    /// the client is waiting on a FutureResponse, it can go there, or if
    /// the client reading a RecvStream (request body), it can go there.
    fn propagate_error(self, error: io::Error) {
        match self {
            State::SendReq(mut h) => {
                if let Some(res_tx) = h.res_tx.take() {
                    res_tx.send(Err(error));
                }
            }
            State::RecvRes(mut b) => {
                if let Some(res_tx) = b.handle.res_tx.take() {
                    res_tx.send(Err(error));
                } else if let Some((tx_body, _)) = b.holder.take() {
                    if !tx_body.send(Err(error)) {
                        // best effort, and it failed, not much to do. the
                        // error still surfaces in the Connection
                        debug!("Failed to notify RecvStream about error");
                    }
                }
            }
            State::RecvBody(r) => {
                if !r.tx_body.send(Err(error)) {
                    // best effort, and it failed, not much to do. the
                    // error still surfaces in the Connection
                    debug!("Failed to notify RecvStream about error");
                }
            }
            State::Waiting | State::Empty => {}
        }
    }
}

/// Bidirection state. Receive response as well as send request body (if appropriate).
struct Bidirect {
    handle: Handle,
    /// If we are finished sending request body.
    done_req_body: bool,
    /// If we are finished receiving the response.
    done_response: bool,
    /// Whether the response version + headers allow connection reuse.
    allow_reuse: bool,
    /// Placeholder used if we received a response but are not finished
    /// sending the request body.
    holder: Option<(Sender<io::Result<Vec<u8>>>, LimitRead)>,
}

/// Receiver of response body.
struct BodyReceiver {
    handle: Handle,
    limit: LimitRead,
    tx_body: Sender<io::Result<Vec<u8>>>,
    recv_buf: FastBuf,
    allow_reuse: bool,
}

impl<S> Codec<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn new(io: S, req_rx: Receiver<Handle>) -> Self {
        Codec {
            io: BufIo::with_capacity(READ_BUF_INIT_SIZE, io),
            req_rx,
            to_write: Vec::with_capacity(MAX_REQUEST_SIZE),
            to_write_flush_after: false,
            state: State::Waiting,
        }
    }

    #[instrument(skip(self, cx))]
    fn poll_drive(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        loop {
            // whether we have Poll::Pending on io.
            let mut pending_io = false;

            // first try to write queued outgoing bytes until it is pending or empty,
            match try_write(
                cx,
                &mut self.io,
                &mut self.to_write,
                &mut self.to_write_flush_after,
            ) {
                Poll::Ready(v) => v?,
                Poll::Pending => {
                    pending_io = true;
                }
            }

            // then drive state forward
            match ready!(self.drive_state(cx, &mut pending_io)) {
                Ok(do_loop) => {
                    // drive_state() can signal whether we should continue looping.
                    // this is not the same asPoll::Pending. ending the loop means
                    // the connectiong should be gracefully closed.
                    if !do_loop {
                        break;
                    }
                }

                Err(e) => {
                    // clone the error to be sent API side.
                    let clone = io::Error::new(e.kind(), format!("{}", e));

                    // try propagate the error to the client side.
                    let state = mem::replace(&mut self.state, State::Empty);
                    state.propagate_error(clone);

                    // the actual error goes to the connection.
                    return Err(e).into();
                }
            }
        }

        Ok(()).into()
    }

    fn drive_state(&mut self, cx: &mut Context, pending_io: &mut bool) -> Poll<io::Result<bool>> {
        trace!("drive_state: {:?}", self.state);

        match &mut self.state {
            State::Empty => {
                // invariant: Empty is just a placeholder.
                panic!("State::Empty in drive_state");
            }

            State::Waiting => {
                // try get the next request.
                let next = ready!(Pin::new(&mut self.req_rx).poll_recv(cx, true));

                if let Some(h) = next {
                    trace!("Waiting => SendReq");
                    self.state = State::SendReq(h);
                } else {
                    // sender has closed, no more requests to come
                    trace!("Request sender closed");
                    return Ok(false).into();
                }
            }

            State::SendReq(h) => {
                // invariant: should be no bytes waiting to be written at this point.
                assert!(self.to_write.is_empty());

                // prep size.
                self.to_write.resize(MAX_REQUEST_SIZE, 0);

                let amount = write_http1x_req(&h.req, &mut self.to_write)?;

                // scale down
                self.to_write.resize(amount, 0);
                self.to_write_flush_after = true;

                // see if the request allows for reuse.
                let allow_reuse = allow_reuse(h.req.headers(), h.req.version());

                // if we don't expect a request body, we mark the next state as being
                // done for send body already.
                let done_req_body = h.no_send_body;

                let handle = self.state.take_handle();

                trace!("SendReq => RecvRes");
                self.state = State::RecvRes(Bidirect {
                    handle,
                    done_req_body,
                    done_response: false,
                    allow_reuse,
                    holder: None,
                });
            }

            State::RecvRes(b) => {
                // This state does two things. It both sends a request body (if indicated by
                // headers) at the same time as receiving a response.

                let mut req_body_pending = false;

                if !b.done_response {
                    let res = match poll_for_crlfcrlf(cx, &mut self.io, try_parse_res) {
                        Poll::Pending => {
                            *pending_io = true;
                            return Poll::Pending;
                        }
                        Poll::Ready(v) => v??,
                    };

                    // invariant: poll_for_crlfcrlf should provide a full header and
                    //            try_parse_res should not be able to get a partial response.
                    let res = res.expect("Parsed partial response");

                    // the allow_reuse flag will be false if request was sent with connection: close.
                    if b.allow_reuse {
                        // request allowed reused, now check if response does.
                        b.allow_reuse = allow_reuse(res.headers(), res.version());
                    }

                    // we have a response for sure.
                    trace!("done_response: true");
                    b.done_response = true;

                    let limit = LimitRead::from_headers(res.headers(), res.version(), true);

                    // https://tools.ietf.org/html/rfc7230#page-31
                    // Any response to a HEAD request and any response with a 1xx
                    // (Informational), 204 (No Content), or 304 (Not Modified) status
                    // code is always terminated by the first empty line after the
                    // header fields, regardless of the header fields present in the
                    // message, and thus cannot contain a message body.
                    let status = res.status();
                    let is_no_body = limit.is_no_body()
                        || b.handle.req.method() == http::Method::HEAD
                        || status.is_informational()
                        || status == http::StatusCode::NO_CONTENT
                        || status == http::StatusCode::NOT_MODIFIED;

                    // TODO: handle CONNECT with a special state where connection becomes a tunnel

                    // bounded to have backpressure if client is reading slowly.
                    let (tx_body, rx_body) = Receiver::new(1);

                    // holder indicates whether we expect a body.
                    b.holder = if is_no_body {
                        None
                    } else {
                        Some((tx_body, limit))
                    };

                    let recv = RecvStream::new(rx_body, None, is_no_body);

                    let (parts, _) = res.into_parts();
                    let res = http::Response::from_parts(parts, recv);

                    // invariant: the oneshot handle should only exist once.
                    let res_tx = b.handle.res_tx.take().expect("Missing res_tx");

                    if !res_tx.send(Ok(res)) {
                        // res_tx is unbounded, the only error possible is that the
                        // response future is dropped and client is not interested in response.
                        // This is not an error, we continue to drive the connection.
                        trace!("Failed to send http::Response to ResponseFuture");
                    }
                }

                if !b.done_req_body {
                    // Not done sending a request body. Try get a body chunk to send.
                    match Pin::new(&mut b.handle.rx_body).poll_recv(cx, !*pending_io) {
                        Poll::Pending => {
                            // Pending is ok, it means the SendBody has not sent any chunk.
                            trace!("Read req_body: Pending");
                            req_body_pending = true;
                        }

                        Poll::Ready(Some((mut chunk, end))) => {
                            trace!("Read req_body len: {}, end: {}", chunk.len(), end);

                            // Got a chunk to send
                            if self.to_write.is_empty() {
                                self.to_write = chunk;
                            } else {
                                self.to_write.append(&mut chunk);
                            }
                            self.to_write_flush_after = false;

                            // Sender signalled end of stream
                            if end {
                                trace!("done_req_body (end): true");
                                b.done_req_body = true;
                            }

                            // loop to try write body.
                            return Ok(true).into();
                        }

                        Poll::Ready(None) => {
                            // No more body chunks to be expected, SendBody was dropped.
                            trace!("done_req_body (None): true");
                            b.done_req_body = true;
                        }
                    }
                }

                // only proceed out of this state if we have both finished sending a request
                // body and received a response header.
                if b.done_req_body && b.done_response {
                    // TODO: We could validate we actually sent as much body data that was
                    // declared by a content-length header and/or that we sent the chunked
                    // indication for complete. The spec doesn't mention this case specifically,
                    // but it's clearly in "the spirit" to not send half messages.
                    // https://tools.ietf.org/html/rfc7230#page-33

                    if let Some((tx_body, limit)) = b.holder.take() {
                        // carry flag over
                        let allow_reuse = b.allow_reuse;

                        // expect a response body.
                        let handle = self.state.take_handle();

                        trace!("RecvRes => RecvBody");
                        self.state = State::RecvBody(BodyReceiver {
                            handle,
                            tx_body,
                            limit,
                            recv_buf: FastBuf::with_capacity(READ_BUF_INIT_SIZE),
                            allow_reuse,
                        });
                    } else {
                        // expect no response body.
                        if b.allow_reuse {
                            // we can reuse connection
                            trace!("RecvRes => Waiting (expect no body)");
                            self.state = State::Waiting;
                        } else {
                            // drop connection
                            trace!("Connection is not reusable");
                            return Ok(false).into();
                        }
                    }

                    // loop
                    return Ok(true).into();
                }

                // invariant: the only way we can be here is if the request body is
                //            expected and pending.
                assert!(req_body_pending);
                return Poll::Pending;
            }

            State::RecvBody(r) => {
                if !r.recv_buf.is_empty() {
                    trace!("tx_body try send chunk");
                    if !ready!(Pin::new(&r.tx_body).poll_ready(cx, true)) {
                        // receiver of body chunks is dropped, that's ok.
                    }

                    let chunk = r.recv_buf.take_vec();

                    // Since we poll_ready above, the error here is that the receiver is gone,
                    // which isn't a problem.
                    r.tx_body.send(Ok(chunk));

                    trace!("tx_body chunk sent");

                    // loop
                    return Ok(true).into();
                }

                // invariant: if we're here, the recv_buffer must be empty.
                assert!(r.recv_buf.is_empty());

                let mut read_into = r.recv_buf.borrow();

                // read self.io through the limiter to stop reading when we are
                // in place for the next request.
                let res = match r.limit.poll_read(cx, &mut self.io, &mut read_into[..]) {
                    Poll::Pending => {
                        *pending_io = true;
                        return Poll::Pending;
                    }
                    Poll::Ready(v) => v,
                };
                trace!("Read res_body: ({:?})", res);
                let amount = res?;

                if amount > 0 {
                    // commit additional len to buf
                    read_into.add_len(amount);

                    // loop
                    return Ok(true).into();
                } else {
                    // scale buffer back.
                    drop(read_into);

                    // ensure the limiter was complete, or drop the connection.
                    if !r.limit.is_complete() {
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

                    if r.allow_reuse && r.limit.is_reusable() {
                        // No more response body. ready to handle next request.
                        // NB. This drops the r.tx_body which means the RecvStream will
                        // read a 0 amount on next try.
                        trace!("RecvBody => Waiting");
                        self.state = State::Waiting;
                    } else {
                        // This connection can not be reused, could for instance be http/1.0
                        // without connection: keep-alive.
                        trace!("Connection is not reusable");
                        return Ok(false).into();
                    }
                }
            }
        }

        Ok(true).into()
    }
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Waiting => write!(f, "Waiting")?,
            State::SendReq(h) => write!(f, "SendReq: {:?}", h.req)?,
            State::RecvRes(b) => write!(
                f,
                "RecvRes done_req_body: {}, done_response: {}",
                b.done_req_body, b.done_response
            )?,
            State::RecvBody(_) => write!(f, "RecvBody")?,
            State::Empty => write!(f, "Empty")?,
        }
        Ok(())
    }
}

impl fmt::Debug for SendRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SendRequest")
    }
}

impl fmt::Debug for ResponseFuture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ResponseFuture")
    }
}

impl<S> fmt::Debug for Connection<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Connection")
    }
}
