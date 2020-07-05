use crate::http11::{poll_for_crlfcrlf, try_parse_req, write_http11_res};
use crate::limit::{LimitRead, LimitWrite};
use crate::Error;
use crate::RecvStream;
use crate::SendStream;
use crate::{AsyncRead, AsyncWrite};
use futures_channel::{mpsc, oneshot};
use futures_util::future::poll_fn;
use futures_util::ready;
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

/// "handshake" to create a connection.
///
/// This call is a bit odd, but it's to mirror the h2 crate.
pub fn handshake<S>(io: S) -> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    Connection(Arc::new(Mutex::new(Codec::new(io))), PhantomData)
}

/// Server connection for accepting incoming requests.
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

        lock.poll_drive(cx, inner)
    }

    /// Accept a new incoming request to handle. One must accept new requests continuously
    /// to "drive" the connection forward, also for the already accepted requests.
    pub async fn accept(
        &mut self,
    ) -> Option<Result<(http::Request<RecvStream>, SendResponse), Error>> {
        poll_fn(|cx| Pin::new(&mut *self).poll_accept(cx)).await
    }
}

pub struct SendResponse {
    inner: Arc<Mutex<Codec>>,
    tx_res: oneshot::Sender<(http::Response<()>, bool, mpsc::Receiver<(Vec<u8>, bool)>)>,
}

impl SendResponse {
    pub fn send_response(
        self,
        response: http::Response<()>,
        end_of_stream: bool,
    ) -> Result<SendStream, Error> {
        // bounded to get back pressure
        let (tx_body, rx_body) = mpsc::channel(2);

        let limit = LimitWrite::from_headers(response.headers());

        let ended = limit.is_no_body() || end_of_stream;

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
    // buffer to receive next request into
    request_buf: Vec<u8>,
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
}

/// State where can both send a response and receive a request body, if appropriate.
struct Bidirect {
    limit: LimitRead,
    tx_body: mpsc::Sender<io::Result<Vec<u8>>>,
    rx_res: oneshot::Receiver<(http::Response<()>, bool, mpsc::Receiver<(Vec<u8>, bool)>)>,
    done_req_body: bool,
    done_response: bool,
    recv_buf: Vec<u8>,
    /// Placeholder used if we received a response but are not finished
    /// sending the request body.
    holder: Option<(bool, LimitWrite, Option<mpsc::Receiver<(Vec<u8>, bool)>>)>,
}

struct BodySender {
    rx_body: mpsc::Receiver<(Vec<u8>, bool)>,
    ended: bool,
}

enum DriveResult {
    /// Next request arrived.
    Request((http::Request<RecvStream>, SendResponse)),
    /// Loop the drive_server again.
    Loop,
}

impl Codec {
    fn new<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(io: S) -> Self {
        Codec {
            io: Box::new(IoAdapt(io)),
            state: State::Waiting,
            to_write: vec![],
            request_buf: vec![],
        }
    }

    pub(crate) fn poll_drive(
        &mut self,
        cx: &mut Context<'_>,
        inner: Arc<Mutex<Codec>>,
    ) -> Poll<Option<Result<(http::Request<RecvStream>, SendResponse), Error>>> {
        loop {
            // try write any bytes ready to be sent.
            self.try_write(cx)?;

            let ret = ready!(self.drive_state(cx, inner.clone()))?;
            match ret {
                DriveResult::Request(p) => {
                    return Poll::Ready(Some(Ok(p)));
                }

                DriveResult::Loop => {
                    continue;
                }
            }
        }
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

    fn drive_state(
        &mut self,
        cx: &mut Context<'_>,
        inner: Arc<Mutex<Codec>>,
    ) -> Poll<Result<DriveResult, io::Error>> {
        match &mut self.state {
            State::Waiting => {
                ready!(poll_for_crlfcrlf(cx, &mut self.request_buf, &mut self.io))?;

                // we got a full request header in buf
                self.state = State::RecvReq;
            }

            State::RecvReq => {
                // invariant: poll_for_crlfcrlf must have read a full request.
                let (req, size) =
                    try_parse_req(&self.request_buf)?.expect("Didn't read full request");

                // invariant: entire buffer should have been used up.
                assert_eq!(self.request_buf.len(), size);

                // reset for next request
                self.request_buf.resize(0, 0);

                // Limiter to read the correct body amount from the socket.
                let limit = LimitRead::from_headers(req.headers());

                // bound channel to get backpressure
                let (tx_body, rx_body) = mpsc::channel(2);

                let (tx_res, rx_res) = oneshot::channel();

                // Prepare the new "package" to be delivered out of the poll loop.
                let package = {
                    let recv = RecvStream::new(rx_body, Some(inner.clone()));

                    let (parts, _) = req.into_parts();
                    let req = http::Request::from_parts(parts, recv);

                    let send = SendResponse { inner, tx_res };

                    (req, send)
                };

                let done_req_body = limit.is_no_body();

                self.state = State::SendRes(Bidirect {
                    limit,
                    tx_body,
                    rx_res,
                    done_req_body,
                    done_response: false,
                    recv_buf: Vec::with_capacity(READ_BUF_INIT_SIZE),
                    holder: None,
                });

                // Exit drive with the packet.
                return Ok(DriveResult::Request(package)).into();
            }

            State::SendRes(h) => {
                let mut req_body_pending = false;

                if !h.done_req_body {
                    if !h.recv_buf.is_empty() {
                        if let Err(_) = ready!(h.tx_body.poll_ready(cx)) {
                            // The RecvStream is dropped, that's ok, we continue
                            // to drive the connection.
                            trace!("Failed to receive body chunk RecvStream is dropped");
                        }

                        let chunk =
                            mem::replace(&mut h.recv_buf, Vec::with_capacity(READ_BUF_INIT_SIZE));

                        // The RecvStream might be dropped, that's ok.
                        h.tx_body.start_send(Ok(chunk)).ok();
                    }

                    // invariant: there should be nothing in the buffer now.
                    assert!(h.recv_buf.is_empty());

                    self.request_buf.resize(READ_BUF_INIT_SIZE, 0);

                    match h.limit.poll_read(cx, &mut self.io, &mut h.recv_buf) {
                        Poll::Pending => {
                            // Pending is ok, we can still make progress on sending the response.
                            req_body_pending = true;
                        }

                        Poll::Ready(r) => {
                            // read error?
                            let amount = r?;

                            // size down to read amount.
                            h.recv_buf.resize(amount, 0);

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
                    h.done_response = true;

                    // invariant: there should be nothing to send now.
                    assert!(self.to_write.is_empty());

                    // invariant: we should be able to write _any_ response.
                    let amount =
                        write_http11_res(&res, &mut self.to_write).expect("Write http::Response");

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
                        self.state = State::Waiting;
                    } else if let Some(rx_body) = rx_body {
                        self.state = State::SendBody(BodySender {
                            rx_body,
                            ended: false,
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
                // if there is a chunk to write, we will wait until it's written.
                if !self.to_write.is_empty() {
                    return Poll::Pending;
                }

                if b.ended {
                    self.state = State::Waiting;
                    return Ok(DriveResult::Loop).into();
                }

                let next = ready!(Pin::new(&mut b.rx_body).poll_next(cx));

                if let Some((chunk, end)) = next {
                    if end {
                        b.ended = true;
                    }

                    // queue up next chunk to write out.
                    self.to_write = chunk;

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

        match lock.poll_drive(cx, inner) {
            Poll::Pending => {
                // this is ok, we have made max progress.const
                Ok(())
            }

            Poll::Ready(Some(Err(e))) => Err(e.into_io()),

            x @ _ => {
                // invariant: any other return state than an error
                //            should not happen.
                unreachable!("Unexpected return from poll_drive: {:?}", x)
            }
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
