use crate::fast_buf::{ConsumeBuf, FastBuf};
use crate::{AsyncBufRead, AsyncRead, AsyncWrite};
use futures_util::ready;
use std::io;
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

impl<R: AsyncRead> BufIo<R>
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

    pub fn pending_rx(&self) -> bool {
        self.pending_rx
    }

    pub fn pending_tx(&self) -> bool {
        self.pending_tx
    }

    pub fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();

        let cur_len = this.buf.len();

        // when this reference is dropped, the buffer size is reset back.
        // this also extends the buffer with additional capacity if needed.
        let mut bref = this.buf.borrow();

        let read_into = &mut bref[cur_len..];

        match Pin::new(&mut this.inner).poll_read(cx, read_into) {
            Poll::Pending => {
                this.pending_rx = true;
                return Poll::Pending;
            }
            Poll::Ready(Err(e)) => {
                this.pending_rx = false;
                return Err(e).into();
            }
            Poll::Ready(Ok(amount)) => {
                this.pending_rx = false;
                bref.add_len(amount);
            }
        }

        let buf = &this.buf[this.pos..];

        Ok(buf).into()
    }

    pub fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();

        let new_pos = this.pos + amt;

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

    pub fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        let has_amount = this.buf.len() - this.pos;

        if has_amount > 0 {
            let max = buf.len().min(has_amount);
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
                this.pending_rx = true;
                Poll::Pending
            }
            r @ _ => {
                this.pending_rx = false;
                r
            }
        }
    }

    pub fn poll_finish_pending_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
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
                        this.pending_tx = true;
                        return Poll::Pending;
                    }
                    Poll::Ready(v) => v?,
                };
                buf.consume(amount);
            }

            this.write_buf = None;
        }

        if this.need_flush {
            match Pin::new(&mut this.inner).poll_flush(cx) {
                Poll::Pending => {
                    this.pending_tx = true;
                    return Poll::Pending;
                }
                Poll::Ready(v) => v?,
            }
            this.need_flush = false;
        }

        this.pending_tx = false;

        Ok(()).into()
    }

    /// Write all or none of a buffer.
    ///
    /// This poll_write variant write the entire buf to the underlying writer or nothing,
    /// potentially using an internal buffer for half written responses when hitting Pending.
    pub fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
        flush: bool,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // Any pending writes must be dealt with first.
        ready!(Pin::new(&mut *this).poll_finish_pending_write(cx))?;

        assert!(this.write_buf.is_none());
        assert!(!this.need_flush);

        let mut pos = 0;

        loop {
            if pos == buf.len() {
                break;
            }

            match Pin::new(&mut this.inner).poll_write(cx, &buf[pos..]) {
                Poll::Pending => {
                    // Half sent buffer, the rest is for poll_finish_pending_write.
                    this.pending_tx = true;
                    this.write_buf = Some(ConsumeBuf::new((&buf[pos..]).to_vec()));
                    this.need_flush = flush;
                }
                Poll::Ready(Err(e)) => {
                    this.pending_tx = false;
                    return Err(e).into();
                }
                Poll::Ready(Ok(amount)) => {
                    this.pending_tx = false;
                    pos += amount;
                }
            }
        }

        if flush {
            match Pin::new(&mut this.inner).poll_flush(cx) {
                Poll::Pending => {
                    // Do this in poll_finish_pending_write later.
                    this.pending_tx = true;
                    this.need_flush = true;
                    return Poll::Pending;
                }
                Poll::Ready(v) => {
                    this.pending_tx = false;
                    v?
                }
            }
        }

        Ok(()).into()
    }
}

impl<R> AsyncBufRead for BufIo<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();

        let cur_len = this.buf.len();

        // when this reference is dropped, the buffer size is reset back.
        // this also extends the buffer with additional capacity if needed.
        let mut bref = this.buf.borrow();

        let read_into = &mut bref[cur_len..];

        match Pin::new(&mut this.inner).poll_read(cx, read_into) {
            Poll::Pending => {
                return Poll::Pending;
            }
            Poll::Ready(Err(e)) => {
                return Err(e).into();
            }
            Poll::Ready(Ok(amount)) => {
                bref.add_len(amount);
            }
        }

        let buf = &this.buf[this.pos..];

        Ok(buf).into()
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();

        let new_pos = this.pos + amt;

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
}

// * Boilerplate proxying below **********************************

impl<R> AsyncRead for BufIo<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        let has_amount = this.buf.len() - this.pos;

        if has_amount > 0 {
            let max = buf.len().min(has_amount);
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
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<R> AsyncWrite for BufIo<R>
where
    R: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_close(cx)
    }
}
