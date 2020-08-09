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
//!                     let send_body = respond
//!                         .send_response(response, false).unwrap();
//!
//!                     // For big bodies, we would alternate we get flow
//!                     // control by alternating between ready/send_data
//!                     // in a loop.
//!                     let mut send_body = send_body.ready()
//!                         .await.unwrap();
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

use crate::http11::{poll_for_crlfcrlf, try_parse_req, write_http1x_res};
use crate::limit::allow_reuse;
use crate::limit::{LimitRead, LimitWrite};
use crate::try_write::try_write;
use crate::Error;
use crate::RecvStream;
use crate::SendStream;
use crate::{AsyncRead, AsyncWrite};
use futures_channel::{mpsc, oneshot};
use futures_util::future::poll_fn;
use futures_util::ready;
use futures_util::sink::Sink;
use futures_util::stream::Stream;
use std::fmt;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// Size of buffer reading request body into.
const READ_BUF_INIT_SIZE: usize = 16_384;

/// Buffer size when writing a request.
const MAX_RESPONSE_SIZE: usize = 8192;

/// "handshake" to create a connection.
///
/// See [module level doc](index.html) for an example.
pub fn handshake<S>(io: S) -> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    Connection(Arc::new(Mutex::new(Codec::new(io))), PhantomData)
}

/// Server connection for accepting incoming requests.
///
/// See [module level doc](index.html) for an example.
//
// NB: The PhantomData here is to maintain API parity with h2. Keeping Connection generic over <S>
// gives us a future option to make a better impl that doesn't hide the IO behind a Box<dyn trait>.
pub struct Connection<S>(Arc<Mutex<Codec>>, PhantomData<S>);

impl<S> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_accept(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<(http::Request<RecvStream>, SendResponse), Error>>> {
        let this = self.get_mut();

        let inner = this.0.clone();

        let mut lock = this.0.lock().unwrap();

        lock.poll_drive(cx, true, inner)
    }

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

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let inner = self.0.clone();

        let mut codec = self.0.lock().unwrap();

        // It doesn't matter what the return value is, we just need it to not be pending.
        ready!(codec.poll_drive(cx, true, inner.clone()));

        ().into()
    }
}

/// Handle to send a response and body back for a single request.
///
/// See [module level doc](index.html) for an example.
pub struct SendResponse {
    inner: Arc<Mutex<Codec>>,
    tx_res: oneshot::Sender<(http::Response<()>, bool, mpsc::Receiver<(Vec<u8>, bool)>)>,
}

impl SendResponse {
    /// Send a response to a request. Notice that the body is sent separately afterwards.
    pub fn send_response(
        self,
        response: http::Response<()>,
        end_of_stream: bool,
    ) -> Result<SendStream, Error> {
        // bounded to get back pressure
        let (tx_body, rx_body) = mpsc::channel(2);

        let limit = LimitWrite::from_headers(response.headers());

        let status = response.status();

        // https://tools.ietf.org/html/rfc7230#page-31
        // any response with a 1xx (Informational), 204 (No Content), or
        // 304 (Not Modified) status code is always terminated by the first
        // empty line after the header fields, regardless of the header fields
        // present in the message, and thus cannot contain a message body.
        let ended = limit.is_no_body()
            || end_of_stream
            || status.is_informational()
            || status == http::StatusCode::NO_CONTENT
            || status == http::StatusCode::NOT_MODIFIED;

        let send = SendStream::new(tx_body, limit, ended, Some(self.inner));

        self.tx_res
            .send((response, ended, rx_body))
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Connection closed"))?;

        Ok(send)
    }
}

pub(crate) struct Codec {
    io: Box<dyn Io>,
    state: State,
    // current bytes to be written
    to_write: Vec<u8>,
    to_write_flush_after: bool,
    // buffer to receive next request into, and then body bytes
    read_buf: Vec<u8>,
}

enum State {
    /// Waiting for the next request.
    Waiting,
    /// Receive request.
    RecvReq,
    /// Send response, and (if appropriate) receive request body.
    SendRes(Bidirect),
    /// Send response body.
    SendBody(BodySender),
    /// Closed
    Closed,
}

/// State where can both send a response and receive a request body, if appropriate.
struct Bidirect {
    limit: LimitRead,
    tx_body: Option<mpsc::Sender<io::Result<Vec<u8>>>>,
    tx_body_needs_flush: bool,
    rx_res: oneshot::Receiver<(http::Response<()>, bool, mpsc::Receiver<(Vec<u8>, bool)>)>,
    done_req_body: bool,
    done_response: bool,
    /// Placeholder used if we received a response but are not finished
    /// sending the request body.
    holder: Option<(bool, LimitWrite, Option<mpsc::Receiver<(Vec<u8>, bool)>>)>,
    reusable: bool,
}

struct BodySender {
    rx_body: mpsc::Receiver<(Vec<u8>, bool)>,
    ended: bool,
    reusable: bool,
}

#[derive(Debug)]
enum DriveResult {
    /// Next request arrived.
    Request((http::Request<RecvStream>, SendResponse)),
    /// Loop the drive_server again.
    Loop,
    /// No more requests.
    Close,
    /// State is Waiting and want_next_req is false.
    Waiting,
}

impl Codec {
    fn new<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(io: S) -> Self {
        Codec {
            io: Box::new(IoAdapt(io)),
            state: State::Waiting,
            to_write: vec![],
            to_write_flush_after: false,
            read_buf: Vec::with_capacity(READ_BUF_INIT_SIZE),
        }
    }

    pub(crate) fn poll_drive(
        &mut self,
        cx: &mut Context<'_>,
        want_next_req: bool,
        inner: Arc<Mutex<Codec>>,
    ) -> Poll<Option<Result<(http::Request<RecvStream>, SendResponse), Error>>> {
        loop {
            // try write any bytes ready to be sent.
            while try_write(
                cx,
                &mut self.io,
                &mut self.to_write,
                &mut self.to_write_flush_after,
            )? {}

            let ret = ready!(self.drive_state(cx, want_next_req, inner.clone()))?;
            match ret {
                DriveResult::Request(p) => {
                    return Poll::Ready(Some(Ok(p)));
                }

                DriveResult::Loop => {
                    continue;
                }

                DriveResult::Close | DriveResult::Waiting => {
                    return Poll::Ready(None);
                }
            }
        }
    }

    fn drive_state(
        &mut self,
        cx: &mut Context<'_>,
        want_next_req: bool,
        inner: Arc<Mutex<Codec>>,
    ) -> Poll<Result<DriveResult, io::Error>> {
        trace!("drive_state: {:?}", self.state);

        match &mut self.state {
            State::Closed => {
                return Poll::Ready(Ok(DriveResult::Close));
            }

            State::Waiting => {
                if !want_next_req {
                    return Poll::Ready(Ok(DriveResult::Waiting));
                }

                if let Err(e) = ready!(poll_for_crlfcrlf(cx, &mut self.read_buf, &mut self.io)) {
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        trace!("Connection closed");
                    } else {
                        trace!("Other error when reading next: {:?}", e);
                    }
                    return Poll::Ready(Ok(DriveResult::Close));
                }

                // we got a full request header in buf
                self.state = State::RecvReq;
            }

            State::RecvReq => {
                // invariant: poll_for_crlfcrlf must have read a full request.
                let (req, size) = try_parse_req(&self.read_buf)?.expect("Didn't read full request");

                // invariant: entire buffer should have been used up.
                assert_eq!(self.read_buf.len(), size);

                // reset for reuse when reading request body.
                self.read_buf.resize(0, 0);

                // Limiter to read the correct body amount from the socket.
                let limit = LimitRead::from_headers(req.headers(), req.version(), false);

                let reusable = allow_reuse(req.headers(), req.version());

                // https://tools.ietf.org/html/rfc7230#page-31
                // Any response to a HEAD request ... is always terminated by the first
                // empty line after the header fields, regardless of the header fields
                // present in the message, and thus cannot contain a message body.
                let is_no_body = limit.is_no_body() || req.method() == http::Method::HEAD;

                // bound channel to get backpressure
                let (tx_body, rx_body) = mpsc::channel(2);

                let (tx_res, rx_res) = oneshot::channel();

                // Prepare the new "package" to be delivered out of the poll loop.
                let package = {
                    let recv = RecvStream::new(rx_body, Some(inner.clone()), is_no_body);

                    let (parts, _) = req.into_parts();
                    let req = http::Request::from_parts(parts, recv);

                    let send = SendResponse { inner, tx_res };

                    (req, send)
                };

                let done_req_body = limit.is_no_body();

                self.state = State::SendRes(Bidirect {
                    limit,
                    tx_body: Some(tx_body),
                    tx_body_needs_flush: false,
                    rx_res,
                    done_req_body,
                    done_response: false,
                    holder: None,
                    reusable,
                });

                // Exit drive with the packet.
                return Ok(DriveResult::Request(package)).into();
            }

            State::SendRes(h) => {
                // if the tx_body needs flushing, deal with that first
                if h.tx_body_needs_flush {
                    if let Some(tx_body) = h.tx_body.as_mut() {
                        // The RecvStream might be dropped, that's ok.
                        ready!(Pin::new(tx_body).poll_flush(cx)).ok();
                    }
                    h.tx_body_needs_flush = false;
                }

                let mut req_body_pending = false;

                if !h.done_req_body {
                    if !self.read_buf.is_empty() {
                        if let Some(tx_body) = h.tx_body.as_mut() {
                            if let Err(_) = ready!(tx_body.poll_ready(cx)) {
                                // The RecvStream is dropped, that's ok, we continue
                                // to drive the connection. Specifically we need
                                // to still exhaust the entire body to ensure
                                // the socket can be reused for a new request.
                            }

                            let chunk = mem::replace(
                                &mut self.read_buf,
                                Vec::with_capacity(READ_BUF_INIT_SIZE),
                            );

                            // The RecvStream might be dropped, that's ok.
                            let needs_flush = tx_body.start_send(Ok(chunk)).is_ok();

                            // As per Sink contract, flush after send.
                            h.tx_body_needs_flush = needs_flush;

                            // loop to send off what was used received.
                            return Ok(DriveResult::Loop).into();
                        } else {
                            // empty buffer
                            self.read_buf.resize(0, 0);
                            h.tx_body_needs_flush = false;
                        }
                    }

                    self.read_buf.resize(READ_BUF_INIT_SIZE, 0);

                    match h.limit.poll_read(cx, &mut self.io, &mut self.read_buf) {
                        Poll::Pending => {
                            // Pending is ok, we can still make progress on sending the response.
                            trace!("Read req_body: Pending");
                            req_body_pending = true;
                        }

                        Poll::Ready(r) => {
                            trace!("Read req_body: Ready ({:?})", r);

                            // read error?
                            let amount = r?;

                            if amount == 0 {
                                // remove the tx_body to indicate to receiver
                                // side that no more data is coming.
                                h.tx_body.take();
                                trace!("done_req_body: true");
                                h.done_req_body = true;
                            }

                            // size down to read amount.
                            self.read_buf.resize(amount, 0);

                            // loop to send off what was used received.
                            return Ok(DriveResult::Loop).into();
                        }
                    }
                }

                if !h.done_response {
                    let (res, end, rx_body) = match ready!(Pin::new(&mut h.rx_res).poll(cx)) {
                        Ok((res, end, rx_body)) => (res, end, Some(rx_body)),
                        Err(_) => {
                            // SendResponse was dropped before any response was sent.
                            // That's a fault, but we can save the connection! :)
                            warn!("SendResponse dropped without sending a response");
                            (
                                http::Response::builder().status(500).body(()).unwrap(),
                                true,
                                None,
                            )
                        }
                    };

                    // got a response now.
                    trace!("done_response: true");
                    h.done_response = true;

                    // invariant: there should be nothing to send now.
                    assert!(self.to_write.is_empty());

                    self.to_write.resize(MAX_RESPONSE_SIZE, 0);

                    // invariant: we should be able to write _any_ response.
                    let amount =
                        write_http1x_res(&res, &mut self.to_write).expect("Write http::Response");

                    self.to_write.resize(amount, 0);
                    self.to_write_flush_after = true;

                    // invariant: amount must match written buffer length
                    assert_eq!(self.to_write.len(), amount);

                    let limit = LimitWrite::from_headers(res.headers());

                    h.holder = Some((end, limit, rx_body));
                }

                if h.done_req_body && h.done_response {
                    // invariant: We can't be here without sending a response..
                    let (end, limit, rx_body) = h.holder.take().expect("Missing holder");

                    if end || limit.is_no_body() {
                        // No response body to send.
                        trace!("Connection is reusable: {}", h.reusable);
                        self.state = if h.reusable {
                            self.read_buf.resize(0, 0);
                            State::Waiting
                        } else {
                            State::Closed
                        };
                    } else if let Some(rx_body) = rx_body {
                        self.state = State::SendBody(BodySender {
                            rx_body,
                            ended: false,
                            reusable: h.reusable,
                        });
                    } else {
                        // invariant: end or limit.is_no_body() means there is no body,
                        unreachable!("No rx_body when expected");
                    }

                    return Ok(DriveResult::Loop).into();
                }

                // invariant: if we are here, it must be because request body is pending.
                assert!(req_body_pending);
                return Poll::Pending;
            }

            State::SendBody(b) => {
                // If there is a chunk to write, we will wait until it's written.
                // Doing Poll::Pending here is deliberate. Before drive_state() we have
                // made as much progress in try_write as possible.
                if !self.to_write.is_empty() {
                    return Poll::Pending;
                }

                if b.ended {
                    self.state = State::Waiting;
                    return Ok(DriveResult::Loop).into();
                }

                let next = ready!(Pin::new(&mut b.rx_body).poll_next(cx));

                if let Some((mut chunk, end)) = next {
                    if end {
                        b.ended = true;
                    }

                    // queue up next chunk to write out.
                    if self.to_write.is_empty() {
                        self.to_write = chunk;
                    } else {
                        self.to_write.append(&mut chunk);
                    }
                    self.to_write_flush_after = end;

                    if b.ended && self.to_write.is_empty() {
                        trace!("Connection is reusable: {}", b.reusable);
                        self.state = if b.reusable {
                            self.read_buf.resize(0, 0);
                            State::Waiting
                        } else {
                            State::Closed
                        };
                        return Ok(DriveResult::Loop).into();
                    }

                    return Ok(DriveResult::Loop).into();
                } else {
                    // This is a fault, we are expecting more body chunks and
                    // the SendStream was dropped.
                    warn!("SendStream dropped before sending all of the expected body");

                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Unexpected end of body",
                    ))
                    .into();
                }
            }
        }

        return Ok(DriveResult::Loop).into();
    }
}

// ***************** Helper to drive connection externally *************************

pub(crate) trait ServerDrive {
    fn poll_drive_external(&self, cx: &mut Context<'_>) -> Result<(), io::Error>;
}

impl ServerDrive for Arc<Mutex<Codec>> {
    fn poll_drive_external(&self, cx: &mut Context<'_>) -> Result<(), io::Error> {
        let inner = self.clone();

        let mut lock = self.lock().unwrap();

        match lock.poll_drive(cx, false, inner) {
            Poll::Pending => {
                // this is ok, we have made max progress.const
                Ok(())
            }

            Poll::Ready(Some(Err(e))) => Err(e.into_io()),

            Poll::Ready(Some(Ok(_))) => {
                // invariant: we must not receive the next request here.
                unreachable!("Got next request in poll_drive_external")
            }

            // State::Waiting
            Poll::Ready(None) => Ok(()),
        }
    }
}

impl fmt::Debug for SendResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SendResponse")
    }
}

// ***************** Boiler plate to hide IO behind a Box<dyn trait> ***************

trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

struct IoAdapt<S>(S);

impl<S> Io for IoAdapt<S> where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<S> AsyncRead for IoAdapt<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut this.0).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for IoAdapt<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut this.0).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.0).poll_flush(cx)
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.0).poll_close(cx)
    }
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Closed => write!(f, "Closed")?,
            State::Waiting => write!(f, "Waiting")?,
            State::RecvReq => write!(f, "RecvReq")?,
            State::SendRes(b) => write!(
                f,
                "SendRes done_req_body: {}, done_response: {}",
                b.done_req_body, b.done_response
            )?,
            State::SendBody(b) => write!(f, "SendBody: ended: {}", b.ended)?,
        }
        Ok(())
    }
}

impl<S> std::fmt::Debug for Connection<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", "Connection")
    }
}
