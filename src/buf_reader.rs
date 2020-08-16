use crate::fast_buf::FastBuf;
use crate::{AsyncBufRead, AsyncRead, AsyncWrite};
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
pub struct BufReader<R> {
    inner: R,
    buf: FastBuf,
    pos: usize,
}

impl<R: AsyncRead> BufReader<R> {
    pub fn with_capacity(capacity: usize, inner: R) -> Self {
        BufReader {
            inner,
            buf: FastBuf::with_capacity(capacity),
            pos: 0,
        }
    }
}

impl<R> AsyncBufRead for BufReader<R>
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

impl<R> AsyncRead for BufReader<R>
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

impl<R> AsyncWrite for BufReader<R>
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
