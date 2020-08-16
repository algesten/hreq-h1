use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

/// Simple mpsc channel
pub(crate) struct Receiver<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut lock = self.inner.lock().unwrap();

        lock.wake_send();
    }
}

impl<T> Receiver<T> {
    pub fn new(bound: usize) -> (Sender<T>, Receiver<T>) {
        let inner = Arc::new(Mutex::new(Inner::new(bound)));

        let weak = Arc::downgrade(&inner);

        (Sender { inner: weak }, Receiver { inner })
    }

    pub fn poll_recv(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<T>> {
        let this = self.get_mut();

        let mut lock = this.inner.lock().unwrap();

        match lock.poll_dequeue(cx) {
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
}

pub(crate) struct Sender<T> {
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
    pub fn poll_ready(self: Pin<&Self>, cx: &mut Context) -> Poll<bool> {
        let this = self.get_ref();

        if let Some(inner) = this.inner.upgrade() {
            let mut lock = inner.lock().unwrap();
            lock.poll_ready(cx)
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
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let c = Arc::weak_count(&inner);

            if c == 1 {
                // no more senders to wake receiver
                let mut lock = inner.lock().unwrap();
                lock.wake_recv()
            }
        }
    }
}

#[derive(Default)]
struct Inner<T> {
    queue: VecDeque<T>,
    bound: usize,
    recv_wait: Option<Waker>,
    send_wait: VecDeque<Waker>,
}

impl<T> Inner<T> {
    fn new(bound: usize) -> Self {
        Inner {
            queue: VecDeque::new(),
            bound,
            recv_wait: None,
            send_wait: VecDeque::new(),
        }
    }

    fn poll_ready(&mut self, cx: &mut Context) -> Poll<bool> {
        if self.queue.len() >= self.bound {
            self.send_wait.push_back(cx.waker().clone());
            Poll::Pending
        } else {
            true.into()
        }
    }

    fn enqueue(&mut self, t: T) {
        self.queue.push_back(t);
        self.wake_recv();
    }

    fn poll_dequeue(&mut self, cx: &mut Context) -> Poll<Option<T>> {
        if let Some(t) = self.queue.pop_front() {
            self.wake_send();
            Some(t).into()
        } else {
            self.recv_wait = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn wake_recv(&mut self) {
        if let Some(w) = self.recv_wait.take() {
            w.wake();
        }
    }

    fn wake_send(&mut self) {
        for w in self.send_wait.drain(..) {
            w.wake();
        }
    }
}
