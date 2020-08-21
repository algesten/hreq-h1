use crate::fast_buf::{ConsumeBuf, FastBuf};
use crate::{AsyncRead, AsyncWrite};
use futures_util::ready;
use std::io;
use std::mem;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

#[derive(Debug)]
/// Our own BufReader.
///
/// The use AsyncBufRead in poll_for_crlfcrlf requires poll_fill_buf to
/// always fill from the underlying reader, also when there is content
/// buffered already.
pub struct BufIo<R> {
    inner: R,
    buf: FastBuf,
    pos: usize,
    pending_rx: bool,
    pending_tx: bool,
    write_buf: Option<ConsumeBuf>,
    need_flush: bool,
}

impl<R> BufIo<R>
where
    R: AsyncRead + AsyncWrite + Unpin,
{
    pub fn with_capacity(capacity: usize, inner: R) -> Self {
        BufIo {
            inner,
            buf: FastBuf::with_capacity(capacity),
            pos: 0,
            pending_rx: false,
            pending_tx: false,
            write_buf: None,
            need_flush: false,
        }
    }

    pub fn ensure_read_capacity(&mut self, capacity: usize) {
        self.buf.ensure_capacity(capacity);
    }

    pub fn pending_tx(&self) -> bool {
        self.pending_tx
    }

    pub fn pending_rx(&self) -> bool {
        self.pending_rx
    }
}

impl<R> BufIo<R>
where
    R: AsyncWrite + Unpin,
{
    #[instrument(skip(self, cx))]
    pub fn poll_finish_pending_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if let Some(buf) = &mut this.write_buf {
            loop {
                if buf.is_empty() {
                    break;
                }

                // we got stuff left to send
                let amount = match Pin::new(&mut this.inner).poll_write(cx, &buf[..]) {
                    Poll::Pending => {
                        trace!("poll_write: Pending");
                        this.pending_tx = true;
                        return Poll::Pending;
                    }
                    Poll::Ready(v) => {
                        trace!("poll_write: {:?}", v);
                        this.pending_tx = false;
                        v?
                    }
                };
                buf.consume(amount);
            }

            this.write_buf = None;
        }

        if this.need_flush {
            match Pin::new(&mut this.inner).poll_flush(cx) {
                Poll::Pending => {
                    trace!("poll_flush: Pending");
                    this.pending_tx = true;
                    return Poll::Pending;
                }
                Poll::Ready(v) => {
                    trace!("poll_write: {:?}", v);
                    this.pending_tx = false;
                    v?
                }
            }
            this.need_flush = false;
        }

        Ok(()).into()
    }

    /// Check that a poll_write definitely won't reject with Pending before accepting
    pub fn can_poll_write(&self) -> bool {
        self.write_buf.is_none() && !self.need_flush && !self.pending_tx
    }

    /// Write all or none of a buffer.
    ///
    /// This poll_write variant write the entire buf to the underlying writer or nothing,
    /// potentially using an internal buffer for half written responses when hitting Pending.
    #[instrument(skip(self, cx, buf, flush))]
    pub fn poll_write_all(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut Option<&[u8]>,
        flush: bool,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // Any pending writes must be dealt with first.
        ready!(Pin::new(&mut *this).poll_finish_pending_write(cx))?;

        assert!(this.write_buf.is_none());
        assert!(!this.need_flush);

        // Take ownership of the incoming buf. If we can't write it entirely we will
        // Allocate into a ConsumeBuf for the remainder.
        let buf = if let Some(buf) = buf.take() {
            buf
        } else {
            return Ok(()).into();
        };

        let mut pos = 0;

        loop {
            if pos == buf.len() {
                break;
            }

            match Pin::new(&mut this.inner).poll_write(cx, &buf[pos..]) {
                Poll::Pending => {
                    trace!("poll_write: Pending");
                    // Half sent buffer, the rest is for poll_finish_pending_write.
                    this.pending_tx = true;
                    this.write_buf = Some(ConsumeBuf::new((&buf[pos..]).to_vec()));
                    this.need_flush = flush;
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => {
                    trace!("poll_write err: {:?}", e);
                    this.pending_tx = false;
                    return Err(e).into();
                }
                Poll::Ready(Ok(amount)) => {
                    trace!("poll_write sent: {}", amount);
                    this.pending_tx = false;
                    pos += amount;
                }
            }
        }

        if flush {
            match Pin::new(&mut this.inner).poll_flush(cx) {
                Poll::Pending => {
                    trace!("poll_flush: Pending");
                    // Do this in poll_finish_pending_write later.
                    this.pending_tx = true;
                    this.need_flush = true;
                    return Poll::Pending;
                }
                Poll::Ready(v) => {
                    trace!("poll_flush: {:?}", v);
                    this.pending_tx = false;
                    v?
                }
            }
        }

        Ok(()).into()
    }
}

impl<R> BufIo<R>
where
    R: AsyncRead + Unpin,
{
    #[instrument(skip(self, cx, force_append))]
    pub fn poll_fill_buf(
        self: Pin<&mut Self>,
        cx: &mut Context,
        force_append: bool,
    ) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();

        let cur_len = this.buf.len();

        if cur_len == 0 || force_append {
            // when this reference is dropped, the buffer size is reset back.
            // this also extends the buffer with additional capacity if needed.
            let mut bref = this.buf.borrow();

            let read_into = &mut bref[cur_len..];

            match Pin::new(&mut this.inner).poll_read(cx, read_into) {
                Poll::Pending => {
                    trace!("poll_read: Pending");
                    this.pending_rx = true;
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => {
                    trace!("poll_read err: {:?}", e);
                    this.pending_rx = false;
                    return Err(e).into();
                }
                Poll::Ready(Ok(amount)) => {
                    trace!("poll_read amount: {}", amount);
                    this.pending_rx = false;
                    bref.extend(amount);
                }
            }
        }

        let buf = &this.buf[this.pos..];

        Ok(buf).into()
    }

    pub fn can_take_read_buf(&self) -> bool {
        // can not take a partially read buf
        self.pos == 0
    }

    pub fn take_read_buf(&mut self) -> Vec<u8> {
        let replace = FastBuf::with_capacity(self.buf.capacity());
        let buf = mem::replace(&mut self.buf, replace);
        self.pos = 0;
        buf.into_vec()
    }

    pub fn consume(self: Pin<&mut Self>, amount: usize) {
        let this = self.get_mut();

        let new_pos = this.pos + amount;

        // can't consume more than we have.
        assert!(new_pos <= this.buf.len());

        if new_pos == this.buf.len() {
            // all was consumed, reset back to start.
            this.pos = 0;
            this.buf.empty();
        } else {
            this.pos = new_pos;
        }
    }

    #[instrument(skip(self, cx, buf))]
    pub fn poll_read_buf(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        let has_amount = this.buf.len() - this.pos;

        if has_amount > 0 {
            let max = buf.len().min(has_amount);
            trace!("poll_read_buf from buffer: {}", max);

            (&mut buf[0..max]).copy_from_slice(&this.buf[this.pos..this.pos + max]);

            this.pos += max;

            // reset if all is used up.
            if this.pos == this.buf.len() {
                this.pos = 0;
                this.buf.empty();
            }

            return Ok(max).into();
        }

        // once inner buffer is used up, read directly from underlying.
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Pending => {
                trace!("poll_read: Pending");
                this.pending_rx = true;
                Poll::Pending
            }
            r @ _ => {
                trace!("poll_read: {:?}", r);
                this.pending_rx = false;
                r
            }
        }
    }
}

// ***********  BOILERPLATE BELOW ******************************

impl<R> AsyncRead for BufIo<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        Pin::new(this).poll_read_buf(cx, buf)
    }
}

impl<R> AsyncWrite for BufIo<R>
where
    R: AsyncWrite + Unpin,
{
    #[instrument(skip(self, cx, buf))]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        ready!(Pin::new(&mut *this).poll_finish_pending_write(cx))?;

        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Pending => {
                trace!("poll_write: Pending");
                this.pending_tx = true;
                Poll::Pending
            }
            r @ _ => {
                trace!("poll_write: {:?}", r);
                this.pending_tx = false;
                r
            }
        }
    }

    #[instrument(skip(self, cx))]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        ready!(Pin::new(&mut *this).poll_finish_pending_write(cx))?;

        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Pending => {
                trace!("poll_flush: Pending");
                this.pending_tx = true;
                Poll::Pending
            }
            r @ _ => {
                trace!("poll_write: {:?}", r);
                this.pending_tx = false;
                r
            }
        }
    }

    #[instrument(skip(self, cx))]
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        ready!(Pin::new(&mut *this).poll_finish_pending_write(cx))?;

        match Pin::new(&mut this.inner).poll_close(cx) {
            Poll::Pending => {
                trace!("poll_close: Pending");
                this.pending_tx = true;
                Poll::Pending
            }
            r @ _ => {
                trace!("poll_close: {:?}", r);
                this.pending_tx = false;
                r
            }
        }
    }
}
