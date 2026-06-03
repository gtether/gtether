#[allow(unused)]
macro_rules! impl_id_counter {
    ($main_type:ident, $v:vis $id_type:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $v struct $id_type(std::num::NonZeroU64);

        impl $main_type {
            fn next_id() -> $id_type {
                static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                let id = std::num::NonZeroU64::new(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                    .expect("ID counter overflow");
                $id_type(id)
            }

            #[inline]
            $v fn id(&self) -> $id_type {
                self.id
            }
        }

        impl PartialEq for $main_type {
            #[inline]
            fn eq(&self, other: &Self) -> bool {
                self.id == other.id
            }
        }

        impl Eq for $main_type {}

        impl std::hash::Hash for $main_type {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.id.hash(state);
            }
        }
    };
}
#[allow(unused)]
pub(crate) use impl_id_counter;