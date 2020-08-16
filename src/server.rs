//! Server implementation of the HTTP/1.1 protocol.
//!
//! # Example
//!
//! # Example
//!
//! ```rust, no_run
//! use hreq_h1::server;
//! use std::error::Error;
//! use async_std::net::TcpListener;
//! use http::{Response, StatusCode};
//!
//! #[async_std::main]
//! async fn main() -> Result<(), Box<dyn Error>> {
//!     let mut listener = TcpListener::bind("127.0.0.1:3000").await?;
//!
//!     // Accept all incoming TCP connections.
//!     loop {
//!         if let Ok((socket, _peer_addr)) = listener.accept().await {
//!
//!             // Spawn a new task to process each connection individually
//!             async_std::task::spawn(async move {
//!                 let mut h1 = server::handshake(socket);
//!
//!                 // Handle incoming requests from this socket, one by one.
//!                 while let Some(request) = h1.accept().await {
//!                     let (req, mut respond) = request.unwrap();
//!
//!                     println!("Receive request: {:?}", req);
//!
//!                     // Build a response with no body, since
//!                     // that is sent later.
//!                     let response = Response::builder()
//!                         .status(StatusCode::OK)
//!                         .body(())
//!                         .unwrap();
//!
//!                     // Send the response back to the client
//!                     let mut send_body = respond
//!                         .send_response(response, false).unwrap();
//!
//!                     send_body.send_data(b"Hello world!", true)
//!                         .await.unwrap();
//!                 }
//!             });
//!         }
//!     }
//!
//!    Ok(())
//! }
//!
//!

use crate::buf_reader::BufIo;
use crate::fast_buf::FastBuf;
use crate::http11::{poll_for_crlfcrlf, try_parse_req, write_http1x_res, READ_BUF_INIT_SIZE};
use crate::limit::allow_reuse;
use crate::limit::{LimitRead, LimitWrite};
use crate::mpsc::{Receiver, Sender};
use crate::Error;
use crate::RecvStream;
use crate::SendStream;
use crate::{AsyncRead, AsyncWrite};
use futures_util::future::poll_fn;
use futures_util::ready;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// Buffer size when writing a request.
const MAX_RESPONSE_SIZE: usize = 8192;

/// "handshake" to create a connection.
///
/// See [module level doc](index.html) for an example.
pub fn handshake<S>(io: S) -> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    Connection(Arc::new(Mutex::new(Codec::new(io))))
}

/// Server connection for accepting incoming requests.
///
/// See [module level doc](index.html) for an example.
pub struct Connection<S>(Arc<Mutex<Codec<S>>>);

impl<S> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// Accept a new incoming request to handle. One must accept new requests continuously
    /// to "drive" the connection forward, also for the already accepted requests.
    pub async fn accept(
        &mut self,
    ) -> Option<Result<(http::Request<RecvStream>, SendResponse), Error>> {
        poll_fn(|cx| Pin::new(&mut *self).poll_accept(cx)).await
    }

    /// Wait until the connection has sent/flush all data and is ok to drop.
    pub async fn close(mut self) {
        poll_fn(|cx| Pin::new(&mut self).poll_close(cx)).await;
    }

    #[instrument(skip(self, cx))]
    fn poll_accept(
        self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Option<Result<(http::Request<RecvStream>, SendResponse), Error>>> {
        let this = self.get_mut();

        let inner = this.0.clone();

        let mut lock = this.0.lock().unwrap();

        lock.poll_server(cx, inner, true, true)
    }

    #[instrument(skip(self, cx))]
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        let inner = self.0.clone();

        let mut lock = self.0.lock().unwrap();

        // It doesn't matter what the return value is, we just need it to not be pending.
        ready!(lock.poll_server(cx, inner, false, true));

        ().into()
    }
}

/// Handle to send a response and body back for a single request.
///
/// See [module level doc](index.html) for an example.
pub struct SendResponse {
    drive_external: Box<dyn DriveExternal>,
    tx_res: Sender<(http::Response<()>, bool, Receiver<(Vec<u8>, bool)>)>,
}

impl SendResponse {
    /// Send a response to a request. Notice that the body is sent separately afterwards.
    ///
    /// The lib will infer that there will be no response body if there is a `content-length: 0`
    /// header or a status code that should not have a body (1xx, 204, 304).
    ///
    /// `no_body` is an alternative way, in addition to headers and status, to inform the library
    /// there will be no body to send.
    ///
    /// It's an error to send a body when the status or headers indicate there should not be one.
    #[instrument(skip(self, response, no_body))]
    pub fn send_response(
        self,
        response: http::Response<()>,
        no_body: bool,
    ) -> Result<SendStream, Error> {
        trace!("Send response: {:?}", response);

        // bounded to get back pressure
        let (tx_body, rx_body) = Receiver::new(1);

        let limit = LimitWrite::from_headers(response.headers());

        let status = response.status();

        // https://tools.ietf.org/html/rfc7230#page-31
        // any response with a 1xx (Informational), 204 (No Content), or
        // 304 (Not Modified) status code is always terminated by the first
        // empty line after the header fields, regardless of the header fields
        // present in the message, and thus cannot contain a message body.
        let ended = no_body
            || limit.is_no_body()
            || status.is_informational()
            || status == http::StatusCode::NO_CONTENT
            || status == http::StatusCode::NOT_MODIFIED;

        let drive_external = Some(self.drive_external);

        let send = SendStream::new(tx_body, limit, ended, drive_external);

        if !self.tx_res.send((response, ended, rx_body)) {
            Err(io::Error::new(io::ErrorKind::Other, "Connection closed"))?;
        }

        Ok(send)
    }
}
pub(crate) struct Codec<S> {
    io: BufIo<S>,
    state: State,
}

impl<S> Codec<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn new(io: S) -> Self {
        Codec {
            io: BufIo::with_capacity(READ_BUF_INIT_SIZE, io),
            state: State::RecvReq(RecvReq),
        }
    }
}

impl<S> Codec<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    #[instrument(skip(self, cx, inner, want_next_req, register_on_user_input))]
    fn poll_server(
        &mut self,
        cx: &mut Context,
        inner: Arc<Mutex<Codec<S>>>,
        want_next_req: bool,
        register_on_user_input: bool,
    ) -> Poll<Option<Result<(http::Request<RecvStream>, SendResponse), Error>>> {
        // Any error bubbling up closes the connection.
        match self.drive(cx, inner, want_next_req, register_on_user_input) {
            Poll::Ready(Some(Err(e))) => {
                debug!("Close on error: {:?}", e);

                trace!("{:?} => Closed", self.state);
                self.state = State::Closed;

                Some(Err(e)).into()
            }
            r @ _ => r,
        }
    }

    fn drive(
        &mut self,
        cx: &mut Context,
        inner: Arc<Mutex<Codec<S>>>,
        want_next_req: bool,
        register_on_user_input: bool,
    ) -> Poll<Option<Result<(http::Request<RecvStream>, SendResponse), Error>>> {
        loop {
            ready!(Pin::new(&mut self.io).poll_finish_pending_write(cx))?;

            match &mut self.state {
                State::RecvReq(h) => {
                    // poll_drive() called with the intention of just driving server state
                    // and not to handle the next read request.
                    if !want_next_req {
                        return None.into();
                    }

                    let (next_req, next_state) = ready!(h.poll_next_req(cx, inner, &mut self.io))?;

                    trace!("RecvReq => {:?}", next_state);
                    self.state = next_state;

                    return Some(Ok(next_req)).into();
                }
                State::SendRes(h) => {
                    let next_state =
                        ready!(h.poll_bidirect(cx, &mut self.io, register_on_user_input))?;

                    trace!("SendRes => {:?}", next_state);
                    self.state = next_state;
                }
                State::SendBody(h) => {
                    let next_state =
                        ready!(h.poll_send_body(cx, &mut self.io, register_on_user_input))?;

                    trace!("SendBody => {:?}", next_state);
                    self.state = next_state;
                }
                State::Closed => {
                    // Nothing to do
                    return None.into();
                }
            }
        }
    }
}

enum State {
    /// Receive next request.
    RecvReq(RecvReq),
    /// Send response, and (if appropriate) receive request body.
    SendRes(Bidirect),
    /// Send response body.
    SendBody(BodySender),
    /// Closed, error or cleanly.
    Closed,
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            State::RecvReq(_) => write!(f, "RecvReq"),
            State::SendRes(_) => write!(f, "SendRes"),
            State::SendBody(_) => write!(f, "SendBody"),
            State::Closed => write!(f, "Closed"),
        }
    }
}

/// Waiting for the next request to arrive.
///
/// Reads a buffer for 2 x crlf to know we got an entire request header.
struct RecvReq;

impl RecvReq {
    #[instrument(skip(self, cx, inner, io))]
    fn poll_next_req<S>(
        &mut self,
        cx: &mut Context,
        inner: Arc<Mutex<Codec<S>>>,
        io: &mut BufIo<S>,
    ) -> Poll<Result<((http::Request<RecvStream>, SendResponse), State), Error>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let req = ready!(poll_for_crlfcrlf(cx, io, try_parse_req))??;

        // invariant: poll_for_crlfcrlf must have read a full request header.
        let req = req.expect("Didn't read full request");

        // Limiter to read the correct body amount from the socket.
        let limit = LimitRead::from_headers(req.headers(), req.version(), false);

        let request_allows_reuse = allow_reuse(req.headers(), req.version());

        // https://tools.ietf.org/html/rfc7230#page-31
        // Any response to a HEAD request ... is always terminated by the first
        // empty line after the header fields, regardless of the header fields
        // present in the message, and thus cannot contain a message body.
        let is_no_body = limit.is_no_body() || req.method() == http::Method::HEAD;

        // bound channel to get backpressure
        let (tx_body, rx_body) = Receiver::new(1);

        let (tx_res, rx_res) = Receiver::new(1);

        // Prepare the new "package" to be delivered out of the poll loop.
        let package = {
            let drive_external1: Box<dyn DriveExternal> = Box::new(inner.clone());
            let drive_external2: Box<dyn DriveExternal> = Box::new(inner);

            let recv = RecvStream::new(rx_body, is_no_body, Some(drive_external1));

            let (parts, _) = req.into_parts();
            let req = http::Request::from_parts(parts, recv);

            let send = SendResponse {
                drive_external: drive_external2,
                tx_res,
            };

            (req, send)
        };

        // Drop tx_body straight away if headers indicate we are not expecting any request body.
        let tx_body = if limit.is_no_body() {
            None
        } else {
            Some(tx_body)
        };

        let bidirect = Bidirect {
            limit,
            request_allows_reuse,
            tx_body,
            rx_res: Some(rx_res),
            holder: None,
        };

        Ok((package, State::SendRes(bidirect))).into()
    }
}

/// Both receive a request body (if headers indicate it), and
/// send a response which is obtained from the library user.
struct Bidirect {
    // limiter/dechunker for reading incoming request body.
    limit: LimitRead,
    // remember this for when we are to go back into state RecvReq
    request_allows_reuse: bool,
    // send body chunks from socket to this sender.
    tx_body: Option<Sender<io::Result<Vec<u8>>>>,
    // receive a response (once), from this to pass to socket.
    rx_res: Option<Receiver<(http::Response<()>, bool, Receiver<(Vec<u8>, bool)>)>>,
    // Holder of data from rx_res used to receive/write a response body.
    holder: Option<(bool, LimitWrite, Receiver<(Vec<u8>, bool)>)>,
}

impl Bidirect {
    #[instrument(skip(self, cx, io))]
    fn poll_bidirect<S>(
        &mut self,
        cx: &mut Context,
        io: &mut BufIo<S>,
        register_on_user_input: bool,
    ) -> Poll<Result<State, Error>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // Alternate between attempting to send a user response and receving more body chunks.
        loop {
            // We keep on looping until both these are None which signals
            // the bidirect state is done.
            if self.rx_res.is_none() && self.tx_body.is_none() {
                break;
            }

            let mut send_resp_pending = false;

            // Handle user sending a response.
            if self.rx_res.is_some() {
                // register_on_user_input means we should register a Waker when polling for a response
                // from the user. We should not register two wakers for the same Context, which means
                // if we get Pending while register_on_user_input is false, we can proceed to also drive IO.
                match self.poll_send_resp(cx, io, register_on_user_input) {
                    Poll::Pending => {
                        send_resp_pending = true;
                    }
                    Poll::Ready(v) => {
                        v?;
                    }
                }
            }

            if register_on_user_input && send_resp_pending {
                // A Waker is registered in mpsc::Receiver::poll_recv.
                // We cannot continue with IO since that would risk
                // registering wakers in multiple places.
                return Poll::Pending;
            }

            // Read request body from socket and propagate to user.
            if self.tx_body.is_some() {
                ready!(self.poll_read_body(cx, io))?;
            }
        }

        // invariant: we must have the details required in holder.
        let (no_body, limit, rx_body) = self.holder.take().expect("Holder of rx_body");

        let next_state = if no_body || limit.is_no_body() {
            if self.request_allows_reuse {
                trace!("Request does not allow reuse");
                State::Closed
            } else {
                trace!("No body to send");
                State::RecvReq(RecvReq)
            }
        } else {
            State::SendBody(BodySender {
                request_allows_reuse: self.request_allows_reuse,
                rx_body,
            })
        };

        Ok(next_state).into()
    }

    #[instrument(skip(self, cx, io, register_on_user_input))]
    fn poll_send_resp<S>(
        &mut self,
        cx: &mut Context,
        io: &mut BufIo<S>,
        register_on_user_input: bool,
    ) -> Poll<Result<(), Error>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // We shouldn't be here unless we have rx_res.
        let rx_res = self.rx_res.as_mut().unwrap();

        if let Some((res, end, rx_body)) =
            ready!(Pin::new(rx_res).poll_recv(cx, register_on_user_input))
        {
            // We got a response from the user.

            // Remember things for the next state, SendBody
            let limit = LimitWrite::from_headers(res.headers());
            self.holder = Some((end, limit, rx_body));

            let mut buf = FastBuf::with_capacity(MAX_RESPONSE_SIZE);

            let mut write_to = buf.borrow();

            let amount = write_http1x_res(&res, &mut write_to[..])?;

            write_to.add_len(amount);

            let mut to_send = Some(&buf[..]);

            // invariant: poll_drive deals with pending outgoing io before anything
            //            else. at this point we should not have any pending write io.
            assert!(io.can_poll_write());

            match Pin::new(io).poll_write_all(cx, &mut to_send, true) {
                Poll::Pending => {
                    // invariant: Pending without "taking" all to_send bytes is a fault in BufIo
                    assert!(to_send.is_none());
                }
                Poll::Ready(v) => v?,
            }

            // Remove rx_res since we don't need anything more from it. This makes
            // poll_bidirect() not go into poll_send_resp anymore.
            self.rx_res.take();
        } else {
            // The user dropped the SendResponse instance before sending a response.
            // This is a user fault.
            return Err(Error::User(format!(
                "SendResponse dropped before sending any response"
            )))
            .into();
        }

        Ok(()).into()
    }

    #[instrument(skip(self, cx, io))]
    fn poll_read_body<S>(&mut self, cx: &mut Context, io: &mut BufIo<S>) -> Poll<Result<(), Error>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // We shouldn't be here unless we have tx_body.
        let tx_body = self.tx_body.as_mut().unwrap();

        // Ensure we can send off any incoming read chunk to the user. This makes for flow control.
        if !ready!(Pin::new(&*tx_body).poll_ready(cx, true)) {
            // User has dropped the RecvStream. That's ok, we will just discard
            // the entire incoming body.
        }

        let buf = ready!(Pin::new(&mut *io).poll_fill_buf(cx, false))?;

        if buf.is_empty() {
            // End of incoming data before we have fulfilled the LimitRead.
            // configured by the headers.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF before complete body received",
            )
            .into())
            .into();
        }

        let mut chunk = FastBuf::with_capacity(buf.len());

        let mut read_into = chunk.borrow();

        let amount = ready!(self.limit.poll_read(cx, io, &mut read_into[..]))?;

        if amount > 0 {
            // consume read_into to reset chunk size back to correct length as per FastBuf contract.
            read_into.add_len(amount);

            tx_body.send(Ok(chunk.into_vec()));
        } else {
            // Remove tx_body Sender which indicates to the RecvStream that there is
            // no more body chunks to come.
            self.tx_body.take();
        }

        Ok(()).into()
    }
}

/// Sender of a response body.
struct BodySender {
    request_allows_reuse: bool,
    rx_body: Receiver<(Vec<u8>, bool)>,
}

impl BodySender {
    #[instrument(skip(self, cx, io))]
    fn poll_send_body<S>(
        &mut self,
        cx: &mut Context,
        io: &mut BufIo<S>,
        register_on_user_input: bool,
    ) -> Poll<Result<State, Error>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // Keep try to send body chunks until we got no more to send or Pending.
        loop {
            // Always abort on Pending, but register_on_user_input controls whether this resulted in
            // any Waker being registered. This makes for flow control.
            let next = ready!(Pin::new(&mut self.rx_body).poll_recv(cx, register_on_user_input));

            // Pending writes must have been dealt with already at the beginning of poll_drive().
            assert!(io.can_poll_write());

            if let Some((chunk, end)) = next {
                let mut buf = Some(&chunk[..]);

                match Pin::new(&mut *io).poll_write_all(cx, &mut buf, end) {
                    Poll::Pending => {
                        // invariant: The buffer must still been taken by poll_write.
                        assert!(buf.is_none());
                        return Poll::Pending;
                    }
                    Poll::Ready(v) => v?,
                }

                if end {
                    let next_state = if self.request_allows_reuse {
                        trace!("Request does not allow reuse");
                        State::Closed
                    } else {
                        trace!("Finished sending body");
                        State::RecvReq(RecvReq)
                    };

                    return Ok(next_state).into();
                }
            } else {
                // This is a fault, we are expecting more body chunks and
                // the SendStream was dropped.
                warn!("SendStream dropped before sending end_of_body");

                return Err(io::Error::new(io::ErrorKind::Other, "Unexpected end of body").into())
                    .into();
            }
        }
    }
}

impl<S> std::fmt::Debug for Connection<S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", "Connection")
    }
}

impl fmt::Debug for SendResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SendResponse")
    }
}

pub(crate) trait DriveExternal: Send {
    fn poll_drive_external(&self, cx: &mut Context) -> Poll<Result<(), Error>>;
}

impl<S> DriveExternal for Arc<Mutex<Codec<S>>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    fn poll_drive_external(&self, cx: &mut Context) -> Poll<Result<(), Error>> {
        let inner = self.clone();

        let mut lock = self.lock().unwrap();

        match lock.poll_server(cx, inner, false, false) {
            Poll::Pending => {
                let pending_io = lock.io.pending_rx() || lock.io.pending_rx();

                // Only propagate Pending if it was due to io. We send register_on_user_input
                // false, which means that reading user input from SendResponse and SendStream
                // will not have registered a Waker. Pending due to IO most propagate as Pending.
                if pending_io {
                    Poll::Pending
                } else {
                    Ok(()).into()
                }
            }
            Poll::Ready(Some(Ok(_))) => {
                // invariant: want_next_req is false, this should not happend.
                unreachable!("Got next request in poll_drive_external");
            }
            // Propagate error
            Poll::Ready(Some(Err(e))) => Err(e).into(),
            //
            Poll::Ready(None) => Ok(()).into(),
        }
    }
}
