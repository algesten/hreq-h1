const MAX_CAPACITY: usize = 10 * 1024 * 1024;

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

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn take_vec(&mut self) -> Vec<u8> {
        let cap = self.0.capacity();
        std::mem::replace(&mut self.0, Vec::with_capacity(cap))
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

        FastBufRef(&mut self.0, len_at_start)
    }
}

impl std::ops::Deref for FastBuf {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &(self.0)[..]
    }
}

pub(crate) struct FastBufRef<'a>(&'a mut Vec<u8>, usize);

impl<'a> FastBufRef<'a> {
    /// Add to length buffer is resized down to at drop.
    ///
    /// The called _must ensure_ there is data read into this amount of length.
    pub fn add_len(mut self, amount: usize) {
        self.1 += amount;
        assert!(self.1 <= self.0.len());
    }
}

/// This is kinda the point of the entire FastBuf
impl<'a> Drop for FastBufRef<'a> {
    fn drop(&mut self) {
        // set length back when ref drops.
        unsafe {
            self.0.set_len(self.1);
        }
    }
}

impl<'a> std::ops::Deref for FastBufRef<'a> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &(self.0)[..]
    }
}

impl<'a> std::ops::DerefMut for FastBufRef<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut (self.0)[..]
    }
}
