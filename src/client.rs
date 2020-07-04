use crate::err_closed;
use crate::http11::{poll_for_crlfcrlf, try_parse_res, write_http11_req};
use crate::limit::{LimitRead, LimitWrite};
use crate::share::Seq;
use crate::Error;
use crate::{AsyncRead, AsyncWrite};
use crate::{RecvStream, SendStream};
use futures_channel::{mpsc, oneshot};
use futures_util::ready;
use futures_util::stream::Stream;
use std::future::Future;
use std::io;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Size of buffer reading response body into.
const READ_BUF_INIT_SIZE: usize = 16_384;

/// Creates a new HTTP/1 client backed by some async `io` connection.
///
/// Returns a handle to send requests and a connection tuple. The connection
/// is a future that must be polled to "drive" the client forward.
pub fn handshake<S>(io: S) -> (SendRequest, Connection<S>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (req_tx, req_rx) = mpsc::channel(100);

    let send_req = SendRequest::new(req_tx);

    let conn = Connection(Codec::new(io, req_rx));

    (send_req, conn)
}

/// Sender of new requests.
#[derive(Clone)]
pub struct SendRequest {
    next_seq: Seq,
    req_tx: mpsc::Sender<ReqHandle>,
}

/// Internal holder of all details for a new request.
///
/// This internally communicates with the `Connection`.
struct ReqHandle {
    seq: Seq,
    req: http::Request<()>,
    no_send_body: bool,
    body_rx: mpsc::Receiver<(Vec<u8>, bool)>,
    res_tx: Option<oneshot::Sender<io::Result<http::Response<RecvStream>>>>,
}

impl SendRequest {
    fn new(req_tx: mpsc::Sender<ReqHandle>) -> Self {
        SendRequest {
            next_seq: Seq(0),
            req_tx,
        }
    }

    /// Send a new request.
    ///
    /// The nature of HTTP/1 means only one request can be sent at a time (no multiplexing).
    /// Each request sent before the next has finished will be queued.
    ///
    /// The `end` argument indiciates there is no body to be sent. The returned `SendStream`
    /// will accept body data unless `end` is `true`.
    ///
    /// Errors if the connection is closed.
    pub fn send_request(
        &mut self,
        req: http::Request<()>,
        end: bool,
    ) -> Result<(ResponseFuture, SendStream), Error> {
        // Grab sequence number.
        let seq = self.next_seq.inc();

        // Channel to send response back.
        let (res_tx, res_rx) = oneshot::channel();

        // bounded so we provide backpressure if socket is full.
        let (body_tx, body_rx) = mpsc::channel(2);

        let limit = LimitWrite::from_headers(req.headers());

        // The handle for the codec/connection.
        let next = ReqHandle {
            seq,
            req,
            no_send_body: end,
            body_rx,
            res_tx: Some(res_tx),
        };

        if self.req_tx.try_send(next).is_err() {
            // errors on full or closed, and since it's unbound...
            return err_closed();
        }

        let fut = ResponseFuture(res_rx);
        let send = SendStream::new(body_tx, limit, end);

        Ok((fut, send))
    }
}

/// Future for a `http::Response<RecvStream>>`
pub struct ResponseFuture(oneshot::Receiver<io::Result<http::Response<RecvStream>>>);

impl Future for ResponseFuture {
    type Output = Result<http::Response<RecvStream>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        let res = ready!(Pin::new(&mut this.0).poll(cx));

        if let Ok(v) = res {
            // nested io::Error
            let v = v?;

            Ok(v).into()
        } else {
            err_closed().into()
        }
    }
}

pub struct Connection<S>(Codec<S>);

impl<S> Future for Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.0.poll_drive(cx)
    }
}

struct Codec<S> {
    io: S,
    req_rx: mpsc::Receiver<ReqHandle>,
    to_write: Vec<u8>,
    state: State,
}

enum State {
    /// Waiting for the next request.
    Waiting,
    /// Send request.
    SendReq(ReqHandle),
    /// Receive response and (if appropriate), send request body.
    RecvRes(Bidirect),
    /// Receive response body.
    RecvBody(BodyReceiver),
    /// Placeholder
    Empty,
}

use std::fmt;

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Waiting => write!(f, "Waiting")?,
            State::SendReq(h) => write!(f, "SendReq({:?}): {:?}", h.seq, h.req)?,
            State::RecvRes(b) => write!(
                f,
                "RecvRes({:?}) done_req_body: {}, done_response: {}",
                b.handle.seq, b.done_req_body, b.done_response
            )?,
            State::RecvBody(r) => write!(f, "RecvBody({:?})", r.handle.seq)?,
            State::Empty => write!(f, "Empty")?,
        }
        Ok(())
    }
}

impl State {
    /// Take the ReqHandle from the state, leave placeholder State::Empty in place.
    fn take_handle(&mut self) -> ReqHandle {
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
}

/// Bidirection state. Receive response as well as send request body (if appropriate).
struct Bidirect {
    handle: ReqHandle,
    /// If we are finished sending request body.
    done_req_body: bool,
    /// If we are finished receiving the response.
    done_response: bool,
    /// Buffer to read response into.
    response_buf: Vec<u8>,
    /// Placeholder used if we received a response but are not finished
    /// sending the request body.
    holder: Option<(mpsc::Sender<io::Result<Vec<u8>>>, LimitRead)>,
}

/// Receiver of response body.
struct BodyReceiver {
    handle: ReqHandle,
    limit: LimitRead,
    tx_body: mpsc::Sender<io::Result<Vec<u8>>>,
    recv_buf: Vec<u8>,
}

impl<S> Codec<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn new(io: S, req_rx: mpsc::Receiver<ReqHandle>) -> Self {
        Codec {
            io,
            req_rx,
            to_write: vec![],
            state: State::Waiting,
        }
    }

    fn poll_drive(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            // first try to write queued outgoing bytes,
            self.try_write(cx)?;

            // then drive state forward
            match ready!(self.drive_state(cx)) {
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

                    // In some states we can "bubble" the error to API side.
                    match &mut self.state {
                        State::SendReq(h) => {
                            if let Some(res_tx) = h.res_tx.take() {
                                res_tx.send(Err(clone)).ok();
                            }
                        }
                        State::RecvRes(b) => {
                            if let Some(res_tx) = b.handle.res_tx.take() {
                                res_tx.send(Err(clone)).ok();
                            } else if let Some((mut tx_body, _)) = b.holder.take() {
                                if let Err(_) = tx_body.try_send(Err(clone)) {
                                    debug!("Failed to notify RecvStream about error");
                                }
                            }
                        }
                        State::RecvBody(r) => {
                            if let Err(_) = r.tx_body.try_send(Err(clone)) {
                                debug!("Failed to notify RecvStream about error");
                            }
                        }
                        State::Waiting | State::Empty => {}
                    }

                    // the actual error goes to the connection.
                    return Err(e).into();
                }
            }
        }

        Ok(()).into()
    }

    /// Try write outgoing bytes.
    fn try_write(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.to_write.is_empty() {
            return Ok(());
        }

        trace!("try_write left: {}", self.to_write.len());

        let poll = Pin::new(&mut self.io).poll_write(cx, &self.to_write);

        match poll {
            Poll::Pending => {
                // Pending is fine. It means the socket is full upstream, we can still
                // progress the downstream (i.e. drive_state()).
                trace!("try_write: Poll::Pending");
            }

            // We managed to write some.
            Poll::Ready(Ok(amount)) => {
                trace!("try_write did write: {}", amount);
                // TODO: some more efficient buffer?
                self.to_write = self.to_write.split_off(amount);
            }

            Poll::Ready(Err(e)) => {
                trace!("try_write error: {:?}", e);
                return Err(e).into();
            }
        }

        Ok(())
    }

    fn drive_state(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
        trace!("drive_state: {:?}", self.state);

        match &mut self.state {
            State::Empty => {
                // invariant: Empty is just a placeholder.
                panic!("State::Empty in drive_state");
            }

            State::Waiting => {
                // try get the next request.
                let next = ready!(Pin::new(&mut self.req_rx).poll_next(cx));

                if let Some(h) = next {
                    trace!("Got next handle: {:?}", h.seq);
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

                if let Err(e) = write_http11_req(&h.req, &mut self.to_write) {
                    return Err(e).into();
                }

                // if we don't expect a request body, we mark the next state as being
                // done for send body already.
                let done_req_body = h.no_send_body;

                let handle = self.state.take_handle();

                self.state = State::RecvRes(Bidirect {
                    handle,
                    done_req_body,
                    done_response: false,
                    response_buf: vec![],
                    holder: None,
                });
            }

            State::RecvRes(b) => {
                if !b.done_req_body {
                    // Not done sending a request body. Try get a body chunk to send.
                    match Pin::new(&mut b.handle.body_rx).poll_next(cx) {
                        Poll::Pending => {
                            // Pending is ok, it means the SendBody
                        }

                        Poll::Ready(Some((mut chunk, end))) => {
                            // Got a chunk to send
                            if self.to_write.is_empty() {
                                self.to_write = chunk;
                            } else {
                                self.to_write.append(&mut chunk);
                            }

                            // Sender signalled end of stream
                            if end {
                                b.done_req_body = true;
                            }
                        }

                        Poll::Ready(None) => {
                            // No more body chunks to be expected, SendBody was dropped.
                            b.done_req_body = true;
                        }
                    }
                }

                if !b.done_response {
                    ready!(poll_for_crlfcrlf(cx, &mut b.response_buf, &mut self.io))?;

                    let res = try_parse_res(&b.response_buf)?;

                    // invariant: poll_for_crlfcrlf should provide a full header and
                    //            try_parse_res should not be able to get a partial response.
                    let (res, size) = res.expect("Parsed partial response");

                    // invariant: all bytes should have been used up
                    assert_eq!(b.response_buf.len(), size);

                    // we have a response for sure.
                    b.done_response = true;

                    let limit = LimitRead::from_headers(res.headers());

                    // bounded to have backpressure if client is reading slowly.
                    let (tx_body, rx_body) = mpsc::channel(2);

                    // holder indicates whether we expect a body.
                    b.holder = if limit.is_no_body() {
                        None
                    } else {
                        Some((tx_body, limit))
                    };

                    let recv = RecvStream::new(rx_body);

                    let (parts, _) = res.into_parts();
                    let res = http::Response::from_parts(parts, recv);

                    // invariant: the oneshot handle should only exist once.
                    let res_tx = b.handle.res_tx.take().expect("Missing res_tx");

                    if let Err(_) = res_tx.send(Ok(res)) {
                        // res_tx is unbounded, the only error possible is that the
                        // response future is dropped and client is not interested in response.
                        // This is not an error, we continue to drive the connection.
                        trace!("Failed to send http::Response to ResponseFuture");
                    }
                }

                // only proceed out of this state if we have both finished sending a request
                // body and received a response header.
                if b.done_req_body && b.done_response {
                    if let Some((tx_body, limit)) = b.holder.take() {
                        // expect a response body.
                        let handle = self.state.take_handle();

                        self.state = State::RecvBody(BodyReceiver {
                            handle,
                            tx_body,
                            limit,
                            recv_buf: Vec::with_capacity(READ_BUF_INIT_SIZE),
                        });
                    } else {
                        // expect no response body.
                        self.state = State::Waiting;
                    }
                }
            }

            State::RecvBody(r) => {
                if !r.recv_buf.is_empty() {
                    // we got a chunk read to send off to the RecvStream
                    if let Err(_) = ready!(r.tx_body.poll_ready(cx)) {
                        // Receiver is gone. We continue receving to get
                        // connection in a good state for next request.
                    }

                    let chunk =
                        mem::replace(&mut r.recv_buf, Vec::with_capacity(READ_BUF_INIT_SIZE));

                    // Since we poll_ready above, the error here is that the receiver is gone,
                    // which isn't a problem.
                    r.tx_body.start_send(Ok(chunk)).ok();
                }

                // invariant: if we're here, the recv_buffer must be empty.
                assert!(r.recv_buf.is_empty());

                // TODO: maybe increase this buffer size if it's fully used?
                r.recv_buf.resize(READ_BUF_INIT_SIZE, 0);

                // read self.io through the limiter to stop reading when we are
                // in place for the next request.
                let amount = ready!(r.limit.poll_read(cx, &mut self.io, &mut r.recv_buf))?;

                if amount > 0 {
                    // scale down buffer to read amount and loop to send off.
                    r.recv_buf.resize(amount, 0);
                } else {
                    // no more response body. ready to handle next request.
                    // NB. This drops the r.tx_body which means the RecvStream will
                    // read a 0 amount on next try.
                    self.state = State::Waiting;
                }
            }
        }

        Ok(true).into()
    }
}
