/// Structs and methods for identifying resources.
///
/// The main type in this module is [ResourceId], which can be used to uniquely identify resources.
/// [ResourceId] is simply an index into a global table of string IDs, and as such is extremely
/// cheaply cloneable.

use ouroboros::self_referencing;
use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[self_referencing]
struct IdMap {
    data: papaya::HashMap<String, u32, ahash::RandomState>,
    #[borrows(data)]
    #[covariant]
    pin: papaya::HashMapRef<'this, String, u32, ahash::RandomState, papaya::OwnedGuard<'this>>,
}

static IDS: boxcar::Vec<String> = boxcar::Vec::new();
static ID_MAP: LazyLock<IdMap> = LazyLock::new(|| {
    let data = papaya::HashMap::builder()
        .hasher(ahash::RandomState::new())
        .build();
    IdMapBuilder {
        data,
        pin_builder: |data| data.pin_owned(),
    }.build()
});

/// String-based identifier for [Resources][res].
///
/// Internally, this structure simply contains an index into a global table of string IDs. This
/// makes ResourceId extremely cheaply cloneable. However, this global table is never cleaned, and
/// as such can only grow in size. In practice, using something along the lines of 1 million IDs
/// will consume <100MB of memory with reasonable length IDs, so the concern should be theoretical
/// at best. Similarly, the index is stored as u32, meaning there is a maximum of 4.3 billion IDs
/// before a panic is triggered due to overflow; if you manage to reach this limit please let us
/// know how :).
///
/// [res]: super::Resource
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(u32);

impl ResourceId {
    /// Get a ResourceId for a given string ID.
    ///
    /// If the string already exists in the global table, this will retrieve the index for it.
    /// Otherwise, the string will be inserted into the global table and the new index will be
    /// yielded.
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    ///
    /// let id = ResourceId::new("my-resource-id");
    /// assert_eq!(id.as_str(), "my-resource-id");
    /// ```
    #[inline]
    pub fn new(id: impl AsRef<str>) -> Self {
        let id = id.as_ref();
        let idx = ID_MAP.borrow_pin()
            .get_or_insert_with(id.to_owned(), || {
                IDS.push(id.to_owned())
                    .try_into()
                    .expect("Exceeded u32::MAX ResourceIds")
            });
        Self(*idx)
    }

    /// Get a ResourceId for a given `OsStr` ID.
    ///
    /// Converts the `OsStr` to a `str` [lossily](OsStr::to_string_lossy), and then functions as
    /// [`Self::new()`].
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    /// use std::ffi::OsString;
    ///
    /// let id = ResourceId::from_os_str(OsString::from("my-resource-id"));
    /// assert_eq!(id.as_str(), "my-resource-id");
    /// ```
    #[inline]
    pub fn from_os_str(id: impl AsRef<OsStr>) -> Self {
        Self::new(id.as_ref().to_string_lossy())
    }

    /// Looks up the string ID from the global table.
    #[inline]
    pub fn as_str(&self) -> &str {
        &IDS[self.0 as usize]
    }
}

impl Ord for ResourceId {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl PartialOrd for ResourceId {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.as_str().partial_cmp(other.as_str())
    }
}

impl AsRef<str> for ResourceId {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Debug for ResourceId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceId")
            .field("idx", &self.0)
            .field("str", &self.as_str())
            .finish()
    }
}

impl Display for ResourceId {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self.as_str(), f)
    }
}

impl From<&ResourceId> for ResourceId {
    #[inline]
    fn from(value: &ResourceId) -> Self { value.clone() }
}

macro_rules! impl_from_str {
    ($str_type:ty) => {
        impl From<$str_type> for ResourceId {
            #[inline]
            fn from(value: $str_type) -> Self {
                Self::new(value)
            }
        }
    };
}

impl_from_str!(String);
impl_from_str!(&String);
impl_from_str!(&str);

macro_rules! impl_from_os_str {
    ($str_type:ty) => {
        impl From<$str_type> for ResourceId {
            #[inline]
            fn from(value: $str_type) -> Self {
                Self::from_os_str(value)
            }
        }
    };
}

impl_from_os_str!(PathBuf);
impl_from_os_str!(&PathBuf);
impl_from_os_str!(&Path);
impl_from_os_str!(OsString);
impl_from_os_str!(&OsString);
impl_from_os_str!(&OsStr);