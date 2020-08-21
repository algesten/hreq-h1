const MAX_CAPACITY: usize = 10 * 1024 * 1024;

/// Buffer with a read start pointer moved by consume().
#[derive(Debug)]
pub(crate) struct ConsumeBuf {
    buf: Vec<u8>,
    pos: usize,
}

impl ConsumeBuf {
    pub fn new(buf: Vec<u8>) -> Self {
        ConsumeBuf { buf, pos: 0 }
    }

    pub fn consume(&mut self, amount: usize) {
        let new_pos = self.pos + amount;
        assert!(new_pos <= self.buf.len());
        self.pos = new_pos;
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.buf.len() - self.pos
    }
}

impl std::ops::Deref for ConsumeBuf {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.buf[self.pos..]
    }
}

#[derive(Debug)]
/// Helper to manage a buf that can be resized without 0-ing.
pub(crate) struct FastBuf(Vec<u8>);

impl FastBuf {
    pub fn with_capacity(capacity: usize) -> Self {
        FastBuf(Vec::with_capacity(capacity))
    }

    pub fn ensure_capacity(&mut self, capacity: usize) {
        if capacity > self.0.capacity() {
            self.0.reserve(capacity - self.0.capacity())
        }
    }

    pub fn empty(&mut self) {
        unsafe {
            self.0.set_len(0);
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    pub fn borrow<'a>(&'a mut self) -> FastBufRef<'a> {
        assert!(self.0.capacity() > 0, "FastBuf::borrow() with 0 capacity");
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

        FastBufRef {
            buf: &mut self.0,
            cur_len: len_at_start,
        }
    }
}

impl std::ops::Deref for FastBuf {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &(self.0)[..]
    }
}

pub(crate) struct FastBufRef<'a> {
    buf: &'a mut Vec<u8>,
    cur_len: usize,
}

impl<'a> FastBufRef<'a> {
    /// Extend this buf from the slice. The
    /// total length will be added to once the
    /// FastBufRef is dropped.
    pub fn extend_from_slice(&mut self, slice: &[u8]) {
        assert!(
            self.cur_len + slice.len() <= self.buf.len(),
            "FastBuf::extend_from_slice not enough len"
        );
        (&mut self.buf[self.cur_len..(self.cur_len + slice.len())]).copy_from_slice(slice);
        self.cur_len += slice.len();
    }

    pub fn extend(mut self, amount: usize) {
        assert!(
            self.cur_len + amount <= self.buf.len(),
            "FastBuf::extend with not enough len"
        );
        self.cur_len += amount;
    }
}

/// This is kinda the point of the entire FastBuf
impl<'a> Drop for FastBufRef<'a> {
    fn drop(&mut self) {
        // set length back when ref drops.
        unsafe {
            self.buf.set_len(self.cur_len);
        }
    }
}

impl<'a> std::ops::Deref for FastBufRef<'a> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &(self.buf)[..]
    }
}

impl<'a> std::ops::DerefMut for FastBufRef<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut (self.buf)[..]
    }
}
