use futures_core::task::__internal::AtomicWaker;
use parking_lot::Mutex;
use std::sync::{Arc, Weak};
use std::task::Wake;

#[derive(Default)]
pub struct MultiWaker(Mutex<Vec<Weak<AtomicWaker>>>);

impl Wake for MultiWaker {
    fn wake(self: Arc<Self>) {
        self.0.lock().retain(|weak| {
            if let Some(waker) = weak.upgrade() {
                waker.wake();
                true
            } else {
                false
            }
        })
    }
}

impl MultiWaker {
    #[inline]
    pub fn push(&self, waker: &Arc<AtomicWaker>) {
        self.0.lock().push(Arc::downgrade(waker))
    }
}