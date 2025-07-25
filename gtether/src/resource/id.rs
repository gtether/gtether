/// Structs and methods for identifying resources.
///
/// The main type in this module is [ResourceId], which can be used to uniquely identify resources.
/// [ResourceId] functions like a simplified [PathBuf]; see it's documentation for more.

use itertools::Itertools;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::iter::FusedIterator;
use std::ops::{Add, AddAssign};
use std::path::{Path, PathBuf};

/// String-based identifier for [Resources][res].
///
/// Functions similarly to [PathBuf], but in a simpler fashion. ResourceId doesn't have the concept
/// of file extensions, and doesn't distinguish between different types of components, though it
/// does separate components based on a separator (currently '/'). Additionally, ResourceId doesn't
/// distinguish between absolute or relative paths; all IDs are considered relative.
///
/// If filesystem operations need to be achieved using a ResourceId, you can convert one to a
/// PathBuf via `from()`.
///
/// [res]: super::Resource
#[derive(Clone)]
pub struct ResourceId {
    inner: String,
    joints: Vec<usize>,
}

impl ResourceId {
    /// The character used to separate different components in a ResourceId.
    pub const SEPARATOR: char = '/';

    /// Create a new empty ResourceId.
    ///
    /// # Examples
    /// ```
    /// use gtether::resource::id::ResourceId;
    ///
    /// let id = ResourceId::new();
    /// assert_eq!(id.as_str(), "");
    /// ```
    pub fn new() -> Self {
        ResourceId {
            inner: String::new(),
            joints: Vec::new(),
        }
    }

    fn sanitize_input(input: impl AsRef<str>) -> Self {
        let input = input.as_ref();
        let inner = input.trim_matches(Self::SEPARATOR).split(Self::SEPARATOR)
            .filter(|v| !v.is_empty())
            .join(&Self::SEPARATOR.to_string());
        let joints = inner.match_indices(Self::SEPARATOR)
            .map(|(idx, _)| idx)
            .collect();
        Self {
            inner,
            joints,
        }
    }

    /// Iterate over the individual components of a ResourceId.
    ///
    /// # Examples
    /// ```
    /// use gtether::resource::id::ResourceId;
    ///
    /// let id = ResourceId::from("a/b/c/d");
    /// let mut components = id.components();
    /// assert_eq!(components.next(), Some("a"));
    /// assert_eq!(components.next_back(), Some("d"));
    /// assert_eq!(components.next_back(), Some("c"));
    /// assert_eq!(components.next(), Some("b"));
    /// assert_eq!(components.next(), None);
    /// assert_eq!(components.next_back(), None);
    ///
    /// assert_eq!(ResourceId::from("value").components().next(), Some("value"));
    /// assert_eq!(ResourceId::from("value").components().next_back(), Some("value"));
    /// assert_eq!(ResourceId::new().components().next(), Some(""));
    /// assert_eq!(ResourceId::new().components().next_back(), Some(""));
    /// ```
    #[inline]
    pub fn components(&self) -> Components<'_> {
        Components {
            id: self,
            joints: self.joints.clone().into(),
            start_idx: Some(0),
            end_idx: Some(self.inner.len()),
        }
    }

    /// Yields the underlying [`&str`] slice.
    ///
    /// # Examples
    /// ```
    /// use gtether::resource::id::ResourceId;
    ///
    /// let id = ResourceId::from("my_id");
    /// assert_eq!(id.as_str(), "my_id");
    /// ```
    #[inline]
    pub fn as_str(&self) -> &str {
        self.inner.as_str()
    }

    /// Push additional components onto this ResourceId.
    ///
    /// A separator will automatically be added to the end of this ResourceId, and further
    /// components will be appended. If the current ResourceId is empty, the additional components
    /// will overwrite the current ResourceId.
    ///
    /// # Examples
    /// ```
    /// use gtether::resource::id::ResourceId;
    ///
    /// let mut id = ResourceId::new();
    /// id.push("my");
    /// assert_eq!(id, ResourceId::from("my"));
    /// id.push("long");
    /// assert_eq!(id, ResourceId::from("my/long"));
    /// id.push("path");
    /// assert_eq!(id, ResourceId::from("my/long/path"));
    /// ```
    pub fn push(&mut self, other: impl Into<ResourceId>) {
        let other = other.into();
        if self.inner.is_empty() {
            self.inner = other.inner;
            self.joints = other.joints;
        } else {
            self.joints.reserve(1 + other.joints.len());
            self.joints.push(self.inner.len());
            self.joints.extend(other.joints);
            self.inner.reserve(1 + other.inner.len());
            self.inner.push(Self::SEPARATOR);
            self.inner.push_str(other.inner.as_str());
        }
    }

    /// Join this ResourceId with additional components, and yield a new ResourceId.
    ///
    /// A separator will automatically be added to the end of this ResourceId, and further
    /// components will be appended. If the current ResourceId is empty, this method effectively
    /// yields the additional components only.
    ///
    /// # Examples
    /// ```
    /// use gtether::resource::id::ResourceId;
    ///
    /// let id = ResourceId::new();
    /// assert_eq!(id.join("value"), ResourceId::from("value"));
    /// let id = ResourceId::from("a");
    /// assert_eq!(id.join("b/c"), ResourceId::from("a/b/c"));
    /// ```
    pub fn join(&self, other: impl Into<ResourceId>) -> ResourceId {
        let mut clone = self.clone();
        clone.push(other);
        clone
    }

    /// Yields the ResourceId representing the parent components of this ResourceId.
    ///
    /// If this ResourceId has no parent components, yields an empty ResourceId.
    ///
    /// # Examples
    /// ```
    /// use gtether::resource::id::ResourceId;
    ///
    /// let id = ResourceId::from("base/value");
    /// assert_eq!(id.parent(), ResourceId::from("base"));
    /// assert_eq!(id.parent().parent(), ResourceId::from(""));
    /// assert_eq!(id.parent().parent().parent(), ResourceId::from(""));
    /// ```
    pub fn parent(&self) -> ResourceId {
        let mut parent_joints = self.joints.clone();
        if let Some(joint) = parent_joints.pop() {
            ResourceId {
                inner: self.inner[..joint].to_string(),
                joints: parent_joints,
            }
        } else {
            ResourceId {
                inner: String::new(),
                joints: parent_joints,
            }
        }
    }

    /// Yields the final component of this ResourceId.
    ///
    /// # Examples
    /// ```
    /// use gtether::resource::id::ResourceId;
    ///
    /// assert_eq!(ResourceId::from("base/value").name(), "value");
    /// assert_eq!(ResourceId::from("value").name(), "value");
    /// assert_eq!(ResourceId::new().name(), "");
    /// ```
    #[inline]
    pub fn name(&self) -> &str {
        self.components().next_back()
            .unwrap_or("")
    }
}

impl<I: Into<ResourceId>> Add<I> for ResourceId {
    type Output = ResourceId;

    #[inline]
    fn add(mut self, rhs: I) -> Self::Output {
        self.push(rhs);
        self
    }
}

impl<I: Into<ResourceId>> AddAssign<I> for ResourceId {
    #[inline]
    fn add_assign(&mut self, rhs: I) {
        self.push(rhs);
    }
}

impl Ord for ResourceId {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        // 'self.joints' is just metadata; optimize Ord by just using the str cmp()
        self.inner.cmp(&other.inner)
    }
}

impl PartialOrd for ResourceId {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ResourceId {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        // 'self.joints' is just metadata; optimize PartialEq by just using the str eq()
        self.inner.eq(&other.inner)
    }
}

impl Eq for ResourceId {}

impl Hash for ResourceId {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        // 'self.joints' is just metadata; optimize hashing by just using the str hash
        self.inner.hash(state)
    }
}

impl AsRef<str> for ResourceId {
    #[inline]
    fn as_ref(&self) -> &str {
        self.inner.as_ref()
    }
}

impl AsRef<Path> for ResourceId {
    #[inline]
    fn as_ref(&self) -> &Path {
        self.inner.as_str().as_ref()
    }
}

impl Debug for ResourceId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut builder = f.debug_tuple("ResourceId");
        for component in self.components() {
            builder.field(&component);
        }
        builder.finish()
    }
}

impl Display for ResourceId {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.inner, f)
    }
}

/// Iterator for [ResourceId] components.
///
/// See [ResourceId::components()].
pub struct Components<'id> {
    id: &'id ResourceId,
    joints: VecDeque<usize>,
    start_idx: Option<usize>,
    end_idx: Option<usize>,
}

impl<'id> Iterator for Components<'id> {
    type Item = &'id str;

    fn next(&mut self) -> Option<Self::Item> {
        let start_idx = self.start_idx?;
        let final_end_idx = self.end_idx?;
        let end_idx = self.joints.pop_front()
            .unwrap_or(final_end_idx);
        let component = &self.id.inner[start_idx..end_idx];
        // Technically this condition could skip a final empty component, but there shouldn't be any
        // empty components beyond a first
        if (end_idx+1) < final_end_idx {
            self.start_idx = Some(end_idx+1);
        } else {
            self.start_idx = None;
            self.end_idx = None;
        }
        Some(component)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let size = if self.start_idx.is_none() || self.end_idx.is_none() {
            0
        } else {
            self.joints.len() + 1
        };
        (size, Some(size))
    }
}

impl<'id> ExactSizeIterator for Components<'id> {}

impl<'id> DoubleEndedIterator for Components<'id> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let final_start_idx = self.start_idx?;
        let end_idx = self.end_idx?;
        let start_idx = match self.joints.pop_back() {
            Some(idx) => idx+1,
            None => final_start_idx,
        };
        let component = &self.id.inner[start_idx..end_idx];
        if start_idx > final_start_idx {
            self.end_idx = Some(start_idx-1);
        } else {
            self.start_idx = None;
            self.end_idx = None;
        }
        Some(component)
    }
}

impl<'id> FusedIterator for Components<'id> {}

impl From<&ResourceId> for ResourceId {
    #[inline]
    fn from(value: &ResourceId) -> Self { value.clone() }
}

impl From<String> for ResourceId {
    #[inline]
    fn from(value: String) -> Self { Self::sanitize_input(value) }
}

impl From<&String> for ResourceId {
    #[inline]
    fn from(value: &String) -> Self { Self::sanitize_input(value) }
}

impl From<&str> for ResourceId {
    #[inline]
    fn from(value: &str) -> Self { Self::sanitize_input(value) }
}

impl From<PathBuf> for ResourceId {
    #[inline]
    fn from(value: PathBuf) -> Self {
        Self::sanitize_input(value.to_string_lossy())
    }
}

impl From<ResourceId> for PathBuf {
    #[inline]
    fn from(value: ResourceId) -> Self {
        PathBuf::from(value.inner)
    }
}