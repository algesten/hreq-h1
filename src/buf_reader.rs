use crate::{AsyncBufRead, AsyncRead, AsyncWrite};
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

const CAPACITY: usize = 16_384;

#[derive(Debug)]
/// Our own BufReader.
///
/// The use AsyncBufRead in poll_for_crlfcrlf requires poll_fill_buf to
/// always fill from the underlying reader, also when there is content
/// buffered already.
pub struct BufReader<R> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
}

impl<R: AsyncRead> BufReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_capacity(CAPACITY, inner)
    }

    pub fn with_capacity(capacity: usize, inner: R) -> Self {
        BufReader {
            inner,
            buf: Vec::with_capacity(capacity),
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

        let can_read = this.buf.capacity() - this.buf.len();

        // if buffer is at capacity, we extend it. this should be rare since
        // we reset back to 0 whenever the entire buffer is consumed.
        if can_read == 0 {
            this.buf.reserve(CAPACITY);
        }

        let len_before_read = this.buf.len();

        // extend buffer to be able to read until capacity.
        let read_into = unsafe {
            this.buf.set_len(this.buf.capacity());
            &mut this.buf[len_before_read..]
        };

        match Pin::new(&mut this.inner).poll_read(cx, read_into) {
            Poll::Pending => unsafe {
                // don't leave buffer in unsafe state.
                this.buf.set_len(len_before_read);
                return Poll::Pending;
            },
            Poll::Ready(Err(e)) => unsafe {
                // don't leave buffer in unsafe state.
                this.buf.set_len(len_before_read);
                return Err(e).into();
            },
            Poll::Ready(Ok(amount)) => unsafe {
                // we have for sure read amount additional data into buffer
                this.buf.set_len(len_before_read + amount);
            },
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
            unsafe {
                this.buf.set_len(0);
            }
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
                unsafe {
                    this.buf.set_len(0);
                }
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
