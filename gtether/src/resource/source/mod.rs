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
//! [ResourceSubData]. [ResourceSubData] implements `From<Box<dyn AsyncRead>>`, so an
//! implementation only needs to get a `Box<dyn AsyncRead>`, and then use `.into()`.
//!
//! If a given source does not have data for a given [id][rp], it is expected to output an `Err`
//! wrapping [ResourceLoadError::NotFound].
//!
//! An example of a bare minimum source that simply yields the given id as raw bytes:
//! ```
//! use async_trait::async_trait;
//! use gtether::resource::path::ResourcePath;
//! use gtether::resource::source::{ResourceSource, ResourceSubDataResult, ResourceWatcher, SourceIndex};
//!
//! struct ReflectingSource {}
//!
//! #[async_trait]
//! impl ResourceSource for ReflectingSource {
//!     async fn load(&self, id: &ResourcePath) -> ResourceSubDataResult {
//!         Ok(Box::new(id.as_bytes()).into())
//!     }
//!
//!     fn watch(&self, id: ResourcePath, watcher: Box<dyn ResourceWatcher>, sub_idx: Option<SourceIndex>) { /* noop */ }
//!     fn unwatch(&self, id: &ResourcePath) { /* noop */ }
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
//! Sources that use constant data, such as the above example, do not need to worry about
//! [watch()][rsw]/[unwatch()][rsu], and can leave them as noop implementations. However, if your
//! source yields data that can change (i.e. from a file that can be modified), then your source
//! should implement [watch()][rsw]/[unwatch()][rsu], and store watch state as such. This involves
//! storing and notifying [ResourceWatchers][rw], which are used to notify when underlying data
//! changes.
//!
//! Example implementing [watch()][rsw]/[unwatch()][rsu]:
//! ```
//! use std::collections::HashMap;
//! use std::sync::Mutex;
//! use async_trait::async_trait;
//! use gtether::resource::path::ResourcePath;
//! use gtether::resource::source::{ResourceSource, ResourceSubDataResult, ResourceWatcher, SourceIndex};
//!
//! struct ChangingSource {
//!     watchers: Mutex<HashMap<ResourcePath, Box<dyn ResourceWatcher>>>,
//! }
//!
//! impl ChangingSource {
//!     // call ResourceWatcher::notify_update() when the data changes for an id
//! }
//!
//! #[async_trait]
//! impl ResourceSource for ChangingSource {
//!     async fn load(&self, id: &ResourcePath) -> ResourceSubDataResult {
//!         todo!("Some custom load operation")
//!     }
//!
//!     fn watch(&self, id: ResourcePath, watcher: Box<dyn ResourceWatcher>, _sub_idx: Option<SourceIndex>) {
//!         // _sub_idx can be ignored for this implementation
//!         self.watchers.lock().unwrap().insert(id, watcher)
//!     }
//!
//!     fn unwatch(&self, id: &ResourcePath) {
//!         self.watchers.lock().unwrap().remove(id)
//!     }
//! }
//! ```
//!
//! # Implementing a middleware source
//!
//! Sometimes it may be useful to implement a middleware source, e.g. when extending the behavior of
//! another source. In this case, some extra logic may need to be implemented, such as propagating
//! [watch()][rsw]/[unwatch()][rsu] calls to any sub-sources. Note that all methods should be
//! passed to sub-sources, including [ResourceSource::sub_load()], even if it's not directly used
//! by the wrapping source. This ensures any sub-sources can make use of
//! [ResourceSource::sub_load()], since its default implementation is to defer to
//! [ResourceSource::load()].
//!
//! Example wrapping a single sub-source:
//! ```
//! use async_trait::async_trait;
//! use gtether::resource::path::ResourcePath;
//! use gtether::resource::source::{ResourceSource, ResourceSubDataResult, ResourceWatcher, SourceIndex};
//!
//! struct WrapperSource<S: ResourceSource> {
//!     inner: S,
//! }
//!
//! #[async_trait]
//! impl<S: ResourceSource> ResourceSource for WrapperSource<S> {
//!     async fn load(&self, id: &ResourcePath) -> ResourceSubDataResult {
//!         // Do any custom wrapper logic
//!         let result = self.inner.load(id).await;
//!         // Possibly modify result if necessary
//!         result
//!     }
//!
//!     async fn sub_load(&self, id: &ResourcePath, sub_idx: &SourceIndex) -> ResourceSubDataResult {
//!         // Do any custom wrapper logic
//!         let result = self.inner.sub_load(id, sub_idx).await;
//!         // Possibly modify result if necessary
//!         result
//!     }
//!
//!     fn watch(&self, id: ResourcePath, watcher: Box<dyn ResourceWatcher>, sub_idx: Option<SourceIndex>) {
//!         self.inner.watch(id, watcher, sub_idx)
//!     }
//!
//!     fn unwatch(&self, id: &ResourcePath) {
//!         self.inner.unwatch(id)
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
//! use async_trait::async_trait;
//! use gtether::resource::path::ResourcePath;
//! use gtether::resource::ResourceLoadError;
//! use gtether::resource::source::{ResourceSource, ResourceSubData, ResourceSubDataResult, ResourceWatcher, SourceIndex};
//!
//! struct MultiSource { /* inner bits */ }
//!
//! impl MultiSource {
//!     fn sub_sources(&self) -> impl Iterator<Item=(usize, &dyn ResourceSource)> {
//!         todo!("Iterate over sub-sources")
//!     }
//!
//!     fn get_sub_source(&self, sub_idx: &SourceIndex) -> Result<&dyn ResourceSource, ResourceLoadError> {
//!         todo!("Yield a sub-source")
//!     }
//!
//!     fn find_sub_data(&self, id: &ResourcePath) -> Result<(usize, ResourceSubData), ResourceLoadError> {
//!         todo!("Yield data from a particular sub-source, as well as the sub-source's index")
//!     }
//! }
//!
//! #[async_trait]
//! impl ResourceSource for MultiSource {
//!     async fn load(&self, id: &ResourcePath) -> ResourceSubDataResult {
//!         let (idx, data) = self.find_sub_data(id)?;
//!         // The inner sub-data must be wrapped with its sub-sources index
//!         Ok(data.wrap(idx))
//!     }
//!
//!     async fn sub_load(&self, id: &ResourcePath, sub_idx: &SourceIndex) -> ResourceSubDataResult {
//!         let source = self.get_sub_source(sub_idx)?;
//!         match sub_idx.sub_idx() {
//!             Some(sub_idx) => source.sub_load(id, sub_idx).await,
//!             None => source.load(id).await,
//!         }
//!     }
//!
//!     fn watch(&self, id: ResourcePath, watcher: Box<dyn ResourceWatcher>, sub_idx: Option<SourceIndex>) {
//!         let sub_idx = sub_idx.unwrap_or(SourceIndex::max());
//!         for (idx, source) in self.sub_sources() {
//!             if idx <= sub_idx.idx() {
//!                 // Clone the watcher with relevant sub-indices for each sub-source
//!                 source.watch(
//!                     id.clone(),
//!                     watcher.clone_with_sub_index(idx.into()),
//!                     sub_idx.sub_idx().map(|inner| inner.clone()),
//!                 );
//!             } else {
//!                 source.unwatch(&id);
//!             }
//!         }
//!     }
//!
//!     fn unwatch(&self, id: &ResourcePath) {
//!         for (_, source) in self.sub_sources() {
//!             source.unwatch(id);
//!         }
//!     }
//! }
//! ```
//!
//! [rs]: ResourceSource
//! [rsw]: ResourceSource::watch()
//! [rsu]: ResourceSource::unwatch()
//! [rd]: ResourceReadData
//! [rp]: ResourcePath
//! [si]: SourceIndex
//! [rw]: ResourceWatcher
use async_trait::async_trait;
use smol::io::AsyncRead;
use std::cmp::Ordering;
use std::fmt::{Debug, Display, Formatter};

use crate::resource::path::ResourcePath;
use crate::resource::{ResourceLoadError, ResourceReadData};

pub mod list;
pub mod constant;

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
/// let total_index = total_index.with_sub_idx(None);
/// ```
///
/// [rs]: ResourceSource
#[derive(Debug, Clone)]
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

impl From<usize> for SourceIndex {
    fn from(value: usize) -> Self {
        Self::new(value)
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

// TODO: Refactor this such that the "sealed" data isn't public
pub struct ResourceData {
    pub data: ResourceReadData,
    pub source_idx: SourceIndex,
}

pub enum ResourceSubData {
    SubIndex(ResourceData),
    NoIndex(ResourceReadData),
}

impl ResourceSubData {
    pub fn seal(self, idx: usize) -> ResourceData {
        match self {
            ResourceSubData::SubIndex(r_data) => ResourceData {
                data: r_data.data,
                source_idx: SourceIndex::new(idx).with_sub_idx(Some(r_data.source_idx)),
            },
            ResourceSubData::NoIndex(r_data) => ResourceData {
                data: r_data,
                source_idx: SourceIndex::new(idx),
            },
        }
    }

    #[inline]
    pub fn wrap(self, idx: usize) -> Self {
        ResourceSubData::SubIndex(self.seal(idx))
    }
}

impl<R: AsyncRead + Unpin + Send + 'static> From<Box<R>> for ResourceSubData {
    #[inline]
    fn from(value: Box<R>) -> Self {
        Self::NoIndex(Box::pin(value))
    }
}

pub type ResourceSubDataResult = Result<ResourceSubData, ResourceLoadError>;
pub type ResourceDataResult = Result<ResourceData, ResourceLoadError>;

/// Watcher context to notify upstream managers of [raw data][rd] changes.
///
/// This context is passed to [ResourceSources][rs] so that they can notify upstream managers when
/// their data changes for specific [ids][rp]. This exists as a trait instead of a struct so that
/// sources can create their own middleware ResourceWatchers that can intercept update notifications
/// if they so desire.
///
/// [rd]: ResourceReadData
/// [rs]: ResourceSource
/// [rp]: ResourcePath
pub trait ResourceWatcher: Debug + Send + Sync + 'static {
    /// [ResourceSources][rs] call this when their data changes for a specific [id][rp].
    ///
    /// [rs]: ResourceSource
    /// [rp]: ResourcePath
    fn notify_update(&self, id: &ResourcePath);

    /// Clone this ResourceWatcher with the context of a specific sub-index.
    ///
    /// This specialized version of clone is intended to allow [ResourceSources][rs] to pass out
    /// modified watchers to sub-sources with the additional context of their sub-indices, so that
    /// when they notify upstream of updates, they can know their exact sub-index.
    ///
    /// [rs]: ResourceSource
    fn clone_with_sub_index(&self, sub_idx: SourceIndex) -> Box<dyn ResourceWatcher>;
}

/// User-defined source of [raw resource data][rd].
///
/// See [module-level][mod] docs for more details.
///
/// [rd]: ResourceReadData
/// [mod]: super::source
#[async_trait]
pub trait ResourceSource: Send + Sync + 'static {
    /// Given an [id][rp], attempt to retrieve the [raw data][rd] associated with it.
    ///
    /// [rp]: ResourcePath
    /// [rd]: ResourceReadData
    async fn load(&self, id: &ResourcePath) -> ResourceSubDataResult;

    /// Given an [id][rp] and [sub-idx][si], attempt to retrieve the [raw data][rd] associated with it.
    ///
    /// This version is intended to be used when a [source][rs] notifies that it has a data update.
    /// In that case, this method will be called with the [sub-idx][si] that [source][rs]
    /// reported with.
    ///
    /// Default implementation of this method is to delegate to [Self::load()].
    ///
    /// [rp]: ResourcePath
    /// [si]: SourceIndex
    /// [rd]: ResourceReadData
    /// [rs]: ResourceSource
    async fn sub_load(&self, id: &ResourcePath, _sub_idx: &SourceIndex) -> ResourceSubDataResult {
        self.load(id).await
    }

    /// Watch a given [id][rp] for data changes.
    ///
    /// For [sources][rs] that use sub-indices, a [sub-index][si] may also be given. If the
    /// sub-index is Some, then any sources that belong to lesser than or equal sub-indices are
    /// expected to be given copies of the watcher, and any sources that belong to greater
    /// sub-indices are expected to be [unwatched](Self::unwatch()).
    ///
    /// [rp]: ResourcePath
    /// [rs]: ResourceSource
    /// [si]: SourceIndex
    fn watch(&self, id: ResourcePath, watcher: Box<dyn ResourceWatcher>, sub_idx: Option<SourceIndex>);

    /// Unwatch a given [id][rp] for data changes.
    ///
    /// An [id][rp] that is unwatched is expected to no longer report data changes, and any relevant
    /// watchers that have been stored for said [id][rp] are expected to be purged.
    ///
    /// [rp]: ResourcePath
    fn unwatch(&self, id: &ResourcePath);
}

impl<S: ResourceSource> From<S> for Box<dyn ResourceSource> {
    #[inline]
    fn from(value: S) -> Self {
        Box::new(value)
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