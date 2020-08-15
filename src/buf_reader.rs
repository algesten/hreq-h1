use crate::{AsyncBufRead, AsyncRead, AsyncWrite};
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

const MAX_CAPACITY: usize = 10 * 1024 * 1024;

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

        // when this reference is dropped, the buffer size is reset back.
        let mut bref = this.buf.borrow();

        // extend buffer to be able to read until capacity.
        let read_into = &mut bref[..];

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

#[derive(Debug)]
/// Helper to manage a buf that can be resized without 0-ing.
pub(crate) struct FastBuf(Vec<u8>);

impl FastBuf {
    pub fn with_capacity(capacity: usize) -> Self {
        FastBuf(Vec::with_capacity(capacity))
    }

    pub fn empty(&mut self) {
        unsafe {
            self.0.set_len(0);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn take_vec(&mut self) -> Vec<u8> {
        let len = self.0.capacity();
        std::mem::replace(&mut self.0, Vec::with_capacity(len))
    }

    pub fn borrow<'a>(&'a mut self) -> FastBufRef<'a> {
        let len_at_start = self.0.len();

        // ensure we have capacity to read more.
        if len_at_start == self.0.capacity() {
            let max = MAX_CAPACITY.min(self.0.capacity() * 2);
            self.0.reserve(max - self.0.capacity());
        }

        // invariant: we must have some spare capacity.
        assert!(self.0.capacity() > len_at_start);

        // size up to full capacity. the idea is that we reset
        // this back when FastBufRef drops.
        unsafe {
            self.0.set_len(self.0.capacity());
        }

        FastBufRef(len_at_start, &mut self.0)
    }
}

impl std::ops::Deref for FastBuf {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &(self.0)[..]
    }
}

pub(crate) struct FastBufRef<'a>(usize, &'a mut Vec<u8>);

impl<'a> FastBufRef<'a> {
    pub fn add_len(mut self, amount: usize) {
        self.0 += amount;
        assert!(self.0 <= self.1.len());
    }
}

impl<'a> std::ops::Deref for FastBufRef<'a> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &(self.1)[..]
    }
}

impl<'a> std::ops::DerefMut for FastBufRef<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut (self.1)[..]
    }
}

impl<'a> Drop for FastBufRef<'a> {
    fn drop(&mut self) {
        // set length back when ref drops.
        unsafe {
            self.1.set_len(self.0);
        }
    }
}
