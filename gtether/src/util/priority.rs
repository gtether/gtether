//! Primitives and traits used for determining priority ordering.

use std::sync::{atomic, Arc};

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