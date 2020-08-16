use crate::limit::LimitWrite;
use crate::mpsc::{Receiver, Sender};
use crate::server::DriveExternal;
use crate::AsyncRead;
use crate::Error;
use futures_util::future::poll_fn;
use futures_util::ready;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Send some body data to a remote peer.
///
/// Obtained either via a [`client::SendRequest`] or a [`server::SendResponse`].
///
/// [`client::SendRequest`]: client/struct.SendRequest.html
/// [`server::SendResponse`]: server/struct.SendResponse.html
pub struct SendStream {
    tx_body: Sender<(Vec<u8>, bool)>,
    limit: LimitWrite,
    ended: bool,
    drive_external: Option<Box<dyn DriveExternal>>,
}

impl SendStream {
    pub(crate) fn new(
        tx_body: Sender<(Vec<u8>, bool)>,
        limit: LimitWrite,
        ended: bool,
        drive_external: Option<Box<dyn DriveExternal>>,
    ) -> Self {
        SendStream {
            tx_body,
            limit,
            ended,
            drive_external,
        }
    }

    /// Send one chunk of data. Use `end_of_body` to signal end of data.
    ///
    /// Alternate calls to this with calls to `ready` for flow control.
    ///
    /// When the body is constrained by a `content-length` header, this will only accept
    /// the amount of bytes specified in the header. If there is too much data, the
    /// function will error with a `Error::User`.
    ///
    /// For `transfer-encoding: chunked`, call to this function corresponds to one "chunk".
    #[instrument(skip(self, data, end_of_body))]
    pub async fn send_data(&mut self, data: &[u8], end_of_body: bool) -> Result<(), Error> {
        trace!("Send len={} end_of_body={}", data.len(), end_of_body);
        poll_fn(|cx| self.poll_drive_server(cx)).await?;
        poll_fn(|cx| Pin::new(&mut *self).poll_send_data(cx, data, end_of_body)).await?;
        poll_fn(|cx| self.poll_drive_server(cx)).await?;

        Ok(())
    }

    #[instrument(skip(self, cx))]
    fn poll_drive_server(&mut self, cx: &mut Context) -> Poll<Result<(), Error>> {
        if let Some(drive_external) = &self.drive_external {
            drive_external.poll_drive_external(cx)
        } else {
            Ok(()).into()
        }
    }

    /// Send some body data.
    ///
    /// `end` controls whether this is the last body chunk to send. It's an error
    /// to send more data after `end` is `true`.
    #[instrument(skip(self, cx, data, end))]
    fn poll_send_data(
        self: Pin<&mut Self>,
        cx: &mut Context,
        data: &[u8],
        end: bool,
    ) -> Poll<Result<(), Error>> {
        let this = self.get_mut();

        if this.ended && end && data.is_empty() {
            // this is a noop
            return Ok(()).into();
        }

        if this.ended {
            warn!("Body data is not expected");
            return Err(Error::User("Body data is not expected".into())).into();
        }

        if !ready!(Pin::new(&this.tx_body).poll_ready(cx, true)) {
            return Err(
                io::Error::new(io::ErrorKind::ConnectionAborted, "Connection closed").into(),
            )
            .into();
        }

        let mut chunk = Vec::with_capacity(data.len() + this.limit.overhead());
        this.limit.write(data, &mut chunk)?;

        if end {
            this.ended = true;
            this.limit.finish(&mut chunk)?;
        }

        let sent = this.tx_body.send((chunk, end));

        if !sent {
            return Err(
                io::Error::new(io::ErrorKind::ConnectionAborted, "Connection closed").into(),
            )
            .into();
        }

        Ok(()).into()
    }
}

/// Receives a body from the remote peer.
///
/// Obtained from either a [`client::ResponseFuture`] or [`server::Connection`].
///
/// [`client::ResponseFuture`]: client/struct.ResponseFuture.html
/// [`server::Connection`]: server/struct.Connection.html
pub struct RecvStream {
    rx_body: Receiver<io::Result<Vec<u8>>>,
    ready: Option<Vec<u8>>,
    index: usize,
    ended: bool,
    drive_external: Option<Box<dyn DriveExternal>>,
}

impl RecvStream {
    pub(crate) fn new(
        rx_body: Receiver<io::Result<Vec<u8>>>,
        ended: bool,
        drive_external: Option<Box<dyn DriveExternal>>,
    ) -> Self {
        RecvStream {
            rx_body,
            ready: None,
            index: 0,
            ended,
            drive_external,
        }
    }

    /// Read some body data into a given buffer.
    ///
    /// Ends when returned size is `0`.
    #[instrument(skip(self, buf))]
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        poll_fn(|cx| self.poll_drive_server(cx)).await?;
        Ok(poll_fn(move |cx| Pin::new(&mut *self).poll_read(cx, buf)).await?)
    }

    /// Returns `true` if there is no more data to receive.
    ///
    /// Specifically any further call to `read` will result in `0` bytes read.
    pub fn is_end_stream(&self) -> bool {
        self.ended
    }

    #[instrument(skip(self, cx))]
    fn poll_drive_server(&mut self, cx: &mut Context) -> Poll<Result<(), Error>> {
        if let Some(drive_external) = &self.drive_external {
            drive_external.poll_drive_external(cx)
        } else {
            Ok(()).into()
        }
    }

    #[doc(hidden)]
    /// Poll for some body data.
    #[instrument(skip(self, cx, buf))]
    fn poll_body_data(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if this.ended {
            return Ok(0).into();
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

            match ready!(Pin::new(&mut this.rx_body).poll_recv(cx, true)) {
                None => {
                    // Channel is closed which indicates end of body.
                    this.ended = true;
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
}

impl AsyncRead for RecvStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_body_data(cx, buf)
    }
}

impl fmt::Debug for SendStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SendStream")
    }
}

impl fmt::Debug for RecvStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RecvStream")
    }
}
