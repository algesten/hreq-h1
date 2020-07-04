use crate::limit::LimitWrite;
use crate::Error;
use futures_channel::mpsc;
use futures_util::future::poll_fn;
use futures_util::ready;
use futures_util::stream::Stream;
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
    limit: LimitWrite,
    body_tx: mpsc::Sender<(Vec<u8>, bool)>,
    ended: bool,
}

impl SendStream {
    pub(crate) fn new(
        body_tx: mpsc::Sender<(Vec<u8>, bool)>,
        limit: LimitWrite,
        ended: bool,
    ) -> Self {
        SendStream {
            body_tx,
            limit,
            ended,
        }
    }

    /// Poll for whether this connection is ready to send more data without blocking.
    pub fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
        let this = self.get_mut();

        ready!(Pin::new(&mut this.body_tx).poll_ready(cx))
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
    /// The data is enqueued to be sent without checking whether there is already data
    /// being handled not yet sent to the remote side. This could lead to holding lots
    /// of data in memory. To avoid that, use `ready()` to be notified when the previous
    /// `send_data()` call is finished transfering.
    ///
    /// `end` controls whether this is the last body chunk to send. It's an error
    /// to send more data after `end` is `true`.
    pub fn send_data(&mut self, data: &[u8], end: bool) -> Result<(), Error> {
        if self.ended {
            return Err(Error::User("Body data is not expected".into()));
        }

        let mut chunk = Vec::with_capacity(data.len() + self.limit.overhead());
        self.limit.write(data, &mut chunk)?;

        if end {
            self.ended = true;
            self.limit.finish(&mut chunk)?;
        }

        self.body_tx
            .start_send((chunk, end))
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e))?;

        Ok(())
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
}

impl RecvStream {
    pub(crate) fn new(rx_body: mpsc::Receiver<io::Result<Vec<u8>>>) -> Self {
        RecvStream {
            rx_body,
            ready: None,
            index: 0,
        }
    }

    /// Read some body data in an async way.
    pub fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

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

//                Ready  <--------
//                  |            |
//                  v            |
//             Bidirection       |
//               /     \         |
//              v       v        |
//         RecvBody    Waiting   |
//               \     /         |
//                v   v          |
//               SendBody --------
