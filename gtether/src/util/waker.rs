use futures_util::task::AtomicWaker;
use parking_lot::RwLock;
use std::sync::{Arc, Weak};
use std::task::Wake;

#[derive(Default)]
pub struct MultiWaker(RwLock<Vec<Weak<AtomicWaker>>>);

impl Wake for MultiWaker {
    fn wake(self: Arc<Self>) {
        self.0.write().retain(|weak| {
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
    pub fn len(&self) -> usize {
        self.0.read().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.read().is_empty()
    }

    #[inline]
    pub fn push(&self, waker: &Arc<AtomicWaker>) {
        self.0.write().push(Arc::downgrade(waker))
    }
}