use futures_util::future::poll_fn;
use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

/// Simple mpsc channel.
#[derive(Clone)]
pub struct Receiver<T> {
    inner: Arc<Mutex<Inner<T>>>,
    count: Arc<()>,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let count = Arc::strong_count(&self.count);
        let mut lock = self.inner.lock().unwrap();

        // If count is 1, this is the last instance. With no more Receiver
        // around, the queue will not be consumed, we clear it out and
        // set it to ended, which means any Sender side will effectively
        // push data into /dev/null.
        if count == 1 {
            lock.queue.clear();
            lock.ended = true;
        }

        lock.wake_all();
    }
}

impl<T> Receiver<T> {
    pub fn new(bound: usize) -> (Sender<T>, Receiver<T>) {
        let inner = Arc::new(Mutex::new(Inner::new(bound)));

        let weak = Arc::downgrade(&inner);

        (
            Sender { inner: weak },
            Receiver {
                inner,
                count: Arc::new(()),
            },
        )
    }

    pub fn poll_recv(self: Pin<&Self>, cx: &mut Context, register: bool) -> Poll<Option<T>> {
        let this = self.get_ref();

        let mut lock = this.inner.lock().unwrap();

        match lock.poll_dequeue(cx, register) {
            Poll::Pending => {
                if Arc::weak_count(&this.inner) == 0 {
                    // no more senders around
                    None.into()
                } else {
                    Poll::Pending
                }
            }

            r @ _ => r,
        }
    }

    pub async fn recv(&self) -> Option<T> {
        poll_fn(|cx| Pin::new(&*self).poll_recv(cx, true)).await
    }
}

pub struct Sender<T> {
    inner: Weak<Mutex<Inner<T>>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Sender<T> {
    pub fn poll_ready(self: Pin<&Self>, cx: &mut Context, register: bool) -> Poll<bool> {
        let this = self.get_ref();

        if let Some(inner) = this.inner.upgrade() {
            let mut lock = inner.lock().unwrap();
            lock.poll_ready(cx, register)
        } else {
            false.into()
        }
    }

    pub fn send(&self, t: T) -> bool {
        if let Some(inner) = self.inner.upgrade() {
            let mut lock = inner.lock().unwrap();

            lock.enqueue(t);

            true
        } else {
            false
        }
    }

    pub fn len(&self) -> usize {
        if let Some(inner) = self.inner.upgrade() {
            let lock = inner.lock().unwrap();
            lock.queue.len()
        } else {
            0
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            // wake everyone, just in case
            let mut lock = inner.lock().unwrap();
            lock.wake_all()
        }
    }
}

#[derive(Default)]
struct Inner<T> {
    queue: VecDeque<T>,
    // Whether we are accepting anymore data.
    ended: bool,
    // How much data we enqueue before Pending the sender.
    bound: usize,
    // We could have separate send and receive wakers. I feel like
    // that creates potential race conditions. In 99.9% of cases
    // there will only be one receiver and one sender anyway.
    wakers: Vec<Waker>,
}

impl<T> Inner<T> {
    fn new(bound: usize) -> Self {
        Inner {
            queue: VecDeque::new(),
            ended: false,
            bound,
            wakers: Vec::new(),
        }
    }

    fn poll_ready(&mut self, cx: &mut Context, register: bool) -> Poll<bool> {
        if !self.ended && self.queue.len() >= self.bound {
            if register {
                self.wakers.push(cx.waker().clone());
            }
            Poll::Pending
        } else {
            true.into()
        }
    }

    fn enqueue(&mut self, t: T) {
        if !self.ended {
            self.queue.push_back(t);
        }
        self.wake_all();
    }

    fn poll_dequeue(&mut self, cx: &mut Context, register: bool) -> Poll<Option<T>> {
        if let Some(t) = self.queue.pop_front() {
            self.wake_all();
            Some(t).into()
        } else {
            if register {
                self.wakers.push(cx.waker().clone());
            }
            Poll::Pending
        }
    }

    fn wake_all(&mut self) {
        for w in self.wakers.drain(..) {
            w.wake();
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Receiver")
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sender")
    }
}
