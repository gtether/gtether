use std::fmt::{Display, Formatter};
use std::ops::{Add, AddAssign, Deref};

/// String-based identifier for [Resources][res].
///
/// Generally functions similar to a String, but may have additional functionality.
///
/// [res]: super::Resource
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourcePath {
    inner: String,
}

impl ResourcePath {
    /// Create a new ResourcePath from something that can be turned into a String.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            inner: value.into(),
        }
    }
}

impl Add<&str> for ResourcePath {
    type Output = ResourcePath;

    #[inline]
    fn add(self, rhs: &str) -> Self::Output {
        ResourcePath::new(self.inner + rhs)
    }
}

impl AddAssign for ResourcePath {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.inner += &rhs;
    }
}

impl Deref for ResourcePath {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target { &self.inner }
}

impl Display for ResourcePath {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.inner, f)
    }
}

impl From<&ResourcePath> for ResourcePath {
    #[inline]
    fn from(value: &ResourcePath) -> Self { value.clone() }
}

impl From<String> for ResourcePath {
    #[inline]
    fn from(value: String) -> Self { Self::new(value) }
}

impl From<&String> for ResourcePath {
    #[inline]
    fn from(value: &String) -> Self { Self::new(value) }
}

impl From<&str> for ResourcePath {
    #[inline]
    fn from(value: &str) -> Self { Self::new(value) }
}