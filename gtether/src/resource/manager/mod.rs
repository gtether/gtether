//! The [ResourceManager] and any supporting logic.
//!
//! The [ResourceManager] can be considered the central system that ties all components of
//! [Resources][res] together.
//!
//! [Resources][res] managed in the [ResourceManager] are cached using weak references - if any
//! given [id][rp] is requested from the [ResourceManager] more than once, further requests will
//! yield a reference to the originally loaded [Resource][res], as long as at least one strong
//! reference to said [Resource][res] is maintained outside the [ResourceManager].
//!
//! When an [id][rp] is requested from the [ResourceManager], an asynchronous [handle][rf] is
//! returned instead of a direct reference to a [Resource][res]. This [handle][rf] can be used to
//! await the loading of a [Resource][res] if it has not finished loading yet, or will simply yield
//! a cached [Resource][res] reference.
//!
//! # Examples
//! Load a [Resource][res]
//! ```
//! use gtether::resource::manager::LoadPriority;
//! # use gtether::resource::manager::ResourceManager;
//! # use gtether::resource::ResourceLoader;
//!
//! # fn wrapper<T: Send + Sync + 'static>(manager: &ResourceManager, loader1: impl ResourceLoader<T>, loader2: impl ResourceLoader<T>) {
//! // Poll whether the resource is ready
//! let res_handle = manager.get_with_loader("key1", loader1);
//! match res_handle.check() {
//!     Ok(res_result) => { /* do something with the load result */ },
//!     Err(res_handle) => { /* do something with the unready handle */ },
//! }
//!
//! // Wait for the resource to be ready
//! let res_handle = manager.get_with_loader("key2", loader2);
//! // Or .wait().await if within an async context
//! let res_result = res_handle.wait();
//! # }
//! ```
//!
//! Multiple handles to one resource
//! ```
//! use std::sync::Arc;
//! use gtether::resource::manager::LoadPriority;
//! # use gtether::resource::manager::ResourceManager;
//! # use gtether::resource::ResourceLoader;
//!
//! # async fn wrapper<T, R>(manager: &ResourceManager, loader1: R, loader2: R, loader3: R)
//! # where T: Send + Sync + 'static, R: ResourceLoader<T> {
//! let handle1 = manager.get_with_loader("key", loader1);
//! let handle2 = manager.get_with_loader("key", loader2);
//! let handle3 = manager.get_with_loader("key", loader3);
//!
//! // Multiple handles should refer to same resource
//! let res1 = handle1.await.unwrap();
//! let res2 = handle2.await.unwrap();
//! assert!(Arc::ptr_eq(&res1, &res2));
//!
//! // Dropping all strong references should cause remaining handles to trigger reloads
//! drop(res1);
//! drop(res2);
//! // This should trigger a new load
//! let res3 = handle3.await.unwrap();
//! # }
//! ```
//!
//! [res]: Resource
//! [rp]: ResourceId
//! [rf]: ResourceFuture
use parking_lot::{Mutex, MutexGuard, RwLock};
use smol::future;
use smol::prelude::*;
use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tracing::{debug, info_span, warn, Instrument};

use crate::resource::id::ResourceId;
use crate::resource::manager::cache::{CacheEntry, CacheEntryGetResult, CacheEntryUpdateFutResult, CacheEntryUpdateResult};
use crate::resource::manager::dependency::{DependencyGraph, DependencyGraphLayer};
use crate::resource::manager::executor::{ManagerExecutor, ManagerTask, ResourceTaskData, TaskPriority};
use crate::resource::source::{ResourceSource, ResourceUpdate, ResourceWatcher, SealedResourceDataResult, SealedResourceDataSource, SourceIndex};
use crate::resource::{Resource, ResourceDefaultLoader, ResourceLoadError, ResourceLoader};

mod cache;
mod dependency;
mod executor;

#[derive(Debug, Clone)]
struct ManagerResourceWatcher {
    manager: Weak<ResourceManager>,
    idx: SourceIndex,
}

impl ResourceWatcher for ManagerResourceWatcher {
    #[inline]
    fn notify_update(&self, id: &ResourceId, update: ResourceUpdate) {
        if let Some(manager) = self.manager.upgrade() {
            manager.update(id.clone(), self.idx.clone(), update);
        }
    }

    #[inline]
    fn clone_with_sub_index(&self, sub_idx: SourceIndex) -> Box<dyn ResourceWatcher> {
        Box::new(Self {
            manager: self.manager.clone(),
            idx: self.idx.clone().with_sub_idx(Some(sub_idx)),
        })
    }
}

pub type ResourceLoadResult<T> = Result<Arc<Resource<T>>, ResourceLoadError>;

/// Priority for resource loading tasks.
///
/// Higher priorities will be chosen first when the internal async executor picks tasks to execute.
#[derive(Debug, Clone, Copy)]
pub enum LoadPriority {
    /// Load this resource as soon as possible.
    Immediate,
    /// This resource is not needed immediately, so load it when there is time.
    Delayed,
}

impl Default for LoadPriority {
    #[inline]
    fn default() -> Self { LoadPriority::Immediate }
}

impl Display for LoadPriority {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadPriority::Immediate => f.write_str("Immediate"),
            LoadPriority::Delayed => f.write_str("Delayed"),
        }
    }
}

enum ResourceFutureState<T: ?Sized + Send + Sync + 'static> {
    Immediate(ResourceLoadResult<T>),
    Delayed{
        cache: Weak<CacheEntry>,
        load_fn: Box<dyn (FnOnce() -> ResourceFuture<T>) + Send + Sync>,
    },
    Awaiting{
        future: Pin<Box<dyn Future<Output = CacheEntryGetResult<T>> + Send>>,
        load_fn: Box<dyn (FnOnce() -> ResourceFuture<T>) + Send + Sync>,
    },
    Uninit,
}

impl<T: ?Sized + Send + Sync + 'static> ResourceFutureState<T> {
    fn poll(self, cx: &mut Context<'_>) -> (ResourceFutureState<T>, Poll<ResourceLoadResult<T>>) {
        match self {
            Self::Immediate(inner) => (Self::Uninit, Poll::Ready(inner)),
            Self::Delayed { cache, load_fn } => {
                if let Some(strong_cache) = cache.upgrade() {
                    Self::Awaiting {
                        future: Box::pin(async move { strong_cache.wait::<T>().await }),
                        load_fn,
                    }.poll(cx)
                } else {
                    // CacheEntry expired, reload
                    load_fn().state.poll(cx)
                }
            },
            Self::Awaiting { mut future, load_fn } => {
                match future.poll(cx) {
                    Poll::Ready(cache_result) => match cache_result {
                        CacheEntryGetResult::Ok(r)
                            => (Self::Uninit, Poll::Ready(Ok(r))),
                        CacheEntryGetResult::Err(e)
                            => (Self::Uninit, Poll::Ready(Err(e))),
                        // CacheEntry expired, reload
                        CacheEntryGetResult::Expired => load_fn().state.poll(cx),
                    },
                    Poll::Pending => (Self::Awaiting { future, load_fn }, Poll::Pending),
                }
            },
            Self::Uninit => panic!("Found uninit state in inner ResourceFuture"),
        }
    }
}

/// Handle for retrieving results when loading a [Resource][res].
///
/// Note that ResourceFutures are not Futures in terms of the async trait, but rather are just
/// handles to an async task that loads a [Resource][res], or an already cached [Resource][res].
///
/// Two (or more) handles can be created that refer to the same load task / cached [Resource][res],
/// simply by asking the [ResourceManager][rm] for the same [id][rp] multiple times. In this case, if
/// one handle resolves to a [Resource][res], and then drops that [Resource][res], the [Resource][res]
/// may be dropped in the [ResourceManager's][rm] cache. Any other handles that then try to resolve
/// will trigger a reload of said [Resource][res], using the [ResourceLoader][rl] provided when that
/// handle was generated.
///
/// [res]: Resource
/// [id]: ResourceId
/// [rl]: ResourceLoader
/// [rm]: ResourceManager
pub struct ResourceFuture<T: ?Sized + Send + Sync + 'static> {
    state: ResourceFutureState<T>,
}

impl<T> Debug for ResourceFuture<T>
where
    T: ?Sized + Debug + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        struct ImmediateOk();
        impl Debug for ImmediateOk {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.write_str("Ok(<resource>)")
            }
        }

        struct ImmediateErr<'a>(&'a ResourceLoadError);
        impl Debug for ImmediateErr<'_> {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.write_str("Err(")?;
                Debug::fmt(&self.0, f)?;
                f.write_str(")")
            }
        }

        struct Delayed<'a>(&'a Weak<CacheEntry>);
        impl Debug for Delayed<'_> {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                if self.0.strong_count() > 0 {
                    f.write_str("<cache>")
                } else {
                    f.write_str("<expired>")
                }
            }
        }

        let mut builder = f.debug_struct("ResourceFuture");
        match &self.state {
            ResourceFutureState::Immediate(Ok(_)) => builder.field("Immediate", &ImmediateOk()),
            ResourceFutureState::Immediate(Err(e)) => builder.field("Immediate", &ImmediateErr(e)),
            ResourceFutureState::Delayed { cache, .. } => builder.field("Delayed", &Delayed(cache)),
            ResourceFutureState::Awaiting { .. } => builder.field("Awaiting", &"<awaiting>"),
            ResourceFutureState::Uninit => builder.field("Uninit", &"<uninit>"),
        };
        builder.finish()
    }
}

impl<T: ?Sized + Send + Sync + 'static> ResourceFuture<T> {
    #[inline]
    fn from_result(result: ResourceLoadResult<T>) -> Self {
        Self { state: ResourceFutureState::Immediate(result) }
    }

    fn from_cache_entry(
        entry: Weak<CacheEntry>,
        manager: Arc<ResourceManager>,
        id: ResourceId,
        loader: Arc<dyn ResourceLoader<T>>,
        task_priority: TaskPriority,
        parents: Vec<ResourceId>,
    ) -> Self {
        Self {
            state: ResourceFutureState::Delayed {
                cache: entry,
                load_fn: Box::new(move || manager.get_or_load_impl(id, loader, task_priority, parents)),
            }
        }
    }

    fn with_state<R>(&mut self, func: impl FnOnce(ResourceFutureState<T>) -> (ResourceFutureState<T>, R)) -> R {
        let mut state = ResourceFutureState::Uninit;
        std::mem::swap(&mut self.state, &mut state);
        let (mut state, result) = func(state);
        std::mem::swap(&mut self.state, &mut state);
        result
    }

    /// Synchronously check whether this handle's [Resource][res] is done loading.
    ///
    /// If this handle's load task is complete, this yields the result of that load task. If the
    /// load task is not yet complete, yield the handle back.
    ///
    /// [res]: Resource
    pub fn check(self) -> Result<ResourceLoadResult<T>, Self> {
        match self.state {
            ResourceFutureState::Immediate(inner) => Ok(inner),
            ResourceFutureState::Delayed{ cache, load_fn } => {
                if let Some(strong_cache) = cache.upgrade() {
                    match strong_cache.poll() {
                        Some(CacheEntryGetResult::Ok(r)) => Ok(Ok(r)),
                        Some(CacheEntryGetResult::Err(e)) => Ok(Err(e)),
                        Some(CacheEntryGetResult::Expired) => load_fn().check(),
                        None => Err(Self { state: ResourceFutureState::Delayed { cache, load_fn }}),
                    }
                } else {
                    // CacheEntry expired, reload
                    load_fn().check()
                }
            },
            ResourceFutureState::Awaiting { .. }
                => panic!("ResourceFuture checked while being awaited"),
            ResourceFutureState::Uninit
                => panic!("Found uninit state in inner ResourceFuture"),
        }
    }

    /// Synchronously wait for this handle's [Resource][res] to be done loading.
    ///
    /// Blocks on awaiting this future.
    #[inline]
    pub fn wait(self) -> ResourceLoadResult<T> {
        future::block_on(self)
    }
}

impl<T: ?Sized + Send + Sync + 'static> Future for ResourceFuture<T> {
    type Output = ResourceLoadResult<T>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.with_state(|state| state.poll(cx))
    }
}

/// Centralized management of resource loading and caching.
///
/// See [module-level documentation][mod] for more.
///
/// ResourceManagers are comprised of multiple [sources][src], and when loading new [Resources][res],
/// they will be searched through them in the order those sources were given to the ResourceManager
/// on construction.
///
/// ResourceManagers also contain an internal async executor, which runs its own thread/s. This
/// async executor will be spun up when the ResourceManager is created, and stopped when the
/// ResourceManager is dropped. Each ResourceManager would contain its own executor, so for that
/// and for caching reasons, it is recommended to have one global ResourceManager, usually owned
/// by the [Engine][eng] itself.
///
/// # Loading
///
/// When retrieving a [Resource] by [ID](ResourceId) and no existing value is already cached for
/// that ID, the given [ResourceLoader] will be used to load and cache a new [Resource]. Loading is
/// handled asynchronously, meaning an async task is enqueued to load the Resource via the internal
/// async executor. In the meantime, a [handle](ResourceFuture) will be returned synchronously from
/// the relevant `get_*()` method. This handle can be used to synchronously poll or asynchronously
/// await the load result.
///
/// If a [Resource] has already been loaded and cached, it will instead be returned wrapped in a
/// [handle](ResourceFuture). The given [ResourceLoader] will be attached to the handle, so that if
/// the cached Resource is expired and evicted before the handle is resolved, it will trigger a new
/// load using the given loader.
///
/// See also:
///  * [`get()`](ResourceManager::get)
///  * [`get_with_priority()`](ResourceManager::get_with_priority)
///  * [`get_with_loader()`](ResourceManager::get_with_loader)
///  * [`get_with_loader_priority()`](ResourceManager::get_with_loader_priority)
///
/// # Examples
/// ```
/// use gtether::resource::manager::ResourceManager;
/// # use gtether::resource::source::ResourceSource;
///
/// # fn wrapper(source1: Box<dyn ResourceSource>, source2: Box<dyn ResourceSource>) {
/// let manager = ResourceManager::builder()
///     .source(source1)
///     .source(source2)
///     .build();
/// # }
/// ```
///
/// [res]: Resource
/// [src]: ResourceSource
/// [eng]: crate::Engine
/// [mod]: super::manager
pub struct ResourceManager {
    executor: Arc<ManagerExecutor>,
    sources: Vec<Box<dyn ResourceSource>>,
    cache: Mutex<HashMap<ResourceId, Arc<CacheEntry>>>,
    dependencies: Arc<RwLock<DependencyGraph>>,
    weak: Weak<Self>,
    #[cfg(test)]
    test_ctx: tests::ResourceManagerTestContext,
}

impl ResourceManager {
    fn new(sources: Vec<Box<dyn ResourceSource>>) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<ResourceManager>| {
            Self {
                executor: Arc::new(ManagerExecutor::new()),
                sources,
                cache: Mutex::new(HashMap::default()),
                dependencies: Arc::new(RwLock::new(DependencyGraph::default())),
                weak: weak.clone(),
                #[cfg(test)]
                test_ctx: tests::ResourceManagerTestContext::default(),
            }
        })
    }

    /// Create a [ResourceManagerBuilder].
    ///
    /// This is the preferred way to create a ResourceManager.
    #[inline]
    pub fn builder() -> ResourceManagerBuilder {
        ResourceManagerBuilder::default()
    }

    /// Get access to the [TestContext][tc].
    ///
    /// This is available when building with `cfg(test)` for unit tests, and can be used to validate
    /// behaviour in regards to either the ResourceManager itself or user-defined [sources][src].
    ///
    /// [tc]: tests::ResourceManagerTestContext
    /// [src]: ResourceSource
    #[cfg(test)]
    #[inline]
    pub fn test_ctx(&self) -> &tests::ResourceManagerTestContext { &self.test_ctx }

    fn watch_n(&self, id: &ResourceId, source_idx: &SourceIndex) {
        for (idx, source) in self.sources.iter().enumerate() {
            if idx <= source_idx.idx() {
                source.watch(id.clone(), Box::new(ManagerResourceWatcher {
                    manager: self.weak.clone(),
                    idx: SourceIndex::new(idx),
                }), source_idx.sub_idx().map(|inner| inner.clone()));
            } else {
                source.unwatch(id);
            }
        }
    }

    fn remove(&self, cache: &mut MutexGuard<HashMap<ResourceId, Arc<CacheEntry>>>, id: &ResourceId) {
        cache.remove(id);
        for source in &self.sources {
            source.unwatch(id);
        }
    }

    fn update(&self, id: ResourceId, source_idx: SourceIndex, update: ResourceUpdate) {
        let task_self = self.weak.upgrade()
            .expect("ResourceManager cyclic reference should not be broken");
        let task_id = id.clone();
        let task_source_idx = source_idx.clone();
        self.executor.spawn(TaskPriority::Update, async move {
            #[cfg(test)]
            let _test_ctx_lock = task_self.test_ctx.sync_update.run().await;

            let before_entry = match task_self.cache.lock().get(&task_id) {
                Some(entry) => entry.clone(),
                None => {
                    debug!(id = %task_id, "Tried to update non-existent resource");
                    return;
                }
            };

            let (hash, task_source_idx, ignore_priority) = match update {
                ResourceUpdate::Added(hash) | ResourceUpdate::Modified(hash)
                => (hash, task_source_idx, false),
                ResourceUpdate::Removed => {
                    match task_self.find_hash(&task_id) {
                        Some(s) => (s.hash, s.idx, true),
                        None => {
                            warn!(id = %task_id, "Resource source removed and no matching backup sources found");
                            return;
                        }
                    }
                },
            };

            if Some(&hash) == before_entry.hash().as_ref() {
                debug!(id = %task_id, hash, "Ignoring update as hashes match");
                return;
            }

            let source = match task_self.sources.get(task_source_idx.idx()) {
                Some(source) => source,
                None => {
                    warn!(id = %task_id, source_idx = %task_source_idx, "Tried to update from non-existent source");
                    return;
                }
            };

            let fut = {
                let mut cache = task_self.cache.lock();
                let fut_result = before_entry.update(
                    &task_self,
                    source,
                    &task_source_idx,
                    ignore_priority,
                );
                match fut_result {
                    CacheEntryUpdateFutResult::Skip => return,
                    CacheEntryUpdateFutResult::Evict => {
                        task_self.remove(&mut cache, &task_id);
                        return;
                    },
                    CacheEntryUpdateFutResult::Run(fut) => fut,
                }
            };
            let update_result = fut.await;

            let mut cache = task_self.cache.lock();
            let after_entry = match cache.get(&task_id) {
                Some(entry) => entry,
                None => {
                    debug!(id = %task_id, "Cache entry evicted before resource update completed");
                    return;
                }
            };

            if Arc::ptr_eq(after_entry, &before_entry) {
                match update_result {
                    CacheEntryUpdateResult::Ok(data_source) => {
                        task_self.watch_n(&task_id, &task_source_idx);
                        after_entry.finalize_update(data_source);
                    },
                    CacheEntryUpdateResult::Err(e)
                        => warn!(id = %task_id, source_idx = %task_source_idx, %e, "Failed to update resource from source"),
                    CacheEntryUpdateResult::Expired => {
                        debug!(id = %task_id, "Cache entry expired before resource update completed");
                        task_self.remove(&mut cache, &task_id);
                    },
                }
            } else {
                debug!(id = %task_id, "Cache entry changed before resource update completed");
            }
        }.instrument(info_span!("resource_update", %id, %source_idx))).detach();
    }

    fn find_hash(&self, id: &ResourceId) -> Option<SealedResourceDataSource> {
        for (idx, source) in self.sources.iter().enumerate() {
            match source.hash(id) {
                Some(hash) => return Some(hash.seal(idx)),
                None => {},
            }
        }
        None
    }

    async fn find_data(&self, id: &ResourceId) -> Option<SealedResourceDataResult> {
        for (idx, source) in self.sources.iter().enumerate() {
            match source.load(id).await {
                Ok(data) => return Some(Ok(data.seal(idx))),
                Err(ResourceLoadError::NotFound(_)) => {},
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }

    /// Spawn a new async task for the ResourceManager to execute in the background.
    ///
    /// The spawned task shares the same executor as all [Resource] load and update tasks. This
    /// spawn() method is intended to be used _only_ for Resource loading adjacent tasks, such as
    /// spawning a delayed load of some of a Resource's components. This is _not_ a general purpose
    /// executor, and should not be used as such.
    #[inline]
    pub fn spawn<T: Send + 'static>(
        &self,
        priority: LoadPriority,
        future: impl Future<Output = T> + Send + 'static,
    ) -> ManagerTask<T> {
        self.executor.spawn(priority.into(), future)
    }

    fn load<T>(
        &self,
        cache: &mut MutexGuard<HashMap<ResourceId, Arc<CacheEntry>>>,
        id: ResourceId,
        loader: Arc<dyn ResourceLoader<T>>,
        task_priority: TaskPriority,
        parents: Vec<ResourceId>,
    ) -> ResourceFuture<T>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let strong_self = self.weak.upgrade()
            .expect("ResourceManager cyclic reference should not be broken");
        let task_self = strong_self.clone();
        let task_id = id.clone();
        let task_loader = loader.clone();
        let task_parents = parents.clone();

        let span = if parents.is_empty() {
            info_span!("resource_load", id = %id.clone(), priority = %task_priority)
        } else {
            info_span!("sub_load", id = %id.clone())
        };

        let task = self.executor.spawn(task_priority, async move {
            #[cfg(test)]
            let _test_ctx_lock = task_self.test_ctx.sync_load.run().await;
            let (result, source_idx) = match task_self.find_data(&task_id).await {
                Some(Ok(data)) => {
                    let ctx = ResourceLoadContext::new(
                        task_self.clone(),
                        task_id.clone(),
                        task_priority,
                        task_parents,
                    );
                    match task_loader.load(data.data, &ctx).await {
                        Ok(v) => {
                            let sub_dependencies = ctx.dependencies.into_inner();
                            sub_dependencies.apply(&mut task_self.dependencies.write());
                            (
                                Ok((Arc::new(Resource::new(task_id.clone(), v)), data.source.hash)),
                                data.source.idx,
                            )
                        },
                        Err(e) => (Err(e), data.source.idx),
                    }
                },
                Some(Err(e)) => (Err(e), SourceIndex::max()),
                None => (Err(ResourceLoadError::NotFound(task_id.clone())), SourceIndex::max()),
            };

            ResourceTaskData {
                id: task_id,
                result,
                source_idx,
                loader: task_loader,
            }
        }.instrument(span));
        let entry = Arc::new(CacheEntry::from_task(id.clone(), task, strong_self.clone()));
        let weak_entry = Arc::downgrade(&entry);
        cache.insert(id.clone(), entry);
        ResourceFuture::from_cache_entry(weak_entry, strong_self, id, loader, task_priority, parents)
    }

    fn get_or_load_impl<T>(
        &self,
        id: ResourceId,
        loader: Arc<dyn ResourceLoader<T>>,
        load_priority: TaskPriority,
        parents: Vec<ResourceId>,
    ) -> ResourceFuture<T>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let mut cache = self.cache.lock();
        if let Some(entry) = cache.get(&id) {
            match entry.poll() {
                Some(CacheEntryGetResult::Ok(resource))
                    => ResourceFuture::from_result(Ok(resource)),
                Some(CacheEntryGetResult::Err(e))
                    => ResourceFuture::from_result(Err(e)),
                // Entry is expired, reload
                Some(CacheEntryGetResult::Expired) => self.load(&mut cache, id, loader, load_priority, parents),
                // Entry is still loading, return a delayed result
                None => ResourceFuture::from_cache_entry(
                    Arc::downgrade(entry),
                    self.weak.upgrade()
                        .expect("ResourceManager cyclic reference should not be broken"),
                    id,
                    loader,
                    load_priority,
                    parents,
                )
            }
        } else {
            // No existing entry, load a new one
            self.load(&mut cache, id, loader, load_priority, parents)
        }
    }

    /// Get/load a [Resource] with a default [ResourceLoader] and [LoadPriority].
    ///
    /// Creates and uses the [ResourceLoader] specified by [ResourceDefaultLoader].
    ///
    /// Uses [LoadPriority::Immediate].
    ///
    /// See also: [Self#loading]
    #[inline]
    pub fn get<T>(&self, id: impl Into<ResourceId>) -> ResourceFuture<T>
    where
        T: ResourceDefaultLoader,
    {
        self.get_with_priority(id, LoadPriority::Immediate)
    }

    /// Get/load a [Resource] with a default [ResourceLoader].
    ///
    /// Creates and uses the [ResourceLoader] specified by [ResourceDefaultLoader].
    ///
    /// See also: [Self#loading]
    #[inline]
    pub fn get_with_priority<T>(
        &self,
        id: impl Into<ResourceId>,
        load_priority: LoadPriority,
    ) -> ResourceFuture<T>
    where
        T: ResourceDefaultLoader,
    {
        self.get_with_loader_priority(id, T::default_loader(), load_priority)
    }

    /// Get/load a [Resource] with a default [LoadPriority].
    ///
    /// Uses [LoadPriority::Immediate].
    ///
    /// See also: [Self#loading]
    #[inline]
    pub fn get_with_loader<T>(
        &self,
        id: impl Into<ResourceId>,
        loader: impl ResourceLoader<T>,
    ) -> ResourceFuture<T>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.get_with_loader_priority(id, loader, LoadPriority::Immediate)
    }

    /// Get/load a [Resource].
    ///
    /// See also: [Self#loading]
    pub fn get_with_loader_priority<T>(
        &self,
        id: impl Into<ResourceId>,
        loader: impl ResourceLoader<T>,
        load_priority: LoadPriority,
    ) -> ResourceFuture<T>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.get_or_load_impl(id.into(), Arc::new(loader), load_priority.into(), vec![])
    }
}

/// Builder pattern for [ResourceManager].
///
/// Recommended way to create is via [ResourceManager::builder()].
#[derive(Default)]
pub struct ResourceManagerBuilder {
    sources: Vec<Box<dyn ResourceSource>>,
}

impl ResourceManagerBuilder {
    /// Add a [source][src].
    ///
    /// [Sources][src] added will be prioritized by the built [ResourceManager] in chronological
    /// order; [sources][src] added first will have higher priority for retrieving resource data
    /// from than [sources][src] added later.
    ///
    /// [src]: ResourceSource
    #[inline]
    pub fn source(mut self, source: impl Into<Box<dyn ResourceSource>>) -> Self {
        self.sources.push(source.into());
        self
    }

    #[inline]
    pub fn build(self) -> Arc<ResourceManager> {
        ResourceManager::new(self.sources)
    }
}

/// Contextual data for loading [Resources](Resource).
///
/// This is created by the [ResourceManager], and passed to resource
/// [`load()`](ResourceLoader::load)/[`update()`](ResourceLoader::update) methods. It provides
/// layered access to the [ResourceManager], and allows loading additional resources as dependencies
/// of the currently loading resource.
pub struct ResourceLoadContext {
    manager: Arc<ResourceManager>,
    id: ResourceId,
    priority: TaskPriority,
    dependencies: RwLock<DependencyGraphLayer>,
}

impl ResourceLoadContext {
    #[inline]
    pub(in crate::resource) fn new(
        manager: Arc<ResourceManager>,
        id: ResourceId,
        priority: TaskPriority,
        parents: impl IntoIterator<Item=ResourceId>,
    ) -> Self {
        let dependencies = DependencyGraphLayer::new(id.clone(), parents);
        Self {
            manager,
            id,
            priority,
            dependencies: RwLock::new(dependencies),
        }
    }

    /// The resource [ID](ResourceId) that is currently being loaded.
    #[inline]
    pub fn id(&self) -> ResourceId {
        self.id.clone()
    }

    /// Get/load a [Resource] with a default [ResourceLoader].
    ///
    /// Creates and uses the [ResourceLoader] specified by [ResourceDefaultLoader].
    ///
    /// Uses the same [LoadPriority] as the currently loading resource.
    ///
    /// See also: [ResourceManager#loading]
    pub fn get<T>(
        &self,
        id: impl Into<ResourceId>,
    ) -> ResourceFuture<T>
    where
        T: ResourceDefaultLoader,
    {
        self.get_with_loader(id, T::default_loader())
    }

    /// Get/load a [Resource].
    ///
    /// Uses the same [LoadPriority] as the currently loading resource.
    ///
    /// See also: [ResourceManager#loading]
    pub fn get_with_loader<T>(
        &self,
        id: impl Into<ResourceId>,
        loader: impl ResourceLoader<T>,
    ) -> ResourceFuture<T>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let id = id.into();
        let parents: Vec<_> = {
            let mut deps = self.dependencies.write();
            deps.insert(id.clone());
            deps.parents()
                .chain([deps.id()])
                .cloned()
                .collect()
        };
        if parents.contains(&id) {
            ResourceFuture::from_result(Err(ResourceLoadError::CyclicLoad {
                parents,
                id,
            }))
        } else {
            self.manager.get_or_load_impl(id, Arc::new(loader), self.priority, parents)
        }
    }
}

#[cfg(test)]
pub(in crate::resource) mod tests {
    use super::*;
    use std::marker::PhantomData;

    use async_trait::async_trait;
    use parking_lot::RwLock;
    use smol::Timer;
    use std::time::Duration;

    use crate::resource::source::{ResourceData, ResourceDataResult, ResourceDataSource};
    use crate::resource::ResourceReadData;

    pub async fn timeout<T>(fut: impl Future<Output = T>, time: Duration) -> T {
        let timeout_fn = async move {
            Timer::after(time).await;
            panic!("Timeout reached: {time:?}");
        };
        fut.or(timeout_fn).await
    }

    pub struct SyncContextReadGuard<'a>{
        #[allow(unused)]
        inner: smol::lock::RwLockReadGuard<'a, ()>,
        count: &'a Mutex<usize>,
    }

    impl Drop for SyncContextReadGuard<'_> {
        fn drop(&mut self) {
            *self.count.lock() += 1;
        }
    }

    pub struct SyncContextWriteGuard<'a>(
        #[allow(unused)]
        smol::lock::RwLockWriteGuard<'a, ()>
    );

    pub struct SyncContext {
        inner: smol::lock::RwLock<()>,
        count: Mutex<usize>,
    }

    impl Default for SyncContext {
        fn default() -> Self {
            Self {
                inner: smol::lock::RwLock::new(()),
                count: Mutex::new(0),
            }
        }
    }

    impl SyncContext {
        pub async fn run(&self) -> SyncContextReadGuard<'_> {
            SyncContextReadGuard {
                inner: self.inner.read().await,
                count: &self.count,
            }
        }

        pub async fn block(&self, time: Option<Duration>) -> SyncContextWriteGuard<'_> {
            let guard = match time {
                Some(time) => timeout(self.inner.write(), time).await,
                None => self.inner.write().await
            };
            SyncContextWriteGuard(guard)
        }

        #[inline]
        pub fn assert_count(&self, count: usize) {
            assert_eq!(*self.count.lock(), count);
        }

        pub async fn wait_count(&self, count: usize) {
            while *self.count.lock() < count {
                Timer::after(Duration::from_millis(50)).await;
            }
        }

        #[inline]
        pub fn clear(&self) {
            *self.count.lock() = 0;
        }
    }

    #[derive(Default)]
    pub struct ResourceManagerTestContext {
        pub sync_load: SyncContext,
        pub sync_update: SyncContext,
    }

    impl ResourceManagerTestContext {
        pub fn clear(&self) {
            self.sync_load.clear();
            self.sync_update.clear();
        }
    }

    #[derive(Default, Clone)]
    pub struct StringLoader(());

    #[async_trait]
    impl ResourceLoader<String> for StringLoader {
        async fn load(
            &self,
            mut data: ResourceReadData,
            _ctx: &ResourceLoadContext,
        ) -> Result<Box<String>, ResourceLoadError> {
            let mut output = String::new();
            data.read_to_string(&mut output).await?;
            Ok(Box::new(output))
        }
    }

    #[derive(Clone)]
    pub struct TestResourceLoader {
        expected_id: ResourceId,
    }

    impl TestResourceLoader {
        #[inline]
        pub fn new(expected_id: impl Into<ResourceId>) -> Self {
            Self {
                expected_id: expected_id.into(),
            }
        }
    }

    #[async_trait]
    impl ResourceLoader<String> for TestResourceLoader {
        async fn load(
            &self,
            data: ResourceReadData,
            ctx: &ResourceLoadContext,
        ) -> Result<Box<String>, ResourceLoadError> {
            assert_eq!(ctx.id(), self.expected_id);
            StringLoader::default().load(data, ctx).await
        }
    }

    pub struct TestResourceRef<T: Send + Sync + 'static> {
        pub resources: Vec<Arc<Resource<T>>>,
    }

    #[derive(Clone)]
    pub struct TestResourceRefLoader<T, L>
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T>,
    {
        loader: L,
        _phantom: PhantomData<T>,
    }

    impl<T, L> TestResourceRefLoader<T, L>
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T>,
    {
        #[inline]
        pub fn new(loader: L) -> Self {
            Self {
                loader,
                _phantom: PhantomData::default(),
            }
        }
    }

    #[async_trait]
    impl<T, L> ResourceLoader<TestResourceRef<T>> for TestResourceRefLoader<T, L>
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T> + Clone,
    {
        async fn load(
            &self,
            mut data: ResourceReadData,
            ctx: &ResourceLoadContext,
        ) -> Result<Box<TestResourceRef<T>>, ResourceLoadError> {
            let mut ids = String::new();
            data.read_to_string(&mut ids).await?;

            let futs = ids.split(':')
                .map(|id| ctx.get_with_loader(id, self.loader.clone()))
                .collect::<Vec<_>>();
            let mut resources = Vec::with_capacity(futs.len());
            for fut in futs {
                resources.push(fut.await?);
            }

            Ok(Box::new(TestResourceRef {
                resources,
            }))
        }
    }

    #[derive(Default)]
    pub struct ResourceDataMap {
        raw: RwLock<HashMap<ResourceId, (&'static [u8], String)>>,
        watch_list: RwLock<HashMap<ResourceId, Box<dyn ResourceWatcher>>>,
    }

    impl ResourceDataMap {
        fn get(&self, id: &ResourceId) -> Option<(&'static [u8], String)> {
            self.raw.read().get(id).map(|(r, h)| (*r, h.clone()))
        }

        fn insert_impl(&self, id: ResourceId, data: &'static [u8], hash: String) {
            let update = match self.raw.write().insert(id.clone(), (data, hash.clone())).is_some() {
                true => ResourceUpdate::Modified(hash),
                false => ResourceUpdate::Added(hash),
            };
            if let Some(watcher) = self.watch_list.read().get(&id) {
                watcher.notify_update(&id, update);
            }
        }

        pub fn insert(&self, id: impl Into<ResourceId>, data: &'static [u8], hash: impl Into<String>) {
            self.insert_impl(id.into(), data, hash.into())
        }

        pub fn remove(&self, id: impl Into<ResourceId>) {
            let id = id.into();
            if self.raw.write().remove(&id).is_some() {
                if let Some(watcher) = self.watch_list.read().get(&id) {
                    watcher.notify_update(&id, ResourceUpdate::Removed);
                }
            }
        }

        fn watch(&self, id: ResourceId, watcher: Box<dyn ResourceWatcher>) {
            self.watch_list.write().insert(id, watcher);
        }

        fn unwatch(&self, id: &ResourceId) {
            self.watch_list.write().remove(id);
        }

        pub fn assert_watch(&self, id: impl Into<ResourceId>, should_watch: bool) {
            let id = id.into();
            let watch_list = self.watch_list.read();
            match (watch_list.get(&id), should_watch) {
                (Some(_), false) => panic!("Watcher found for {id} when there should be none"),
                (None, true) => panic!("No watcher found for {id}"),
                (Some(_), true) => {},
                (None, false) => {},
            }
        }
    }

    #[derive(Default)]
    pub struct TestResourceSource {
        inner: Arc<ResourceDataMap>,
    }

    impl TestResourceSource {
        #[inline]
        pub fn data_map(&self) -> &Arc<ResourceDataMap> { &self.inner }
    }

    #[async_trait]
    impl ResourceSource for TestResourceSource {
        fn hash(&self, id: &ResourceId) -> Option<ResourceDataSource> {
            if let Some((_, hash)) = self.inner.get(id) {
                Some(ResourceDataSource::new(hash))
            } else {
                None
            }
        }

        async fn load(&self, id: &ResourceId) -> ResourceDataResult {
            if let Some((data, hash)) = self.inner.get(id) {
                Ok(ResourceData::new(Box::new(data), hash))
            } else {
                Err(ResourceLoadError::NotFound(id.clone()))
            }
        }

        fn watch(&self, id: ResourceId, watcher: Box<dyn ResourceWatcher>, _sub_idx: Option<SourceIndex>) {
            self.inner.watch(id, watcher);
        }

        fn unwatch(&self, id: &ResourceId) {
            self.inner.unwatch(id);
        }
    }

    pub fn create_resource_manager<const N: usize>() -> (Arc<ResourceManager>, [Arc<ResourceDataMap>; N]) {
        let sources = core::array::from_fn::<_, N, _>(|_| TestResourceSource::default());
        let data_maps = core::array::from_fn(|i| sources[i].data_map().clone());
        let mut builder = ResourceManager::builder();
        for source in sources {
            builder = builder.source(source);
        }
        let manager = builder.build();
        (manager, data_maps)
    }

    #[test]
    fn test_resource_manager_not_found() {
        let (manager, _) = create_resource_manager::<1>();

        let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
        let err = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect_err("No resource data should be present for 'key'");
        assert_matches!(err, ResourceLoadError::NotFound(id) => {
            assert_eq!(id, ResourceId::from("key"));
        });
        manager.test_ctx().sync_load.assert_count(1);

        // Result should be cached, and not trigger another load
        manager.test_ctx().clear();
        let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
        let err = fut.check()
            .expect("Error should be cached and pollable")
            .expect_err("No resource data should be present for 'key'");
        assert_matches!(err, ResourceLoadError::NotFound(id) => {
            assert_eq!(id, ResourceId::from("key"));
        });
        manager.test_ctx().sync_load.assert_count(0);
    }

    #[test]
    fn test_resource_manager_load_error() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("invalid", b"\xC0", "h_invalid");

        let fut = {
            let _lock = future::block_on(manager.test_ctx().sync_load.block(Some(Duration::from_secs(1))));
            let fut = manager.get_with_loader("invalid", TestResourceLoader::new("invalid"));
            fut.check().expect_err("Resource should not be loaded yet")
        };

        let error = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect_err("Resource should fail to load for 'invalid'");
        assert_matches!(error, ResourceLoadError::ReadError(_));
        manager.test_ctx().sync_load.assert_count(1);

        // Result should be cached, and not trigger another load
        manager.test_ctx().clear();
        let fut = manager.get_with_loader("invalid", TestResourceLoader::new("invalid"));
        let error = fut.check()
            .expect("Error should be cached and pollable")
            .expect_err("Resource should still fail to load for 'invalid'");
        assert_matches!(error, ResourceLoadError::ReadError(_));
        manager.test_ctx().sync_load.assert_count(0);
    }

    #[test]
    fn test_resource_manager_found() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"value", "h_value");

        let fut = {
            let _lock = future::block_on(manager.test_ctx().sync_load.block(Some(Duration::from_secs(1))));
            let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
            fut.check().expect_err("Resource should not be loaded yet")
        };

        let value = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());
        manager.test_ctx().sync_load.assert_count(1);

        // Result should be cached, and not trigger another load
        manager.test_ctx().clear();
        let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
        let value = fut.check()
            .expect("Resource should be cached and pollable")
            .expect("Resource should still be loaded for 'key'");
        assert_eq!(*value.read(), "value".to_owned());
        manager.test_ctx().sync_load.assert_count(0);
    }

    #[test]
    fn test_resource_manager_drop_first_should_reload_second() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"value", "h_value");

        data_maps[0].assert_watch("key", false);
        let (fut1, fut2) = {
            let _lock = future::block_on(manager.test_ctx().sync_load.block(Some(Duration::from_secs(1))));
            let fut1 = manager.get_with_loader("key", TestResourceLoader::new("key"));
            let fut2 = manager.get_with_loader("key", TestResourceLoader::new("key"));
            (fut1, fut2)
        };

        let value = future::block_on(timeout(fut1, Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());
        data_maps[0].assert_watch("key", true);
        manager.test_ctx().sync_load.assert_count(1);

        drop(value);

        let value = future::block_on(timeout(fut2, Duration::from_secs(1)))
            .expect("Resource should reload for 'key' after first handle was dropped");
        assert_eq!(*value.read(), "value".to_owned());
        manager.test_ctx().sync_load.assert_count(2);
    }

    #[test]
    fn test_resource_manager_update_value_to_value() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"value", "h_value");

        data_maps[0].assert_watch("key", false);
        let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
        let value = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        data_maps[0].assert_watch("key", true);
        manager.test_ctx().sync_update.assert_count(0);

        {
            let _lock = manager.test_ctx().sync_update.block(Some(Duration::from_secs(1)));
            data_maps[0].insert("key", b"new_value", "h_new_value");
            assert_eq!(*value.read(), "value".to_owned());
        }

        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        assert_eq!(*value.read(), "new_value".to_owned());
        data_maps[0].assert_watch("key", true);
    }

    #[test]
    fn test_resource_manager_update_value_to_error() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"value", "h_value");

        data_maps[0].assert_watch("key", false);
        let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
        let value = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        data_maps[0].assert_watch("key", true);
        manager.test_ctx().sync_update.assert_count(0);

        {
            let _lock = future::block_on(manager.test_ctx().sync_update.block(Some(Duration::from_secs(1))));
            data_maps[0].insert("key", b"\xC0", "h_invalid");
            assert_eq!(*value.read(), "value".to_owned());
        }

        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        assert_eq!(*value.read(), "value".to_owned());
        data_maps[0].assert_watch("key", true);
    }

    #[test]
    fn test_resource_manager_update_error_to_value() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"\xC0", "h_invalid");

        let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
        let error = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect_err("Resource should fail to load for 'key'");
        assert_matches!(error, ResourceLoadError::ReadError(_));
        data_maps[0].assert_watch("key", true);
        manager.test_ctx().sync_update.assert_count(0);

        {
            let _lock = future::block_on(manager.test_ctx().sync_update.block(Some(Duration::from_secs(1))));
            data_maps[0].insert("key", b"new_value", "h_new_value");
        }

        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
        let value = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "new_value".to_owned());
        data_maps[0].assert_watch("key", true);
    }

    #[test]
    fn test_resource_manager_source_priorities() {
        let (manager, data_maps) = create_resource_manager::<3>();
        data_maps[1].insert("key", b"value_1", "h_value_1");
        data_maps[2].insert("key", b"value_2", "h_value_2");

        // Load should retrieve "value_1", as it's earlier in the priority chain
        let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
        let value = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value_1".to_owned());
        manager.test_ctx().sync_update.assert_count(0);
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", true);
        data_maps[2].assert_watch("key", false);

        // Updating lower priority shouldn't change the value
        data_maps[2].insert("key", b"new_value_2", "h_new_value_2");
        // This shouldn't even trigger a watch, so there is no mechanism to wait on, unfortunately
        assert_eq!(*value.read(), "value_1".to_owned());
        manager.test_ctx().sync_update.assert_count(0);
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", true);
        data_maps[2].assert_watch("key", false);

        // Updating same priority *should* change the value
        data_maps[1].insert("key", b"new_value_1", "h_new_value_1");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        assert_eq!(*value.read(), "new_value_1");
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", true);
        data_maps[2].assert_watch("key", false);

        // Updating higher priority *should* change the value - and also what watchers are active
        data_maps[0].insert("key", b"new_value_0", "h_new_value_0");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(2), Duration::from_secs(1)));
        assert_eq!(*value.read(), "new_value_0");
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", false);
        data_maps[2].assert_watch("key", false);

        // Updating previously watched source shouldn't change the value
        data_maps[1].insert("key", b"new_new_value_1", "h_new_new_value_1");
        // This shouldn't even trigger a watch, so there is no mechanism to wait on, unfortunately
        assert_eq!(*value.read(), "new_value_0");
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", false);
        data_maps[2].assert_watch("key", false);

        // Removing higher priority should fall back to new lower priority value
        data_maps[0].remove("key");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(3), Duration::from_secs(1)));
        assert_eq!(*value.read(), "new_new_value_1");
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", true);
        data_maps[2].assert_watch("key", false);
    }

    #[test]
    fn test_resource_manager_hashing() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"value", "h_value");

        let fut = manager.get_with_loader("key", TestResourceLoader::new("key"));
        let value = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());

        // Same hash shouldn't cause an update
        data_maps[0].insert("key", b"new_value", "h_value");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        assert_eq!(*value.read(), "value".to_owned());

        // Different hash *does* cause an update
        data_maps[0].insert("key", b"new_value", "h_new_value");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(2), Duration::from_secs(1)));
        assert_eq!(*value.read(), "new_value".to_owned());
    }

    #[test]
    fn test_resource_manager_dependencies_linear() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("a", b"b", "h_a");
        data_maps[0].insert("b", b"c", "h_b");
        data_maps[0].insert("c", b"value", "h_c");

        {
            let fut = manager.get_with_loader(
                "b",
                TestResourceRefLoader::new(TestResourceLoader::new("c")),
            );
            let value = future::block_on(timeout(fut, Duration::from_secs(1)))
                .expect("Resource should load for 'b'");
            assert_eq!(*value.read().resources[0].read(), "value".to_owned());

            let mut expected_graph = DependencyGraph::default();
            expected_graph.add_dependency(ResourceId::from("b"), ResourceId::from("c"));
            assert_eq!(&*manager.dependencies.read(), &expected_graph);
        }

        {
            let fut = manager.get_with_loader(
                "a",
                TestResourceRefLoader::new(TestResourceRefLoader::new(TestResourceLoader::new("c"))),
            );
            let value = future::block_on(timeout(fut, Duration::from_secs(1)))
                .expect("Resource should load for 'a'");
            assert_eq!(*value.read().resources[0].read().resources[0].read(), "value".to_owned());

            let mut expected_graph = DependencyGraph::default();
            expected_graph.add_dependency(ResourceId::from("a"), ResourceId::from("b"));
            expected_graph.add_dependency(ResourceId::from("b"), ResourceId::from("c"));
            assert_eq!(&*manager.dependencies.read(), &expected_graph);
        }
    }

    #[test]
    fn test_resource_manager_dependencies_tree() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("a1", b"b1:b2", "h_a1");
        data_maps[0].insert("a2", b"b1:b2", "h_a2");
        data_maps[0].insert("b1", b"c1:c2", "h_b1");
        data_maps[0].insert("b2", b"c2:c3", "h_b2");
        data_maps[0].insert("c1", b"value1", "h_c1");
        data_maps[0].insert("c2", b"value2", "h_c2");
        data_maps[0].insert("c3", b"value3", "h_c3");

        let fut_a1 = manager.get_with_loader(
            "a1",
            TestResourceRefLoader::new(TestResourceRefLoader::new(StringLoader::default())),
        );
        let fut_a2 = manager.get_with_loader(
            "a2",
            TestResourceRefLoader::new(TestResourceRefLoader::new(StringLoader::default())),
        );
        let a1 = future::block_on(timeout(fut_a1, Duration::from_secs(1)))
            .expect("Resource should load for 'a1'");
        let a2 = future::block_on(timeout(fut_a2, Duration::from_secs(1)))
            .expect("Resource should load for 'a2'");
        {
            let a1 = a1.read();
            {
                let b1 = a1.resources[0].read();
                let values = b1.resources.iter()
                    .map(|r| r.read().clone())
                    .collect::<Vec<_>>();
                assert_eq!(values, vec!["value1".to_string(), "value2".to_string()]);
            }
            {
                let b2 = a1.resources[1].read();
                let values = b2.resources.iter()
                    .map(|r| r.read().clone())
                    .collect::<Vec<_>>();
                assert_eq!(values, vec!["value2".to_string(), "value3".to_string()]);
            }
        }
        {
            let a2 = a2.read();
            {
                let b1 = a2.resources[0].read();
                let values = b1.resources.iter()
                    .map(|r| r.read().clone())
                    .collect::<Vec<_>>();
                assert_eq!(values, vec!["value1".to_string(), "value2".to_string()]);
            }
            {
                let b2 = a2.resources[1].read();
                let values = b2.resources.iter()
                    .map(|r| r.read().clone())
                    .collect::<Vec<_>>();
                assert_eq!(values, vec!["value2".to_string(), "value3".to_string()]);
            }
        }

        let mut expected_graph = DependencyGraph::default();
        expected_graph.add_dependency(ResourceId::from("a1"), ResourceId::from("b1"));
        expected_graph.add_dependency(ResourceId::from("a1"), ResourceId::from("b2"));
        expected_graph.add_dependency(ResourceId::from("a2"), ResourceId::from("b1"));
        expected_graph.add_dependency(ResourceId::from("a2"), ResourceId::from("b2"));
        expected_graph.add_dependency(ResourceId::from("b1"), ResourceId::from("c1"));
        expected_graph.add_dependency(ResourceId::from("b1"), ResourceId::from("c2"));
        expected_graph.add_dependency(ResourceId::from("b2"), ResourceId::from("c2"));
        expected_graph.add_dependency(ResourceId::from("b2"), ResourceId::from("c3"));
        assert_eq!(&*manager.dependencies.read(), &expected_graph);
    }

    #[derive(Default)]
    pub struct TestResourceCyclicRefLoader(());

    #[async_trait]
    impl ResourceLoader<()> for TestResourceCyclicRefLoader {
        async fn load(
            &self,
            mut data: ResourceReadData,
            ctx: &ResourceLoadContext,
        ) -> Result<Box<()>, ResourceLoadError> {
            let mut id_str = String::new();
            data.read_to_string(&mut id_str).await?;

            let _res = ctx.get_with_loader(
                id_str,
                TestResourceCyclicRefLoader::default(),
            ).await?;

            Ok(Box::new(()))
        }
    }

    #[test]
    fn test_resource_manager_cyclic_load() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("a", b"b", "h_a");
        data_maps[0].insert("b", b"c", "h_b");
        data_maps[0].insert("c", b"d", "h_c");
        data_maps[0].insert("d", b"b", "h_d");

        let fut = manager.get_with_loader("a", TestResourceCyclicRefLoader::default());
        let err = future::block_on(timeout(fut, Duration::from_secs(1)))
            .expect_err("Cyclic load should be detected");
        assert_matches!(err, ResourceLoadError::CyclicLoad { parents, id } => {
            assert_eq!(id, ResourceId::from("b"));
            assert_eq!(parents, vec![
                ResourceId::from("a"),
                ResourceId::from("b"),
                ResourceId::from("c"),
                ResourceId::from("d"),
            ]);
        });
    }
}