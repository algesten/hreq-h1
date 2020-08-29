use crate::fast_buf::ConsumeBuf;
use crate::fast_buf::FastBuf;
use crate::limit::LimitWrite;
use crate::mpsc::{Receiver, Sender};
use crate::server::{DriveExternal, SyncDriveExternal};
use crate::AsyncRead;
use crate::Error;
use futures_util::future::poll_fn;
use futures_util::ready;
use std::fmt;
use std::io;
use std::mem;
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
    drive_external: Option<SyncDriveExternal>,
}

impl SendStream {
    pub(crate) fn new(
        tx_body: Sender<(Vec<u8>, bool)>,
        limit: LimitWrite,
        ended: bool,
        drive_external: Option<SyncDriveExternal>,
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
    /// When the body is constrained by a `content-length` header, this will only accept
    /// the amount of bytes specified in the header. If there is too much data, the
    /// function will error with a `Error::User`.
    ///
    /// For `transfer-encoding: chunked`, call to this function corresponds to one "chunk".
    pub async fn send_data(&mut self, data: &[u8], end_of_body: bool) -> Result<(), Error> {
        let data = Data::Shared(data);

        self.do_send(data, end_of_body).await?;

        Ok(())
    }

    /// Send one chunk of data. Use `end_of_body` to signal end of data.
    ///
    /// This is an optimization which together with a `content-length` shortcuts
    /// some unnecessary copying of data.
    ///
    /// When the body is constrained by a `content-length` header, this will only accept
    /// the amount of bytes specified in the header. If there is too much data, the
    /// function will error with a `Error::User`.
    ///
    /// For `transfer-encoding: chunked`, call to this function corresponds to one "chunk".
    pub async fn send_data_owned(&mut self, data: Vec<u8>, end_of_body: bool) -> Result<(), Error> {
        let data = Data::Owned(data);

        self.do_send(data, end_of_body).await?;

        Ok(())
    }

    async fn do_send(&mut self, mut data: Data<'_>, end_of_body: bool) -> Result<(), Error> {
        trace!("Send len={} end_of_body={}", data.len(), end_of_body);

        poll_fn(|cx| self.poll_drive_server(cx)).await?;
        poll_fn(|cx| Pin::new(&mut *self).poll_send_data(cx, &mut data, end_of_body)).await?;
        poll_fn(|cx| self.poll_drive_server(cx)).await?;

        Ok(())
    }

    fn poll_drive_server(&mut self, cx: &mut Context) -> Poll<Result<(), io::Error>> {
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
    fn poll_send_data(
        self: Pin<&mut Self>,
        cx: &mut Context,
        data: &mut Data,
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

        let to_send = if data.is_owned() && this.limit.can_write_entire_vec() {
            // This is an optmization when sending owned data. We can pass the
            // Vec<u8> straight into the this.tx_body.send without copying the
            // data into a FastBuf first.

            let data = data.take_owned();

            // so limit counters are correct
            this.limit.accept_entire_vec(&data);

            data
        } else {
            // This branch handles shared data as well as chunked body transfer.

            let capacity = data.len() + this.limit.overhead();
            let mut chunk = FastBuf::with_capacity(capacity);
            this.limit.write(&data[..], &mut chunk)?;

            if end {
                this.ended = true;
                this.limit.finish(&mut chunk)?;
            }

            chunk.into_vec()
        };

        let sent = this.tx_body.send((to_send, end));

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
    ready: Option<ConsumeBuf>,
    ended: bool,
    drive_external: Option<SyncDriveExternal>,
}

impl RecvStream {
    pub(crate) fn new(
        rx_body: Receiver<io::Result<Vec<u8>>>,
        ended: bool,
        drive_external: Option<SyncDriveExternal>,
    ) -> Self {
        RecvStream {
            rx_body,
            ready: None,
            ended,
            drive_external,
        }
    }

    /// Read some body data into a given buffer.
    ///
    /// Ends when returned size is `0`.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        Ok(poll_fn(move |cx| Pin::new(&mut *self).poll_read(cx, buf)).await?)
    }

    /// Returns `true` if there is no more data to receive.
    ///
    /// Specifically any further call to `read` will result in `0` bytes read.
    pub fn is_end_stream(&self) -> bool {
        self.ended
    }

    fn poll_drive_server(&mut self, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        if let Some(drive_external) = &self.drive_external {
            drive_external.poll_drive_external(cx)
        } else {
            Ok(()).into()
        }
    }

    #[doc(hidden)]
    /// Poll for some body data.
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
            if let Some(ready) = &mut this.ready {
                let max = buf.len().min(ready.len());
                (&mut buf[0..max]).copy_from_slice(&ready[..max]);

                ready.consume(max);

                if ready.is_empty() {
                    this.ready = None;
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
                    this.ready = Some(ConsumeBuf::new(v));
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
        let this = self.get_mut();

        // can't poll data with an empty buffer
        assert!(!buf.is_empty(), "poll_read with empty buf");

        ready!(this.poll_drive_server(cx))?;

        Pin::new(this).poll_body_data(cx, buf)
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

enum Data<'a> {
    Shared(&'a [u8]),
    Owned(Vec<u8>),
    Empty,
}

impl<'a> Data<'a> {
    fn is_owned(&self) -> bool {
        if let Data::Owned(_) = self {
            return true;
        }
        false
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Data::Shared(v) => v.is_empty(),
            Data::Owned(v) => v.is_empty(),
            Data::Empty => true,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Data::Shared(v) => v.len(),
            Data::Owned(v) => v.len(),
            Data::Empty => 0,
        }
    }

    pub fn take_owned(&mut self) -> Vec<u8> {
        if self.is_owned() {
            if let Data::Owned(v) = mem::replace(self, Data::Empty) {
                return v;
            }
        }
        panic!("Can't take_owned");
    }
}

impl<'a> std::ops::Deref for Data<'a> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        match self {
            Data::Shared(v) => &v[..],
            Data::Owned(v) => &v[..],
            Data::Empty => panic!("Can't deref a Data::Empty"),
        }
    }
}
