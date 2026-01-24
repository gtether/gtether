//! Data structures and traits used for determining priority ordering.
//!
//! In contrast with existing solutions like [`std::collections::BinaryHeap`], this module provides
//! the capability to have "dynamic" priority, in that the priority for a value can change via
//! interior mutability and the data structures in this module will do their best to accommodate.

use std::collections::BinaryHeap;
use std::sync::{atomic, Arc};
use educe::Educe;

/// Trait describing something that provides a static priority value.
///
/// This trait is auto-implemented for anything that implements `Ord`, and is semantically
/// equivalent to `Ord`. It exists only as a convenient marker trait, and to serve as a pair to
/// [`HasDynamicPriority`].
pub trait HasStaticPriority: Ord {}

impl<P: Ord> HasStaticPriority for P {}

/// Trait describing something that provides a dynamically changing priority value.
///
/// Implementors of this trait do not necessarily implement [Ord], as their ordering can change over
/// time. However, the priority value that implementors yield _do_ implement [Ord], and can be
/// safely used for ordering.
///
/// This trait is automatically implemented for many common types and wrappers.
pub trait HasDynamicPriority {
    /// Type of the priority value.
    ///
    /// Must implement [Ord].
    type Value: Ord;

    /// Priority value used for ordering.
    fn priority(&self) -> Self::Value;
}

macro_rules! impl_priority_copy_self {
    ($self_type:ty) => {
        impl HasDynamicPriority for $self_type {
            type Value = $self_type;

            #[inline]
            fn priority(&self) -> Self::Value {
                *self
            }
        }
    };
}

impl_priority_copy_self!(i8);
impl_priority_copy_self!(i16);
impl_priority_copy_self!(i32);
impl_priority_copy_self!(i64);
impl_priority_copy_self!(isize);
impl_priority_copy_self!(u8);
impl_priority_copy_self!(u16);
impl_priority_copy_self!(u32);
impl_priority_copy_self!(u64);
impl_priority_copy_self!(usize);

impl<P: HasDynamicPriority> HasDynamicPriority for Arc<P> {
    type Value = P::Value;

    #[inline]
    fn priority(&self) -> Self::Value {
        (**self).priority()
    }
}

macro_rules! impl_priority_wrapper_method {
    ($wrapper_type:ty, $method:ident) => {
        impl<P: HasDynamicPriority> HasDynamicPriority for $wrapper_type {
            type Value = P::Value;

            #[inline]
            fn priority(&self) -> Self::Value {
                self.$method().priority()
            }
        }
    };
}

impl_priority_wrapper_method!(parking_lot::Mutex<P>, lock);
impl_priority_wrapper_method!(parking_lot::RwLock<P>, read);
impl_priority_wrapper_method!(smol::lock::Mutex<P>, lock_blocking);
impl_priority_wrapper_method!(smol::lock::RwLock<P>, read_blocking);

macro_rules! impl_priority_atomic {
    ($atomic_type:ty, $int_type:ty) => {
        impl HasDynamicPriority for $atomic_type {
            type Value = $int_type;

            #[inline]
            fn priority(&self) -> Self::Value {
                self.load(atomic::Ordering::Relaxed)
            }
        }
    };
}

impl_priority_atomic!(atomic::AtomicI8, i8);
impl_priority_atomic!(atomic::AtomicI16, i16);
impl_priority_atomic!(atomic::AtomicI32, i32);
impl_priority_atomic!(atomic::AtomicI64, i64);
impl_priority_atomic!(atomic::AtomicIsize, isize);
impl_priority_atomic!(atomic::AtomicU8, u8);
impl_priority_atomic!(atomic::AtomicU16, u16);
impl_priority_atomic!(atomic::AtomicU32, u32);
impl_priority_atomic!(atomic::AtomicU64, u64);
impl_priority_atomic!(atomic::AtomicUsize, usize);

/// Common priority queue logic.
///
/// This trait is implemented by the various types of priority queues in this module, such as:
///  * [StaticPriorityQueue]
///  * [DynamicPriorityQueue]
///
/// For more specific documentation and examples, see the implementor type.
pub trait PriorityQueue<T> {
    /// Returns the length of the queue.
    fn len(&self) -> usize;

    /// Checks if the queue is empty.
    fn is_empty(&self) -> bool;

    /// Returns the item in the queue with the highest priority, or `None` if the queue is empty.
    ///
    /// Time complexity depends on the implementation; see individual documentation for more.
    fn peek(&self) -> Option<&T>;

    /// Push an item onto the queue.
    ///
    /// Time complexity depends on the implementation; see individual documentation for more.
    fn push(&mut self, value: T);

    /// Removes the item with the highest priority and returns it, or `None` if the queue is empty.
    ///
    /// Time complexity depends on the implementation; see individual documentation for more.
    fn pop(&mut self) -> Option<T>;

    /// Compares `value` to the highest priority in the queue, and swaps with it if it is higher.
    ///
    /// Time complexity depends on the implementation; see individual documentation for more.
    fn swap_if_higher(&mut self, value: T) -> T;
}

/// Priority queue using [static priorities](HasStaticPriority).
///
/// Uses a [BinaryHeap] internally, so most operations simply delegate to [BinaryHeap].
#[derive(Educe)]
#[educe(Default)]
pub struct StaticPriorityQueue<T: HasStaticPriority> {
    inner: BinaryHeap<T>,
}

impl<T: HasStaticPriority> PriorityQueue<T> for StaticPriorityQueue<T> {
    /// Returns the length of the queue.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, StaticPriorityQueue};
    /// let queue = StaticPriorityQueue::<isize>::from([-2, 3]);
    /// assert_eq!(queue.len(), 2);
    /// ```
    #[inline]
    fn len(&self) -> usize {
        self.inner.len()
    }

    /// Checks if the queue is empty.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, StaticPriorityQueue};
    /// let mut queue = StaticPriorityQueue::<isize>::default();
    /// assert!(queue.is_empty());
    ///
    /// queue.push(0);
    /// queue.push(-2);
    /// queue.push(3);
    /// assert!(!queue.is_empty());
    /// ```
    #[inline]
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the item in the queue with the highest priority, or `None` if the queue is empty.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, StaticPriorityQueue};
    /// let mut queue = StaticPriorityQueue::<isize>::default();
    /// assert_eq!(queue.peek(), None);
    ///
    /// queue.push(0);
    /// queue.push(3);
    /// queue.push(-2);
    /// assert_eq!(queue.peek(), Some(&3));
    /// ```
    ///
    /// # Time complexity
    ///
    /// Delegates to [`BinaryHeap::peek()`]; see that method for time complexity.
    #[inline]
    fn peek(&self) -> Option<&T> {
        self.inner.peek()
    }

    /// Push an item onto the queue.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, StaticPriorityQueue};
    /// let mut queue = StaticPriorityQueue::<isize>::default();
    /// queue.push(0);
    /// queue.push(3);
    /// queue.push(-2);
    ///
    /// assert_eq!(queue.len(), 3);
    /// assert_eq!(queue.peek(), Some(&3));
    /// ```
    ///
    /// # Time complexity
    ///
    /// Delegates to [`BinaryHeap::push()`]; see that method for time complexity.
    #[inline]
    fn push(&mut self, value: T) {
        self.inner.push(value)
    }

    /// Removes the item with the highest priority and returns it, or `None` if the queue is empty.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, StaticPriorityQueue};
    /// let mut queue = StaticPriorityQueue::<isize>::from([-2, 3]);
    ///
    /// assert_eq!(queue.pop(), Some(3));
    /// assert_eq!(queue.pop(), Some(-2));
    /// assert_eq!(queue.pop(), None);
    /// ```
    ///
    /// # Time complexity
    ///
    /// Delegates to [`BinaryHeap::pop()`]; see that method for time complexity.
    #[inline]
    fn pop(&mut self) -> Option<T> {
        self.inner.pop()
    }

    /// Compares `value` to the highest priority in the queue, and swaps with it if it is higher.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, StaticPriorityQueue};
    /// let mut queue = StaticPriorityQueue::<isize>::from([-2, 3]);
    ///
    /// assert_eq!(queue.swap_if_higher(0), 3);
    /// assert_eq!(queue.swap_if_higher(10), 10);
    /// ```
    ///
    /// # Time complexity
    ///
    /// This is equivalent to a `peek()`, comparison, `pop()` and `push()`, and inherits time
    /// complexity appropriately. This means the worst is in general is _O_(log(_n_)), but can be as
    /// bad as _O_(_n_) in accordance with [`BinaryHeap::push()`].
    fn swap_if_higher(&mut self, value: T) -> T {
        if let Some(highest) = self.inner.peek() && highest > &value {
            let new_value = self.inner.pop().unwrap();
            self.inner.push(value);
            new_value
        } else {
            value
        }
    }
}

impl<T: HasStaticPriority> From<Vec<T>> for StaticPriorityQueue<T> {
    #[inline]
    fn from(value: Vec<T>) -> Self {
        Self {
            inner: BinaryHeap::from(value)
        }
    }
}

impl<T: HasStaticPriority> FromIterator<T> for StaticPriorityQueue<T> {
    #[inline]
    fn from_iter<II: IntoIterator<Item=T>>(iter: II) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl<T: HasStaticPriority, const N: usize> From<[T; N]> for StaticPriorityQueue<T> {
    #[inline]
    fn from(value: [T; N]) -> Self {
        Self::from_iter(value)
    }
}

/// Priority queue using [dynamic priorities](HasDynamicPriority).
#[derive(Educe)]
#[educe(Default)]
pub struct DynamicPriorityQueue<T: HasDynamicPriority> {
    inner: Vec<T>,
}

impl<T: HasDynamicPriority> DynamicPriorityQueue<T> {
    fn find_highest(&self) -> Option<(usize, &T, T::Value)> {
        let mut highest: Option<(usize, &T, T::Value)> = None;
        for (idx, value) in self.inner.iter().enumerate() {
            let priority = value.priority();
            match highest {
                Some((_, _, ref highest_priority)) => {
                    if &priority > highest_priority {
                        highest = Some((idx, value, priority));
                    }
                },
                None => {
                    highest = Some((idx, value, priority));
                },
            }
        }
        highest
    }
}

impl<T: HasDynamicPriority> PriorityQueue<T> for DynamicPriorityQueue<T> {
    /// Returns the length of the queue.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, DynamicPriorityQueue};
    /// use std::sync::atomic::AtomicIsize;
    /// let queue = DynamicPriorityQueue::<AtomicIsize>::from([AtomicIsize::new(-2), AtomicIsize::new(3)]);
    /// assert_eq!(queue.len(), 2);
    /// ```
    #[inline]
    fn len(&self) -> usize {
        self.inner.len()
    }

    /// Checks if the queue is empty.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, DynamicPriorityQueue};
    /// use std::sync::atomic::AtomicIsize;
    /// let mut queue = DynamicPriorityQueue::<AtomicIsize>::default();
    /// assert!(queue.is_empty());
    ///
    /// queue.push(AtomicIsize::new(0));
    /// queue.push(AtomicIsize::new(-2));
    /// queue.push(AtomicIsize::new(3));
    /// assert!(!queue.is_empty());
    /// ```
    #[inline]
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the item in the queue with the highest priority, or `None` if the queue is empty.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, DynamicPriorityQueue};
    /// use std::sync::{Arc, atomic::{AtomicIsize, Ordering}};
    /// let mut queue = DynamicPriorityQueue::<Arc<AtomicIsize>>::default();
    /// assert!(queue.peek().is_none());
    ///
    /// let val_a = Arc::new(AtomicIsize::new(-2));
    /// let val_b = Arc::new(AtomicIsize::new(3));
    /// queue.push(val_a.clone());
    /// queue.push(val_b.clone());
    ///
    /// {
    ///     let val = queue.peek().expect("should be Some()");
    ///     assert_eq!(val.load(Ordering::Relaxed), 3);
    /// }
    ///
    /// {
    ///     val_a.store(10, Ordering::Relaxed);
    ///     let val = queue.peek().expect("should be Some()");
    ///     assert_eq!(val.load(Ordering::Relaxed), 10);
    /// }
    /// ```
    ///
    /// # Time complexity
    ///
    /// Because priorities are dynamic, the entire queue must be iterated to find the highest
    /// priority every time, making the cost _O_(_n_).
    #[inline]
    fn peek(&self) -> Option<&T> {
        self.find_highest().map(|(_, value, _)| value)
    }

    /// Push an item onto the queue.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, DynamicPriorityQueue};
    /// use std::sync::atomic::{AtomicIsize, Ordering};
    /// let mut queue = DynamicPriorityQueue::<AtomicIsize>::default();
    /// queue.push(AtomicIsize::new(0));
    /// queue.push(AtomicIsize::new(-2));
    /// queue.push(AtomicIsize::new(3));
    ///
    /// assert_eq!(queue.len(), 3);
    /// let val = queue.peek().expect("should be Some()");
    /// assert_eq!(val.load(Ordering::Relaxed), 3);
    /// ```
    ///
    /// # Time complexity
    ///
    /// Takes amortized _O_(1) time. See [`Vec::push()`] for more.
    #[inline]
    fn push(&mut self, value: T) {
        self.inner.push(value)
    }

    /// Removes the item with the highest priority and returns it, or `None` if the queue is empty.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, DynamicPriorityQueue};
    /// use std::sync::{Arc, atomic::{AtomicIsize, Ordering}};
    ///
    /// let val_a = Arc::new(AtomicIsize::new(0));
    /// let val_b = Arc::new(AtomicIsize::new(-2));
    /// let val_c = Arc::new(AtomicIsize::new(3));
    ///
    /// let mut queue = DynamicPriorityQueue::<Arc<AtomicIsize>>::from([
    ///     val_a.clone(),
    ///     val_b.clone(),
    ///     val_c.clone(),
    /// ]);
    ///
    /// {
    ///     let val = queue.pop().expect("should be Some()");
    ///     assert_eq!(val.load(Ordering::Relaxed), 3);
    /// }
    ///
    /// {
    ///     val_b.store(10, Ordering::Relaxed);
    ///     let val = queue.pop().expect("should be Some()");
    ///     assert_eq!(val.load(Ordering::Relaxed), 10);
    /// }
    ///
    /// {
    ///     let val = queue.pop().expect("should be Some()");
    ///     assert_eq!(val.load(Ordering::Relaxed), 0);
    /// }
    ///
    /// assert!(queue.pop().is_none());
    /// ```
    ///
    /// # Time complexity
    ///
    /// Because priorities are dynamic, the entire queue must be iterated to find the highest
    /// priority every time, making the cost _O_(_n_).
    #[inline]
    fn pop(&mut self) -> Option<T> {
        self.find_highest()
            // Drop the value ref so that we can mutate self.inner
            .map(|(idx, _, _)| idx)
            .map(|idx| self.inner.swap_remove(idx))
    }

    /// Compares `value` to the highest priority in the queue, and swaps with it if it is higher.
    ///
    /// ```
    /// use gtether::util::priority::{PriorityQueue, DynamicPriorityQueue};
    /// use std::sync::{Arc, atomic::{AtomicIsize, Ordering}};
    ///
    /// let val_a = Arc::new(AtomicIsize::new(5));
    /// let mut queue = DynamicPriorityQueue::<Arc<AtomicIsize>>::from([val_a.clone()]);
    ///
    /// let val_b = Arc::new(AtomicIsize::new(10));
    /// {
    ///     let val = queue.swap_if_higher(val_b.clone());
    ///     assert_eq!(val.load(Ordering::Relaxed), 10);
    /// }
    ///
    /// val_a.store(20, Ordering::Relaxed);
    /// {
    ///     let val = queue.swap_if_higher(val_b.clone());
    ///     assert_eq!(val.load(Ordering::Relaxed), 20);
    /// }
    /// ```
    ///
    /// # Time complexity
    ///
    /// Because priorities are dynamic, the entire queue must be iterated to find the highest
    /// priority every time, making the cost _O_(_n_).
    ///
    /// This method only searches for the highest priority once before comparing and swapping, so
    /// it is faster than manually comparing with `peek()` and then calling `pop()` and `push()`.
    fn swap_if_higher(&mut self, value: T) -> T {
        let priority = value.priority();
        let highest = self.find_highest().map(|(idx, _, priority)| (idx, priority));
        if let Some((idx, highest_priority)) = highest && highest_priority > priority {
            let new_value = self.inner.swap_remove(idx);
            self.inner.push(value);
            new_value
        } else {
            value
        }
    }
}

impl<T: HasDynamicPriority> From<Vec<T>> for DynamicPriorityQueue<T> {
    #[inline]
    fn from(value: Vec<T>) -> Self {
        Self {
            inner: value,
        }
    }
}

impl<T: HasDynamicPriority> FromIterator<T> for DynamicPriorityQueue<T> {
    #[inline]
    fn from_iter<II: IntoIterator<Item=T>>(iter: II) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl<T: HasDynamicPriority, const N: usize> From<[T; N]> for DynamicPriorityQueue<T> {
    #[inline]
    fn from(value: [T; N]) -> Self {
        Self::from_iter(value)
    }
}