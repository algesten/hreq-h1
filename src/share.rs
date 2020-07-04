use crate::limit::LimitWrite;
use crate::server::Codec;
use crate::server::ServerDrive;
use crate::Error;
use futures_channel::mpsc;
use futures_util::future::poll_fn;
use futures_util::ready;
use futures_util::stream::Stream;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// Send some body data to a remote peer.
///
/// Obtained either via a [`client::SendRequest`] or a [`server::SendResponse`].
///
/// [`client::SendRequest`]: client/struct.SendRequest.html
/// [`server::SendResponse`]: server/struct.SendResponse.html
pub struct SendStream {
    tx_body: mpsc::Sender<(Vec<u8>, bool)>,
    limit: LimitWrite,
    ended: bool,
    // used in RecvStream originating in server to drive the connection
    // from the RecvStream polling itelf.
    server_inner: Option<Arc<Mutex<Codec>>>,
}

impl SendStream {
    pub(crate) fn new(
        tx_body: mpsc::Sender<(Vec<u8>, bool)>,
        limit: LimitWrite,
        ended: bool,
        server_inner: Option<Arc<Mutex<Codec>>>,
    ) -> Self {
        SendStream {
            tx_body,
            limit,
            ended,
            server_inner,
        }
    }

    /// Poll for whether this connection is ready to send more data without blocking.
    pub fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
        let this = self.get_mut();

        // must drive the connection if server.
        if let Some(server_inner) = &this.server_inner {
            server_inner.poll_drive_external(cx)?;
        }

        ready!(Pin::new(&mut this.tx_body).poll_ready(cx))
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e))?;

        Ok(()).into()
    }

    /// Test whether connection is ready to send more data. The call stalls until
    /// any previous data provided in `send_data()` has been transfered to the remote
    /// peer (or at least in a buffer). As such, this can form part of a flow control.
    pub async fn ready(mut self) -> Result<SendStream, Error> {
        poll_fn(|cx| Pin::new(&mut self).poll_ready(cx)).await?;
        Ok(self)
    }

    /// Send some body data.
    ///
    /// `end` controls whether this is the last body chunk to send. It's an error
    /// to send more data after `end` is `true`.
    pub fn poll_send_data(
        self: Pin<&mut Self>,
        cx: &mut Context,
        data: &[u8],
        end: bool,
    ) -> Poll<Result<(), Error>> {
        let this = self.get_mut();

        if this.ended {
            return Err(Error::User("Body data is not expected".into())).into();
        }

        // must drive the connection if server.
        if let Some(server_inner) = &this.server_inner {
            server_inner.poll_drive_external(cx)?;
        }

        let mut chunk = Vec::with_capacity(data.len() + this.limit.overhead());
        this.limit.write(data, &mut chunk)?;

        if end {
            this.ended = true;
            this.limit.finish(&mut chunk)?;
        }

        this.tx_body
            .start_send((chunk, end))
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e))?;

        Ok(()).into()
    }

    pub async fn send_data(&mut self, data: &[u8], end: bool) -> Result<(), Error> {
        poll_fn(|cx| Pin::new(&mut *self).poll_send_data(cx, data, end)).await
    }
}

/// Receives a body from the remote peer.
///
/// Obtained from either a [`client::ResponseFuture`] or [`server::Connection`].
///
/// [`client::ResponseFuture`]: client/struct.ResponseFuture.html
/// [`server::Connection`]: server/struct.Connection.html
pub struct RecvStream {
    rx_body: mpsc::Receiver<io::Result<Vec<u8>>>,
    ready: Option<Vec<u8>>,
    index: usize,
    // used in RecvStream originating in server to drive the connection
    // from the RecvStream polling itelf.
    server_inner: Option<Arc<Mutex<Codec>>>,
}

impl RecvStream {
    pub(crate) fn new(
        rx_body: mpsc::Receiver<io::Result<Vec<u8>>>,
        server_inner: Option<Arc<Mutex<Codec>>>,
    ) -> Self {
        RecvStream {
            rx_body,
            ready: None,
            index: 0,
            server_inner,
        }
    }

    /// Read some body data in an async way.
    pub fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        // must drive the connection if server.
        if let Some(server_inner) = &this.server_inner {
            server_inner.poll_drive_external(cx)?;
        }

        loop {
            // First ship out ready data already received.
            if let Some(ready) = &this.ready {
                let i = this.index;

                let max = buf.len().min(ready.len() - i);

                (&mut buf[0..max]).copy_from_slice(&ready[i..(i + max)]);
                this.index += max;

                if this.index == ready.len() {
                    // all used up
                    this.ready.take();
                }

                return Ok(max).into();
            }

            // invariant: Should be no ready bytes if we're here.
            assert!(this.ready.is_none());

            match ready!(Pin::new(&mut this.rx_body).poll_next(cx)) {
                None => {
                    // Channel is closed which indicates end of body.
                    return Ok(0).into();
                }
                Some(v) => {
                    // nested io::Error
                    let v = v?;

                    this.ready = Some(v);
                    this.index = 0;
                }
            }
        }
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        Ok(poll_fn(move |cx| Pin::new(&mut *self).poll_read(cx, buf)).await?)
    }

    pub fn is_end(&self) -> bool {
        todo!()
    }
}

impl fmt::Debug for SendStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SendStream")
    }
}

impl fmt::Debug for RecvStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RecvStream")
    }
}
