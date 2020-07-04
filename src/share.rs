use crate::Error;
use futures_util::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Receives a body from the remote peer.
///
/// Obtained from either a [`client::ResponseFuture`] or [`server::Connection`].
///
/// [`client::ResponseFuture`]: client/struct.ResponseFuture.html
/// [`server::Connection`]: server/struct.Connection.html
pub struct RecvStream {}

impl RecvStream {
    fn new() -> Self {
        todo!()
    }

    /// Read some body data in an async way.
    pub fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        todo!()
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        Ok(poll_fn(move |cx| Pin::new(&mut *self).poll_read(cx, buf)).await?)
    }

    pub fn is_end(&self) -> bool {
        todo!()
    }
}

/// States the connection can be in.
///
/// A client sending h1 requests:
///
/// ```text
///                Ready  <--------
///                  |            |
///                  v            |
///             Bidirection       |
///               /     \         |
///              v       v        |
///         SendBody    Waiting   |
///               \     /         |
///                v   v          |
///               RecvBody --------
/// ```
///
/// A server receiving h1 requests:
///
/// ```text
///                Ready  <--------
///                  |            |
///                  v            |
///             Bidirection       |
///               /     \         |
///              v       v        |
///         RecvBody    Waiting   |
///               \     /         |
///                v   v          |
///               SendBody --------
/// ```
// #[derive(Clone, Copy, Eq, PartialEq, Debug)]
// pub enum State {
//     /// Send a new request (client), waiting to receive request (server).
//     Ready,
//     /// Send body or receive response (client), receive body or send response (server).
//     Bidirection,
//     /// Response received and need to send body (client), need to send body (server).
//     SendBody,
//     /// Body sent and need to receive response (client), received body and need to send response (server).
//     Waiting,
//     /// Need to receive response body (client), response sent and need to receive body (server).
//     RecvBody,
//     /// Connection failed or closed.
//     Closed,
// }

/// Send some body data to a remote peer.
///
/// Obtained either via a [`client::SendRequest`] or a [`server::SendResponse`].
///
/// [`client::SendRequest`]: client/struct.SendRequest.html
/// [`server::SendResponse`]: server/struct.SendResponse.html
pub struct SendStream {}

impl SendStream {
    fn new() -> Self {
        todo!()
    }

    fn poll_can_send_data(&self, cx: &mut Context) -> Poll<Result<(), Error>> {
        todo!()
    }

    /// Test whether connection is ready to send more data. The call stalls until
    /// any previous data provided in `send_data()` has been transfered to the remote
    /// peer (or at least in a buffer). As such, this can form part of a flow control.
    pub async fn ready(self) -> Result<SendStream, Error> {
        poll_fn(|cx| self.poll_can_send_data(cx)).await?;
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
        todo!()
    }
}
