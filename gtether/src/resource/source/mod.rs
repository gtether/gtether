//! Logic related to loading raw resource data from user-defined sources.
//!
//! [ResourceSources][rs] are responsible for two main actions: providing [raw data][rd] in the form
//! of async data streams, and alerting upstream resource management when the underlying data they
//! are responsible for changes. The latter is done via [sources][rs] "watching" certain [ids][rp],
//! so that when the data for said [id][rp] changes - i.e. due to a file being modified or a remote
//! data source updating itself - the [source][rs] can notify upstream of said changes.
//!
//! [ResourceSources][rs] can be layered, with one source containing many others. In order to
//! facilitate this, sources use [SourceIndices][si] to represent what source any given
//! [raw data][rd] came from. [SourceIndices][si] are nested indices, that are largely controlled
//! by [sources][rs] themselves. See the documentation for [SourceIndex][si] for more.
//!
//! This module contains several pre-defined sources to use, covering cases such as sourcing from
//! static application data, or utility layers such as combining multiple sub-sources under one
//! sub-source.
//!
//! # Implementing a custom source
//!
//! All custom sources need to implement the async [ResourceSource::load()] method, which accepts
//! an [id][rp] and outputs a result possibly containing [raw data][rd] in the form of
//! [ResourceData]. [ResourceData] implements `From<Box<dyn AsyncRead>>`, so an
//! implementation only needs to get a `Box<dyn AsyncRead>`, and then use `.into()`.
//!
//! If a given source does not have data for a given [id][rp], it is expected to output an `Err`
//! wrapping [ResourceLoadError::NotFound].
//!
//! All resource sources should also maintain a [ResourceWatcher], and provide a reference to said
//! watcher. This is used to keep track of what resources are currently requested from the source.
//! See the [`watcher`](crate::resource::watcher) module for more.
//!
//! An example of a bare minimum source that simply yields the given id as raw bytes:
//! ```
//! use async_trait::async_trait;
//! use gtether::resource::id::ResourceId;
//! use gtether::resource::source::{ResourceData, ResourceDataResult , ResourceDataSource, ResourceSource};
//! use gtether::resource::watcher::ResourceWatcher;
//! use smol::io::Cursor;
//!
//! struct ReflectingSource {
//!     watcher: ResourceWatcher,
//! }
//!
//! #[async_trait]
//! impl ResourceSource for ReflectingSource {
//!     fn hash(&self, id: &ResourceId) -> Option<ResourceDataSource> {
//!         Some(ResourceDataSource::new(
//!             // The hash can simply be the ID, since the value won't change for the same ID
//!             id.to_string().clone()
//!         ))
//!     }
//!
//!     async fn load(&self, id: &ResourceId) -> ResourceDataResult {
//!         Ok(ResourceData::new(
//!             Box::new(Cursor::new(id.to_string().into_bytes())),
//!             // The hash can simply be the ID, since the value won't change for the same ID
//!             id.to_string().clone(),
//!         ))
//!     }
//!
//!     fn watcher(&self) -> &ResourceWatcher {
//!         &self.watcher
//!     }
//! }
//! ```
//!
//! <div class="warning">
//!
//! Note that implementations of [ResourceSource] must use the [async_trait] attribute macro. See
//! that crate for why this is needed.
//!
//! </div>
//!
//! # Implementing a middleware source
//!
//! Sometimes it may be useful to implement a middleware source, e.g. when extending the behavior of
//! another source. In this case, some extra logic may need to be implemented, such as maintaining
//! the child watchers for the middleware's watcher. Note that all methods should be passed to
//! sub-sources, including [ResourceSource::sub_load()], even if it's not directly used by the
//! wrapping source. This ensures any sub-sources can make use of [ResourceSource::sub_load()],
//! since its default implementation is to defer to [ResourceSource::load()].
//!
//! Example wrapping a single sub-source:
//! ```
//! use ahash::HashSet;
//! use async_trait::async_trait;
//! use gtether::resource::id::ResourceId;
//! use gtether::resource::source::{ResourceSource, ResourceDataResult, SourceIndex, ResourceDataSource};
//! use gtether::resource::watcher::ResourceWatcher;
//!
//! struct WrapperSource<S: ResourceSource> {
//!     watcher: ResourceWatcher,
//!     inner: S,
//! }
//!
//! impl<S: ResourceSource> WrapperSource<S> {
//!     fn new(source: S) -> Self {
//!         let watcher = ResourceWatcher::new(());
//!         // The wrapped source's watcher needs to be added to this source's watcher
//!         watcher.push_child(source.watcher());
//!         Self {
//!             watcher,
//!             inner: source,
//!         }
//!     }
//! }
//!
//! #[async_trait]
//! impl<S: ResourceSource> ResourceSource for WrapperSource<S> {
//!     fn hash(&self, id: &ResourceId) -> Option<ResourceDataSource> {
//!         // Do any custom wrapper logic
//!         let hash = self.inner.hash(id);
//!         // Possibly modify result if necessary
//!         hash
//!     }
//!
//!     async fn load(&self, id: &ResourceId) -> ResourceDataResult {
//!         // Do any custom wrapper logic
//!         let result = self.inner.load(id).await;
//!         // Possibly modify result if necessary
//!         result
//!     }
//!
//!     async fn sub_load(&self, id: &ResourceId, sub_idx: &SourceIndex) -> ResourceDataResult {
//!         // Do any custom wrapper logic
//!         let result = self.inner.sub_load(id, sub_idx).await;
//!         // Possibly modify result if necessary
//!         result
//!     }
//!
//!     fn watcher(&self) -> &ResourceWatcher {
//!         &self.watcher
//!     }
//! }
//! ```
//!
//! If wrapping multiple other sources, [SourceIndex][si] sub-indices may be necessary, in order to
//! distinguish between nested sources. It is also necessary to implement
//! [ResourceSource::sub_load()], which is used for loading raw data from a particular sub-source.
//!
//! Example using sub-indices with multiple sub-sources:
//! ```
//! use ahash::HashSet;
//! use async_trait::async_trait;
//! use gtether::resource::id::ResourceId;
//! use gtether::resource::ResourceLoadError;
//! use gtether::resource::source::{ResourceSource, ResourceData, ResourceDataResult, SourceIndex, ResourceDataSource};
//! use gtether::resource::watcher::ResourceWatcher;
//!
//! struct MultiSource<S: ResourceSource> {
//!     watcher: ResourceWatcher,
//!     sub_sources: Vec<S>,
//! }
//!
//! impl<S: ResourceSource> MultiSource<S> {
//!     fn new(sub_sources: impl IntoIterator<Item=S>) -> Self {
//!         let watcher = ResourceWatcher::new(());
//!         let sub_sources = sub_sources.into_iter().collect::<Vec<_>>();
//!         for sub_source in &sub_sources {
//!             // All sub-source watchers need to be added to this source's watcher
//!             watcher.push_child(sub_source.watcher());
//!         }
//!         Self {
//!             watcher,
//!             sub_sources,
//!         }
//!     }
//!
//!     fn find_sub_hash(&self, id: &ResourceId) -> Option<(usize, ResourceDataSource)> {
//!         unimplemented!("Yield a hash from a particular sub-source, as well as the sub-source's index")
//!     }
//!
//!     fn find_sub_data(&self, id: &ResourceId) -> Result<(usize, ResourceData), ResourceLoadError> {
//!         unimplemented!("Yield data from a particular sub-source, as well as the sub-source's index")
//!     }
//! }
//!
//! #[async_trait]
//! impl<S: ResourceSource> ResourceSource for MultiSource<S> {
//!     fn hash(&self, id: &ResourceId) -> Option<ResourceDataSource> {
//!         let (idx, hash) = self.find_sub_hash(id)?;
//!         // The inner sub-hash must be wrapped with its sub-sources index
//!         Some(hash.wrap(idx))
//!     }
//!
//!     async fn load(&self, id: &ResourceId) -> ResourceDataResult {
//!         let (idx, data) = self.find_sub_data(id)?;
//!         // The inner sub-data must be wrapped with its sub-sources index
//!         Ok(data.wrap(idx))
//!     }
//!
//!     async fn sub_load(&self, id: &ResourceId, sub_idx: &SourceIndex) -> ResourceDataResult {
//!         let source = self.sub_sources.get(sub_idx.idx())
//!             .ok_or(ResourceLoadError::NotFound(id.clone()))?;
//!         match sub_idx.sub_idx() {
//!             Some(sub_idx) => source.sub_load(id, sub_idx).await,
//!             None => source.load(id).await,
//!         }
//!     }
//!
//!     fn watcher(&self) -> &ResourceWatcher {
//!         &self.watcher
//!     }
//! }
//! ```
//!
//! # Data Hashing
//!
//! Resource management uses hashing to identify raw data in a compact way. These hashes are used
//! for many parts of the resource management lifecycle, including determining if a notified update
//! can be ignored (i.e. if hashes match), or even for middleware caching (e.g. for a middleware
//! source that caches remote data on-disk to prevent repeated downloads).
//!
//! It is highly recommended that a consistent hashing scheme is kept across all sources that are
//! used. If two sources use a different hashing scheme for the same data, then that data will be
//! treated as two separate sets of data, one from each source, which can cause caching to not
//! function as well as it should.
//!
//! All pre-provided sources that this module provides use SHA256 hashing, encoded using base64 to
//! turn it into a String. At this point in time, there are no ways to tell these sources to use a
//! different hashing scheme, so it is recommended that all user-defined sources also use SHA256
//! with base64 encoding, unless all of your sources are custom, and you can ensure they use the
//! same hashing scheme.
//!
//! [rs]: ResourceSource
//! [rsw]: ResourceSource::watch()
//! [rsu]: ResourceSource::unwatch()
//! [rd]: ResourceReadData
//! [rp]: ResourceId
//! [si]: SourceIndex
//! [rw]: ResourceUpdaterTrait

use ahash::HashMap;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use smol::io::AsyncRead;
use std::cmp::Ordering;
use std::fmt::{Debug, Display, Formatter};

use crate::resource::id::ResourceId;
use crate::resource::watcher::ResourceWatcher;
use crate::resource::{ResourceLoadError, ResourceReadData};

pub mod constant;
pub mod dynamic;
pub mod fs;
pub mod list;

#[derive(Debug, Clone)]
enum SubIndex {
    None,
    Some(Box<SourceIndex>),
    Min,
    Max,
}

/// Multi-layered index type that can be used to represent nested [ResourceSources][rs].
///
/// Comparable index type that can nest sub-indices. SourceIndices that have the same index will
/// check their sub-indices for further comparison, if they have one. If only one SourceIndex has
/// a sub-index, the SourceIndex without a sub-index takes priority (is ordered first).
///
/// # Examples
/// ```
/// use gtether::resource::source::SourceIndex;
///
/// let source_index = SourceIndex::new(1);
/// let source_index: SourceIndex = 2.into();
/// ```
///
/// With sub-indices:
/// ```
/// use gtether::resource::source::SourceIndex;
///
/// let base_index = SourceIndex::new(0);
/// let total_index = base_index.with_sub_idx(Some(3));
///
/// let other_index = SourceIndex::new(4);
/// let total_index_2 = total_index.clone().with_sub_idx(Some(other_index));
/// // Remove the sub-index
/// let total_index = total_index.with_sub_idx(None::<SourceIndex>);
/// ```
///
/// [rs]: ResourceSource
#[derive(Clone)]
pub struct SourceIndex {
    idx: usize,
    sub_idx: SubIndex,
}

impl SourceIndex {
    /// Create a minimum SourceIndex.
    #[inline]
    pub fn min() -> Self {
        Self {
            idx: usize::MIN,
            sub_idx: SubIndex::Min,
        }
    }

    /// Create a maximum SourceIndex.
    #[inline]
    pub fn max() -> Self {
        Self {
            idx: usize::MAX,
            sub_idx: SubIndex::Max,
        }
    }

    /// Create a new SourceIndex from an usize.
    ///
    /// SourceIndex also implements From<usize>.
    #[inline]
    pub fn new(idx: usize) -> Self {
        Self {
            idx,
            sub_idx: SubIndex::None,
        }
    }

    /// Consume this SourceIndex to create a new one with a given sub-index.
    ///
    /// Can erase the sub-index by specifying None.
    #[inline]
    pub fn with_sub_idx(mut self, sub_idx: Option<impl Into<SourceIndex>>) -> Self {
        self.sub_idx = match sub_idx {
            Some(sub_idx) => SubIndex::Some(Box::new(sub_idx.into())),
            None => SubIndex::None,
        };
        self
    }

    /// Get the underlying index of this SourceIndex (ignoring sub-indices).
    #[inline]
    pub fn idx(&self) -> usize { self.idx }

    /// Get the sub-index of this SourceIndex, if any.
    #[inline]
    pub fn sub_idx(&self) -> Option<&SourceIndex> {
        match &self.sub_idx {
            SubIndex::Some(sub_idx) => Some(Box::as_ref(sub_idx)),
            _ => None,
        }
    }
}

impl Display for SourceIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.sub_idx {
            SubIndex::Some(sub_idx) => {
                write!(f, "{}:", self.idx)?;
                Display::fmt(sub_idx, f)
            },
            SubIndex::None => write!(f, "{}", self.idx),
            SubIndex::Min => write!(f, "{}:MIN", self.idx),
            SubIndex::Max => write!(f, "{}:MAX", self.idx),
        }
    }
}

impl Debug for SourceIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "SourceIndex(")?;
        Display::fmt(self, f)?;
        write!(f, ")")
    }
}

impl From<usize> for SourceIndex {
    #[inline]
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

impl FromIterator<usize> for SourceIndex {
    fn from_iter<T: IntoIterator<Item=usize>>(iter: T) -> Self {
        // need to collect into a vec first so we can reverse it
        let mut indices = iter.into_iter().collect::<Vec<_>>().into_iter().rev();
        let first_idx = indices.next().unwrap();
        let mut src_idx = Self::new(first_idx);
        for idx in indices {
            src_idx = Self::new(idx).with_sub_idx(Some(src_idx));
        }
        src_idx
    }
}

impl<const N: usize> From<[usize; N]> for SourceIndex {
    #[inline]
    fn from(value: [usize; N]) -> Self {
        value.into_iter().collect()
    }
}

impl PartialEq for SourceIndex {
    fn eq(&self, other: &Self) -> bool {
        if self.idx == other.idx {
            match (&self.sub_idx, &other.sub_idx) {
                (SubIndex::Some(sub_a), SubIndex::Some(sub_b)) => {
                    sub_a == sub_b
                },
                (SubIndex::Min, SubIndex::Min) => true,
                (SubIndex::Max, SubIndex::Max) => true,
                (SubIndex::None, SubIndex::None) => true,
                _ => false,
            }
        } else {
            false
        }
    }
}

impl Eq for SourceIndex {}

impl Ord for SourceIndex {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.idx.cmp(&other.idx) {
            Ordering::Equal => {
                match (&self.sub_idx, &other.sub_idx) {
                    (SubIndex::Some(sub_a), SubIndex::Some(sub_b)) => {
                        sub_a.cmp(sub_b)
                    },
                    (SubIndex::Min, SubIndex::Min) => Ordering::Equal,
                    (SubIndex::Max, SubIndex::Max) => Ordering::Equal,
                    (SubIndex::Min, _) | (_, SubIndex::Max) => Ordering::Less,
                    (_, SubIndex::Min) | (SubIndex::Max, _) => Ordering::Greater,
                    (SubIndex::None, SubIndex::None) => Ordering::Equal,
                    (SubIndex::None, SubIndex::Some(_)) => Ordering::Less,
                    (SubIndex::Some(_), SubIndex::None) => Ordering::Greater,
                }
            },
            order => order,
        }
    }
}

impl PartialOrd for SourceIndex {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(in crate::resource) struct SealedResourceDataSource {
    pub hash: String,
    pub idx: SourceIndex,
}

pub(in crate::resource) struct SealedResourceData {
    pub data: ResourceReadData,
    pub source: SealedResourceDataSource,
}

/// Pair of string hash and [SourceIndex] that represents a "source" of some data.
#[derive(Debug, Clone)]
pub struct ResourceDataSource {
    hash: String,
    idx: Option<SourceIndex>,
}

impl ResourceDataSource {
    #[inline]
    pub fn new(hash: String) -> Self {
        Self {
            hash,
            idx: None,
        }
    }

    pub(in crate::resource) fn seal(self, idx: usize) -> SealedResourceDataSource {
        match self.idx {
            Some(sub_idx) => SealedResourceDataSource {
                hash: self.hash,
                idx: SourceIndex::new(idx).with_sub_idx(Some(sub_idx)),
            },
            None => SealedResourceDataSource {
                hash: self.hash,
                idx: SourceIndex::new(idx),
            }
        }
    }

    /// Wrap this ResourceDataSource with another index.
    ///
    /// The resulting ResourceDataSource will use the given index as its top-level index in
    /// [SourceIndex].
    #[inline]
    pub fn wrap(mut self, idx: usize) -> Self {
        self.idx = Some(match self.idx {
            Some(sub_idx) => SourceIndex::new(idx).with_sub_idx(Some(sub_idx)),
            None => SourceIndex::new(idx),
        });
        self
    }
}

/// Struct bundling raw resource data and the [SourceIndex] it came from.
pub struct ResourceData {
    data: ResourceReadData,
    source: ResourceDataSource,
}

impl ResourceData {
    /// Create ResourceData from raw data.
    ///
    /// It is recommended to use a SHA256 hash of the data as the hash. See
    /// [module-level documentation][mod] for more.
    ///
    /// [more]: super::source#data-hashing
    pub fn new(data: Box<dyn AsyncRead + Unpin + Send + 'static>, hash: String) -> Self {
        Self {
            data: Box::into_pin(data),
            source: ResourceDataSource::new(hash),
        }
    }

    pub(in crate::resource) fn seal(self, idx: usize) -> SealedResourceData {
        SealedResourceData {
            data: self.data,
            source: self.source.seal(idx),
        }
    }

    /// Wrap this ResourceData with another index.
    ///
    /// The resulting ResourceData will use the given index as its top-level index in [SourceIndex].
    #[inline]
    pub fn wrap(mut self, idx: usize) -> Self {
        self.source = self.source.wrap(idx);
        self
    }
}

pub type ResourceDataResult = Result<ResourceData, ResourceLoadError>;
pub(in crate::resource) type SealedResourceDataResult = Result<SealedResourceData, ResourceLoadError>;

/// Update type for [ResourceUpdaters](ResourceUpdaterTrait).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceUpdate {
    /// A resource was added with the specified [hash](super::source#data-hashing).
    Added(String),

    /// A resource was modified with the specified [hash](super::source#data-hashing).
    Modified(String),

    /// A resource was removed, and is no longer available.
    Removed,

    /// A resource moved from the specified [SourceIndex] to the new one specified by
    /// [BulkResourceUpdate].
    MovedSourceIndex(SourceIndex),
}

/// Bulk collection of updates for a single update timestamp.
///
/// Can be easily created via `HashMap<ResourceId, ResourceUpdate>::into()`.
#[derive(Debug, Clone)]
pub struct BulkResourceUpdate{
    /// The timestamp of this bulk update.
    pub timestamp: DateTime<Utc>,
    /// Map of IDs to updates.
    pub updates: HashMap<ResourceId, ResourceUpdate>,
}

impl From<HashMap<ResourceId, ResourceUpdate>> for BulkResourceUpdate {
    #[inline]
    fn from(updates: HashMap<ResourceId, ResourceUpdate>) -> Self {
        Self {
            timestamp: Utc::now(),
            updates,
        }
    }
}

impl From<(ResourceId, ResourceUpdate)> for BulkResourceUpdate {
    #[inline]
    fn from(value: (ResourceId, ResourceUpdate)) -> Self {
        let mut updates = HashMap::default();
        updates.insert(value.0, value.1);
        Self {
            timestamp: Utc::now(),
            updates,
        }
    }
}

impl FromIterator<(ResourceId, ResourceUpdate)> for BulkResourceUpdate {
    #[inline]
    fn from_iter<T: IntoIterator<Item=(ResourceId, ResourceUpdate)>>(iter: T) -> Self {
        Self {
            timestamp: Utc::now(),
            updates: iter.into_iter().collect(),
        }
    }
}

/// User-defined source of [raw resource data][rd].
///
/// See [module-level][mod] docs for more details.
///
/// [rd]: ResourceReadData
/// [mod]: super::source
#[async_trait]
pub trait ResourceSource: Send + Sync + 'static {
    /// Given an [id](ResourceId), attempt to retrieve the [hash](super::source#data-hashing)
    /// associated with it.
    ///
    /// If this source doesn't have data associated with the given id, yields `None`.
    fn hash(&self, id: &ResourceId) -> Option<ResourceDataSource>;

    /// Given an [id][rp], attempt to retrieve the [raw data][rd] associated with it.
    ///
    /// [rp]: ResourceId
    /// [rd]: ResourceReadData
    async fn load(&self, id: &ResourceId) -> ResourceDataResult;

    /// Given an [id][rp] and [sub-idx][si], attempt to retrieve the [raw data][rd] associated with it.
    ///
    /// This version is intended to be used when a [source][rs] notifies that it has a data update.
    /// In that case, this method will be called with the [sub-idx][si] that [source][rs]
    /// reported with.
    ///
    /// Default implementation of this method is to delegate to [Self::load()].
    ///
    /// [rp]: ResourceId
    /// [si]: SourceIndex
    /// [rd]: ResourceReadData
    /// [rs]: ResourceSource
    // TODO: Is this still needed?
    async fn sub_load(&self, id: &ResourceId, _sub_idx: &SourceIndex) -> ResourceDataResult {
        self.load(id).await
    }

    fn watcher(&self) -> &ResourceWatcher;
}

#[async_trait]
impl<S: ResourceSource + ?Sized> ResourceSource for Box<S> {
    #[inline]
    fn hash(&self, id: &ResourceId) -> Option<ResourceDataSource> {
        (**self).hash(id)
    }

    #[inline]
    async fn load(&self, id: &ResourceId) -> ResourceDataResult {
        (**self).load(id).await
    }

    #[inline]
    async fn sub_load(&self, id: &ResourceId, sub_idx: &SourceIndex) -> ResourceDataResult {
        (**self).sub_load(id, sub_idx).await
    }

    #[inline]
    fn watcher(&self) -> &ResourceWatcher {
        (**self).watcher()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sub_idx_cmp() {
        assert_eq!(SourceIndex::new(0), SourceIndex::new(0));
        assert_eq!(SourceIndex::new(42), SourceIndex::new(42));
        assert_eq!(SourceIndex::new(1).with_sub_idx(Some(2)), SourceIndex::new(1).with_sub_idx(Some(2)));

        assert!(SourceIndex::new(3) < SourceIndex::new(5));
        assert!(SourceIndex::new(5) > SourceIndex::new(3));
        assert!(SourceIndex::new(4) < SourceIndex::new(4).with_sub_idx(Some(42)));
        assert!(SourceIndex::new(4).with_sub_idx(Some(42)) > SourceIndex::new(4));
    }

    #[test]
    fn test_sub_idx_cmp_min() {
        assert_eq!(SourceIndex::min(), SourceIndex::min());

        assert!(SourceIndex::min() < SourceIndex::new(usize::MIN));
        assert!(SourceIndex::min() < SourceIndex::new(1));
        assert!(SourceIndex::min() < SourceIndex::new(0).with_sub_idx(Some(0)));
        assert!(SourceIndex::min() < SourceIndex::new(0).with_sub_idx(Some(SourceIndex::min())));
        assert!(SourceIndex::min() < SourceIndex::max());

        assert!(SourceIndex::new(usize::MIN) > SourceIndex::min());
        assert!(SourceIndex::new(1) > SourceIndex::min());
        assert!(SourceIndex::new(0).with_sub_idx(Some(0)) > SourceIndex::min());
        assert!(SourceIndex::new(0).with_sub_idx(Some(SourceIndex::min())) > SourceIndex::min());
        assert!(SourceIndex::max() > SourceIndex::min());
    }

    #[test]
    fn test_sub_idx_cmp_max() {
        assert_eq!(SourceIndex::max(), SourceIndex::max());

        assert!(SourceIndex::max() > SourceIndex::new(usize::MAX));
        assert!(SourceIndex::max() > SourceIndex::new(1));
        assert!(SourceIndex::max() > SourceIndex::new(0).with_sub_idx(Some(0)));
        assert!(SourceIndex::max() > SourceIndex::new(0).with_sub_idx(Some(SourceIndex::max())));
        assert!(SourceIndex::max() > SourceIndex::min());

        assert!(SourceIndex::new(usize::MAX) < SourceIndex::max());
        assert!(SourceIndex::new(1) < SourceIndex::max());
        assert!(SourceIndex::new(0).with_sub_idx(Some(0)) < SourceIndex::max());
        assert!(SourceIndex::new(0).with_sub_idx(Some(SourceIndex::max())) < SourceIndex::max());
        assert!(SourceIndex::min() < SourceIndex::max());
    }
}