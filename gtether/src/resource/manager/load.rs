use ahash::{HashMap, HashSet};
use educe::Educe;
use futures_core::future::BoxFuture;
use futures_util::task::AtomicWaker;
use parking_lot::RwLock;
use smol::future::FutureExt;
use std::any::{Any, TypeId};
use std::fmt::{Display, Formatter};
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{ready, Context, Poll, Waker};
use tracing::{debug, info_span, trace, warn, Instrument};

use crate::resource::id::ResourceId;
use crate::resource::manager::dependency::{DependencyGraph, DependencyGraphLayer};
use crate::resource::manager::source::Sources;
use crate::resource::manager::task::{ManagerExecutor, TaskPriority};
use crate::resource::manager::update::UpdateLock;
use crate::resource::manager::{GetOrLoad, ManagerTask, ResourceFuture};
use crate::resource::source::{ResourceUpdate, SourceIndex};
use crate::resource::{
    res_key,
    BoxedResKey,
    ResKey,
    Resource,
    ResourceDefaultLoader,
    ResourceLoadError,
    ResourceLoadResult,
    ResourceLoader,
    ResourceLoaderSourceType,
    ResourceMut,
};
use crate::util::waker::MultiWaker;

/// Priority for resource loading tasks.
///
/// Higher priorities will be chosen first when the internal async executor picks tasks to execute.
#[derive(Debug, Clone, Copy)]
pub enum LoadPriority {
    /// Load this resource as soon as possible.
    ///
    /// Use the `i64` value for more specific priority ordering. Higher values mean higher priority.
    Immediate(i64),

    /// This resource is not needed immediately, so load it when there is time.
    ///
    /// use the `i64` value for more specific priority ordering. Higher values mean higher priority.
    Delayed(i64),
}

impl LoadPriority {
    /// [Immediate](Self::Immediate) priority with a default sub-priority of `0`.
    #[inline]
    pub fn immediate() -> Self {
        Self::Immediate(0)
    }

    /// [Delayed](Self::Delayed) priority with a default sub-priority of `0`.
    #[inline]
    pub fn delayed() -> Self {
        Self::Delayed(0)
    }
}

impl Default for LoadPriority {
    #[inline]
    fn default() -> Self { LoadPriority::immediate() }
}

impl Display for LoadPriority {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadPriority::Immediate(val) => write!(f, "Immediate({val})"),
            LoadPriority::Delayed(val) => write!(f, "Delayed({val})"),
        }
    }
}

impl From<LoadPriority> for TaskPriority {
    #[inline]
    fn from(value: LoadPriority) -> Self {
        match value {
            LoadPriority::Immediate(val) => Self::Immediate(val),
            LoadPriority::Delayed(val) => Self::Delayed(val),
        }
    }
}

// This exists primarily to keep duplicate loads from expiring during complex
// load dependency trees
pub type ResourceLoadContextCache = HashMap<Box<dyn ResKey>, Arc<dyn Any + Send + Sync + 'static>>;

/// Contextual data for loading [Resources](Resource).
///
/// This is created by the [ResourceManager], and passed to resource
/// [`load()`](ResourceLoader::load) methods. It provides information on the current load process,
/// and allows for loading sub-resources as dependencies.
pub struct ResourceLoadContext {
    cache: Arc<Cache>,
    key: Box<dyn ResKey>,
    priority: TaskPriority,
    dependencies: RwLock<DependencyGraphLayer>,
    resource_cache: Arc<RwLock<ResourceLoadContextCache>>,
}

impl ResourceLoadContext {
    fn new(
        cache: Arc<Cache>,
        key: impl Into<Box<dyn ResKey>>,
        priority: TaskPriority,
        parents: impl IntoIterator<Item=impl Into<Box<dyn ResKey>>>,
        resource_cache: Arc<RwLock<ResourceLoadContextCache>>,
    ) -> Self {
        let key = key.into();
        let dependencies = DependencyGraphLayer::new(key.clone(), parents);
        Self {
            cache,
            key,
            priority,
            dependencies: RwLock::new(dependencies),
            resource_cache,
        }
    }

    /// The resource [key](ResKey) that is currently being loaded.
    #[inline]
    pub fn key(&self) -> &dyn ResKey {
        &*self.key
    }

    /// Mark a [Resource] as a dependency of the currently loading Resource.
    ///
    /// Note that [`get()`](Self::get)/[`get_with_loader()`](Self::get_with_loader) automatically
    /// mark the loaded Resource as a dependency if it succeeds in loading.
    #[inline]
    pub fn add_dependency(&self, key: impl AsRef<dyn ResKey>) {
        self.dependencies.write().insert(key.as_ref().to_box());
    }

    #[inline]
    pub(in crate::resource) fn cache_resource<T>(&self, resource: Arc<Resource<T>>)
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.resource_cache.write().insert(
            resource.key().to_box(),
            resource as Arc<dyn Any + Send + Sync + 'static>,
        );
    }

    /// Spawn a new async task on the ResourceManager's executor.
    ///
    /// The spawned task will use the same priority as the currently loading resource. This method
    /// is intended to allow for resource loading to branch into multiple async tasks for more
    /// efficiency.
    #[inline]
    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> ManagerTask<T> {
        self.cache.executor.spawn(self.priority, future)
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
    ) -> ResourceFuture<'_, GetOrLoad<T::Loader>>
    where
        T: ?Sized + ResourceDefaultLoader,
    {
        self.get_with_loader(id, T::default_loader())
    }

    /// Get/load a [Resource].
    ///
    /// Uses the same [LoadPriority] as the currently loading resource.
    ///
    /// See also: [ResourceManager#loading]
    pub fn get_with_loader<L>(
        &self,
        id: impl Into<ResourceId>,
        loader: L,
    ) -> ResourceFuture<'_, GetOrLoad<L>>
    where
        L: ResourceLoader,
    {
        let id = id.into();
        let key = res_key(id, &loader);
        let parents: HashSet<_> = {
            let deps = self.dependencies.read();
            deps.parents()
                .cloned()
                .chain([deps.key().to_box()])
                .collect()
        };

        let load_params = ResourceLoadParams {
            loader: Arc::new(loader),
            priority: self.priority,
            parents: parents.clone(),
            resource_cache: self.resource_cache.clone(),
            operation: ResourceLoadOperation::Load,
        };

        if parents.contains(&key) {
            let result = Err(ResourceLoadError::CyclicLoad {
                parents,
                key,
            });
            ResourceFuture::with_context(
                GetOrLoad::Cached(result),
                id,
                self.cache.clone(),
                load_params,
                self,
            )
        } else {
            let future = self.cache.get_or_load(
                id,
                load_params.clone(),
            );
            ResourceFuture::with_context(future, id, self.cache.clone(), load_params, self)
        }
    }
}

#[derive(Educe)]
#[educe(Clone, Debug)]
pub struct ResourceLoadParams<L: ?Sized> {
    #[educe(Debug(ignore))]
    pub loader: Arc<L>,
    pub priority: TaskPriority,
    pub parents: HashSet<Box<dyn ResKey>>,
    #[educe(Debug(ignore))]
    pub resource_cache: Arc<RwLock<ResourceLoadContextCache>>,
    pub operation: ResourceLoadOperation,
}

impl<L: ?Sized> ResourceLoadParams<L> {
    #[inline]
    pub fn with_loader<L2: ?Sized>(
        self,
        loader: Arc<L2>,
    ) -> ResourceLoadParams<L2> {
        ResourceLoadParams {
            loader,
            priority: self.priority,
            parents: self.parents,
            resource_cache: self.resource_cache,
            operation: self.operation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLoadOperation {
    Load,
    Update,
}

pub type ManagerLoadTask<T> = ManagerTask<Result<ResourceTaskData<T>, ResourceLoadError>>;
pub type ManagerUpdateTask = ManagerTask<()>;

pub struct ResourceTaskData<T: ?Sized + Send + Sync + 'static> {
    pub value: Box<T>,
    pub dependencies: DependencyGraphLayer,
}

#[derive(Debug, Clone)]
pub struct SourceMetadata {
    pub hash: Option<String>,
    pub source_idx: SourceIndex,
}

impl Default for SourceMetadata {
    fn default() -> Self {
        Self {
            hash: None,
            source_idx: SourceIndex::max(),
        }
    }
}

impl SourceMetadata {
    #[inline]
    pub fn valid_for_update(&self, new_hash: &str, new_source_idx: &SourceIndex) -> bool {
        if let Some(hash) = &self.hash && hash == new_hash {
            return false
        }
        new_source_idx <= &self.source_idx
    }
}

struct UpdateWork<L: ?Sized + ResourceLoader> {
    id: ResourceId,
    source_idx: SourceIndex,
    loader: Arc<L>,
    load_params: ResourceLoadParams<L>,
    load_fn: Arc<dyn (Fn(
        ResourceLoadContext,
    ) -> BoxFuture<'static, Result<ResourceTaskData<L::Output>, ResourceLoadError>>) + Send + Sync + 'static>,
    cache: Weak<Cache>,
    wakers: Arc<MultiWaker>,
    update_lock: Arc<UpdateLock>,
}

impl<L: ?Sized + ResourceLoader> UpdateWork<L> {
    fn new(
        id: ResourceId,
        source_idx: SourceIndex,
        loader: Arc<L>,
        cache: &Arc<Cache>,
        wakers: Arc<MultiWaker>,
        update_lock: Arc<UpdateLock>,
    ) -> Self {
        let load_params = ResourceLoadParams {
            loader: loader.clone(),
            priority: TaskPriority::Update,
            parents: HashSet::default(),
            resource_cache: Arc::new(RwLock::new(HashMap::default())),
            operation: ResourceLoadOperation::Update,
        };
        let load_fn = Arc::new(cache.find_and_load_fn::<L>(id, &load_params));

        Self {
            id,
            source_idx,
            loader,
            load_params,
            load_fn,
            cache: Arc::downgrade(cache),
            wakers,
            update_lock,
        }
    }

    fn check_output_requested(
        group: &mut CacheGroup,
        id: ResourceId,
        loader: &Arc<L>,
        task_data: ResourceTaskData<L::Output>,
        wakers: &Arc<MultiWaker>,
    ) {
        if !wakers.is_empty() {
            // Entry was expired, but there are update futures waiting on it, so re-cache
            let resource = Arc::new(Resource::new(
                (id, &**loader),
                task_data.value,
            ));
            let entry = CacheEntryTyped::from_parts(
                id,
                loader.clone(),
                CacheEntryTypedInner::Loaded {
                    strong: resource,
                },
            );
            group.insert(entry);
        } else {
            group.remove(loader);
        }
    }

    fn handle_expired(
        group: &mut CacheGroup,
        id: ResourceId,
        loader: &Arc<L>,
        task_data: ResourceTaskData<L::Output>,
        wakers: &Arc<MultiWaker>,
    ) {
        debug!(%id, "Cache entry expired before resource finished updating");
        Self::check_output_requested(group, id, loader, task_data, wakers);
    }

    async fn execute(self) {
        let cache = match self.cache.upgrade() {
            Some(cache) => cache,
            None => {
                // no cache left; nothing to update
                return
            },
        };

        let key = res_key(self.id, &*self.loader);

        let _start_load_lock = self.update_lock.start_load().await;
        let result = {
            #[cfg(test)]
            let _load_lock = self.update_lock.sync.load.run(key.clone()).await;
            let ctx = ResourceLoadContext::new(
                cache.clone(),
                key.clone(),
                self.load_params.priority,
                self.load_params.parents,
                self.load_params.resource_cache,
            );
            (self.load_fn)(ctx).await
        };

        #[cfg(test)]
        let _update_lock = self.update_lock.sync.update.run(key).await;

        let task_data = match result {
            Ok(task_data) => task_data,
            Err(error) => {
                warn!(
                    id = %self.id,
                    ?error,
                    "Resource failed to load while updating; will be left unchanged",
                );
                let entries = cache.entries.read().await;
                if let Some(group) = entries.group(self.id) {
                    let mut group = group.write().await;
                    // check for cancellation before making a mutating change
                    smol::future::yield_now().await;
                    if let Some(entry) = group.get_mut(&self.loader) {
                        if let CacheEntryTypedInner::Updating { weak, .. } = &entry.inner {
                            entry.inner = CacheEntryTypedInner::Value { weak: weak.clone() };
                        }
                    }
                }
                return
            }
        };

        let resource = {
            let entries = cache.entries_with_group(self.id).await;
            let group_lock = entries.group(self.id).unwrap();
            let mut group = group_lock.write().await;
            // check for cancellation before making any mutating changes
            smol::future::yield_now().await;
            let entry = match group.get_mut(&self.loader) {
                Some(entry) => entry,
                None => {
                    Self::handle_expired(
                        &mut group,
                        self.id,
                        &self.loader,
                        task_data,
                        &self.wakers,
                    );
                    return
                }
            };

            match &mut entry.inner {
                CacheEntryTypedInner::Loading { wakers, .. } => {
                    self.wakers.extend(wakers);
                    let resource = Arc::new(Resource::new(
                        (self.id, &*self.loader),
                        task_data.value,
                    ));
                    entry.inner = CacheEntryTypedInner::Loaded {
                        strong: resource.clone(),
                    };
                    task_data.dependencies.apply(&mut cache.dependencies.write());
                    cache.sources.watch_n(&self.id, &self.source_idx);

                    entry.inner = if !self.wakers.is_empty() {
                        // at least one future is waiting on this, so we can set a strong reference
                        CacheEntryTypedInner::Loaded {
                            strong: resource,
                        }
                    } else {
                        CacheEntryTypedInner::Value {
                            weak: Arc::downgrade(&resource),
                        }
                    };

                    return
                },
                CacheEntryTypedInner::Loaded { strong } => {
                    strong.clone()
                },
                CacheEntryTypedInner::Value { weak } |
                CacheEntryTypedInner::Updating { weak, .. }=> {
                    if let Some(strong) = weak.upgrade() {
                        strong
                    } else {
                        Self::handle_expired(
                            &mut group,
                            self.id,
                            &self.loader,
                            task_data,
                            &self.wakers,
                        );
                        return
                    }
                },
                CacheEntryTypedInner::Error(_) => {
                    debug!(
                        id = %self.id,
                        "Tried to update errored Resource, ignoring",
                    );
                    Self::check_output_requested(
                        &mut group,
                        self.id,
                        &self.loader,
                        task_data,
                        &self.wakers,
                    );
                    return
                },
            }
        };

        task_data.dependencies.apply(&mut cache.dependencies.write());
        cache.sources.watch_n(&self.id, &self.source_idx);

        let (resource_mut, drop_checker) = ResourceMut::from_resource(resource.clone());
        self.loader.update(resource_mut, task_data.value).await;

        // Drop checker is used to wait until ResourceMut is no longer used, in case the update
        // needed to be delayed beyond the resolution of loader.update()
        match drop_checker.recv().await {
            Ok(_) => { debug!("Drop-checker received an unexpected message"); },
            Err(_) => { /* sender was dropped, this is expected */ },
        }

        resource.update_sub_resources().await;

        let entries = cache.entries_with_group(self.id).await;
        let group_lock = entries.group(self.id).unwrap();
        let mut group = group_lock.write().await;
        // check for cancellation before making a mutating change
        smol::future::yield_now().await;

        let mut entry_inner = if !self.wakers.is_empty() {
            // at least one future is waiting on this, so we can insert a strong reference
            CacheEntryTypedInner::Loaded {
                strong: resource,
            }
        } else {
            CacheEntryTypedInner::Value {
                weak: Arc::downgrade(&resource),
            }
        };
        if let Some(entry) = group.get_mut(&self.loader) {
            std::mem::swap(&mut entry.inner, &mut entry_inner);
            if let CacheEntryTypedInner::Updating { task, .. } = entry_inner {
                // Let this task cleanly complete without being canceled.
                task.detach();
            }
        } else {
            group.insert(CacheEntryTyped::from_parts(
                self.id,
                self.loader,
                entry_inner,
            ));
        }
    }
}

enum EntryGetResult<T: ?Sized + Send + Sync + 'static> {
    Ok(Arc<Resource<T>>),
    Err(ResourceLoadError),
    Expired,
}

impl<T: ?Sized + Send + Sync + 'static> EntryGetResult<T> {
    #[inline]
    fn from_weak(weak: &Weak<Resource<T>>) -> Self {
        match weak.upgrade() {
            Some(strong) => Self::Ok(strong),
            None => Self::Expired,
        }
    }

    fn try_result(self, force_retry: bool) -> Option<ResourceLoadResult<T>> {
        match (self, force_retry) {
            (Self::Ok(resource), _) => Some(Ok(resource)),
            (Self::Err(error), false) => Some(Err(error)),
            (Self::Err(_), true) => None,
            (Self::Expired, _) => None,
        }
    }
}

impl<T: ?Sized + Send + Sync + 'static> From<ResourceLoadResult<T>> for EntryGetResult<T> {
    fn from(result: ResourceLoadResult<T>) -> Self {
        match result {
            Ok(resource) => Self::Ok(resource),
            Err(error) => Self::Err(error),
        }
    }
}

/*#[derive(Debug, Clone)]
struct SourceMetadata {
    hash: String,
    source_idx: SourceIndex,
}

#[derive(Debug, Clone)]
enum CacheEntrySourceMetadata {
    NoSource,
    SingleSource(SourceMetadata),
}

impl CacheEntrySourceMetadata {
    fn valid_for_update(&self, new_hash: &str, new_source_idx: &SourceIndex) -> bool {
        match self {
            Self::NoSource => true,
            Self::SingleSource(metadata) =>
                &metadata.hash != new_hash && new_source_idx <= &metadata.source_idx,
        }
    }
}*/

enum CacheEntryTypedInner<L: ?Sized + ResourceLoader> {
    Loading {
        task: ManagerLoadTask<L::Output>, // Box<ManagerLoadTask<T>>
        wakers: Arc<MultiWaker>,
        dependencies: Arc<RwLock<DependencyGraph>>,
    },
    // This is a special edge-case state where loading has been completed, but a strong reference
    // has not yet been retrieved, so we need to keep one to keep it from being dropped. This mostly
    // only occurs during certain race conditions when updating an actively loading resource.
    Loaded {
        strong: Arc<Resource<L::Output>>, // Arc<Resource<T>>,
    },
    Value {
        weak: Weak<Resource<L::Output>>, // Weak<Resource<T>>,
    },
    Error(ResourceLoadError),
    Updating {
        weak: Weak<Resource<L::Output>>,
        task: ManagerUpdateTask,
        wakers: Arc<MultiWaker>,
    }
}

pub struct CacheEntryTyped<L: ?Sized + ResourceLoader> {
    id: ResourceId,
    loader: Arc<L>,
    inner: CacheEntryTypedInner<L>,
}

impl<L: ?Sized + ResourceLoader> CacheEntryTyped<L> {
    fn from_load_task(
        task: ManagerLoadTask<L::Output>,
        id: ResourceId,
        loader: Arc<L>,
        dependencies: Arc<RwLock<DependencyGraph>>,
    ) -> Self {
        Self {
            id,
            loader,
            inner: CacheEntryTypedInner::Loading {
                task,
                wakers: Arc::new(MultiWaker::default()),
                dependencies,
            },
        }
    }

    fn from_parts(
        id: ResourceId,
        loader: Arc<L>,
        inner: CacheEntryTypedInner<L>,
    ) -> Self {
        Self {
            id,
            loader,
            inner,
        }
    }

    fn try_get(&self) -> Option<EntryGetResult<L::Output>> {
        match &self.inner {
            CacheEntryTypedInner::Loading { .. } => None,
            CacheEntryTypedInner::Value{ weak, .. } =>
                Some(EntryGetResult::from_weak(weak)),
            CacheEntryTypedInner::Error(error) =>
                Some(EntryGetResult::Err(error.clone())),
            // Technically these can be polled, but we need them to be mutated to transform into ::Value
            CacheEntryTypedInner::Loaded { .. } | CacheEntryTypedInner::Updating { .. } => None,
        }
    }

    fn poll(&mut self, new_waker: Option<&Arc<AtomicWaker>>) -> Poll<EntryGetResult<L::Output>> {
        match &mut self.inner {
            CacheEntryTypedInner::Loading {
                task,
                wakers,
                dependencies,
                ..
            } => {
                if let Some(new_waker) = new_waker {
                    wakers.push(new_waker);
                }
                let waker = wakers.clone().into();
                let mut cx = Context::from_waker(&waker);
                match task.poll(&mut cx) {
                    Poll::Ready(result) => {
                        match result {
                            Ok(data) => {
                                let resource = Arc::new(Resource::new(
                                    (self.id, &*self.loader),
                                    data.value,
                                ));
                                data.dependencies.apply(&mut dependencies.write());
                                self.inner = CacheEntryTypedInner::Value {
                                    weak: Arc::downgrade(&resource),
                                };
                                Poll::Ready(EntryGetResult::Ok(resource))
                            },
                            Err(error) => {
                                self.inner = CacheEntryTypedInner::Error(error.clone());
                                Poll::Ready(EntryGetResult::Err(error))
                            },
                        }
                    },
                    Poll::Pending => Poll::Pending,
                }
            },
            CacheEntryTypedInner::Loaded {
                strong,
            } => {
                let strong = strong.clone();
                self.inner = CacheEntryTypedInner::Value {
                    weak: Arc::downgrade(&strong),
                };
                Poll::Ready(EntryGetResult::Ok(strong))
            },
            CacheEntryTypedInner::Value{ weak, .. } =>
                Poll::Ready(EntryGetResult::from_weak(weak)),
            CacheEntryTypedInner::Error(error) =>
                Poll::Ready(EntryGetResult::Err(error.clone())),
            CacheEntryTypedInner::Updating {
                weak,
                task,
                wakers,
            } => {
                if let Some(new_waker) = new_waker {
                    wakers.push(new_waker);
                }
                let waker = wakers.clone().into();
                let mut cx = Context::from_waker(&waker);
                match task.poll(&mut cx) {
                    Poll::Ready(()) => {
                        // The task itself should replace the inner type with a different one, so
                        // this should never be reachable
                        unreachable!("update task should be replaced before completing")
                    },
                    Poll::Pending => {
                        match weak.upgrade() {
                            Some(resource)
                                => Poll::Ready(EntryGetResult::Ok(resource)),
                            None => Poll::Pending,
                        }
                    }
                }
            }
        }
    }
}

pub trait CacheEntry: Send + Sync + 'static {
    fn start_update(
        &mut self,
        cache: &Arc<Cache>,
        src_idx: &SourceIndex,
        update_lock: Arc<UpdateLock>,
    ) -> bool;
}

impl<L: ?Sized + ResourceLoader> CacheEntry for CacheEntryTyped<L> {
    fn start_update(
        &mut self,
        cache: &Arc<Cache>,
        src_idx: &SourceIndex,
        update_lock: Arc<UpdateLock>,
    ) -> bool {
        // Use an empty "Value" as a placeholder
        let mut inner = CacheEntryTypedInner::Value { weak: Weak::default() };
        std::mem::swap(&mut inner, &mut self.inner);

        let (weak, prev_task, wakers) = match inner {
            CacheEntryTypedInner::Loading { wakers, .. } =>
                (Weak::default(), None, wakers),
            CacheEntryTypedInner::Loaded { strong } =>
                (Arc::downgrade(&strong), None, Arc::new(MultiWaker::default())),
            CacheEntryTypedInner::Value { weak } => {
                if let Some(_) = weak.upgrade() {
                    (weak, None, Arc::new(MultiWaker::default()))
                } else {
                    // Nothing to update
                    self.inner = CacheEntryTypedInner::Value { weak };
                    #[cfg(test)]
                    update_lock.sync.update.mark_attempt(res_key(self.id, &*self.loader));
                    return false
                }
            },
            CacheEntryTypedInner::Error(error) => {
                // Nothing to update
                self.inner = CacheEntryTypedInner::Error(error);
                #[cfg(test)]
                update_lock.sync.update.mark_attempt(res_key(self.id, &*self.loader));
                return false
            },
            CacheEntryTypedInner::Updating { weak, task, wakers }
                => (weak, Some(task), wakers),
        };

        let cancel_fut = match prev_task {
            Some(prev_task) => {
                let mut fut = Box::pin(prev_task.cancel());
                let mut cx = Context::from_waker(Waker::noop());
                // Poll once to ensure that the cancellation has been triggered while there is still
                // a mutable lock on the entry (implied by this method taking `&mut self`)
                match fut.poll(&mut cx) {
                    Poll::Ready(_) => None,
                    Poll::Pending => Some(fut),
                }
            },
            None => None,
        };

        let work = UpdateWork::new(
            self.id,
            src_idx.clone(),
            self.loader.clone(),
            cache,
            wakers.clone(),
            update_lock,
        );

        let task = cache.executor.spawn(
            TaskPriority::Update,
            async move {
                if let Some(cancel_fut) = cancel_fut {
                    cancel_fut.await;
                }
                work.execute().await;
            }.instrument(info_span!("resource_update", id = %self.id))
        );

        self.inner = CacheEntryTypedInner::Updating {
            weak,
            task,
            wakers
        };

        true
    }
}

#[derive(Educe)]
#[educe(Default)]
pub struct CacheSubGroupTyped<L: ?Sized + ResourceLoader> {
    entries: HashMap<L::Key, CacheEntryTyped<L>>,
}

impl<L: ?Sized + ResourceLoader> CacheSubGroupTyped<L> {
    #[inline]
    pub fn get(&self, loader: &Arc<L>) -> Option<&CacheEntryTyped<L>> {
        let key = loader.key();
        self.entries.get(&key)
    }

    #[inline]
    pub fn get_mut(&mut self, loader: &Arc<L>) -> Option<&mut CacheEntryTyped<L>> {
        let key = loader.key();
        self.entries.get_mut(&key)
    }

    #[inline]
    pub fn insert(&mut self, new_entry: CacheEntryTyped<L>) -> Option<CacheEntryTyped<L>> {
        let key = new_entry.loader.key();
        self.entries.insert(key, new_entry)
    }

    #[inline]
    pub fn remove(&mut self, loader: &Arc<L>) -> Option<CacheEntryTyped<L>> {
        let key = loader.key();
        self.entries.remove(&key)
    }
}

pub trait CacheSubGroup: Any + Send + Sync + 'static {
    fn start_update(
        &mut self,
        cache: &Arc<Cache>,
        src_idx: &SourceIndex,
        update_lock: &Arc<UpdateLock>,
        dependent: bool,
    ) -> HashSet<BoxedResKey>;
}

impl<L: ?Sized + ResourceLoader> CacheSubGroup for CacheSubGroupTyped<L> {
    fn start_update(
        &mut self,
        cache: &Arc<Cache>,
        src_idx: &SourceIndex,
        update_lock: &Arc<UpdateLock>,
        dependent: bool,
    ) -> HashSet<BoxedResKey> {
        let mut keys = HashSet::default();
        let mut remove = HashSet::default();
        for (k, entry) in self.entries.iter_mut() {
            if entry.loader.source_type() == ResourceLoaderSourceType::Virtual && !dependent {
                #[cfg(test)]
                update_lock.sync.update.mark_attempt(res_key(entry.id, &*entry.loader));
                continue
            }
            if entry.start_update(cache, src_idx, update_lock.clone()) {
                keys.insert(res_key(entry.id, &*entry.loader));
            } else {
                remove.insert(k.clone());
            }
        }
        for k in remove {
            self.entries.remove(&k);
        }
        keys
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheGroupUpdateResult {
    Skipped,
    Moved,
    Updated(HashSet<BoxedResKey>),
}

#[derive(Default)]
pub struct CacheGroup {
    metadata: SourceMetadata,
    sub_groups: HashMap<TypeId, Box<dyn CacheSubGroup>>,
}

impl CacheGroup {
    pub fn sub_group<L>(&self) -> Option<&CacheSubGroupTyped<L>>
    where
        L: ?Sized + ResourceLoader,
    {
        if let Some(sub_group) = self.sub_groups.get(&TypeId::of::<L>()) {
            let sub_group = (&**sub_group as &dyn Any)
                .downcast_ref::<CacheSubGroupTyped<L>>()
                .expect("Cache entry type should match key type");
            Some(sub_group)
        } else {
            None
        }
    }

    pub fn sub_group_mut<L>(&mut self) -> Option<&mut CacheSubGroupTyped<L>>
    where
        L: ?Sized + ResourceLoader,
    {
        if let Some(sub_group) = self.sub_groups.get_mut(&TypeId::of::<L>()) {
            let sub_group = (&mut **sub_group as &mut dyn Any)
                .downcast_mut::<CacheSubGroupTyped<L>>()
                .expect("Cache entry type should match key type");
            Some(sub_group)
        } else {
            None
        }
    }

    #[inline]
    pub fn get<L>(&self, loader: &Arc<L>) -> Option<&CacheEntryTyped<L>>
    where
        L: ?Sized + ResourceLoader,
    {
        self.sub_group::<L>()
            .and_then(|sub_group| sub_group.get(loader))
    }

    #[inline]
    pub fn get_mut<L>(&mut self, loader: &Arc<L>) -> Option<&mut CacheEntryTyped<L>>
    where
        L: ?Sized + ResourceLoader,
    {
        self.sub_group_mut::<L>()
            .and_then(|sub_group| sub_group.get_mut(loader))
    }

    pub fn insert<L: ?Sized + ResourceLoader>(
        &mut self,
        new_entry: CacheEntryTyped<L>,
    ) -> Option<CacheEntryTyped<L>> {
        let sub_group = self.sub_groups.entry(TypeId::of::<L>())
            .or_insert_with(|| Box::new(CacheSubGroupTyped::<L>::default()));
        let sub_group = (&mut **sub_group as &mut dyn Any)
            .downcast_mut::<CacheSubGroupTyped<L>>()
            .expect("Cache entry type should match key type");
        sub_group.insert(new_entry)
    }

    pub fn remove<L>(&mut self, loader: &Arc<L>) -> Option<CacheEntryTyped<L>>
    where
        L: ?Sized + ResourceLoader
    {
        if let Some(sub_group) = self.sub_group_mut::<L>() {
            sub_group.remove(loader)
        } else {
            None
        }
    }

    pub fn start_update(
        &mut self,
        id: ResourceId,
        update: ResourceUpdate,
        source_idx: &SourceIndex,
        sources: &Sources,
        update_lock: &Arc<UpdateLock>,
        cache: &Arc<Cache>,
    ) -> CacheGroupUpdateResult {
        let (hash, source_idx) = match update {
            ResourceUpdate::Added(hash) | ResourceUpdate::Modified(hash) => {
                if !self.metadata.valid_for_update(&hash, source_idx) {
                    return CacheGroupUpdateResult::Skipped
                }

                (hash, source_idx.clone())
            },
            ResourceUpdate::Removed => {
                let new_source = match sources.find_hash(&id) {
                    Some(new_source) => new_source,
                    // Data was removed, but there isn't a backup. Leave existing
                    // entry alone for now
                    None => {
                        warn!(%id, "All valid data sources have been removed for active resource");
                        return CacheGroupUpdateResult::Skipped
                    },
                };

                (new_source.hash, new_source.idx)
            },
            ResourceUpdate::MovedSourceIndex(old_source_idx) => {
                let result = if self.metadata.source_idx == old_source_idx {
                    self.metadata.source_idx = source_idx.clone();
                    CacheGroupUpdateResult::Moved
                } else {
                    CacheGroupUpdateResult::Skipped
                };
                return result
            },
        };

        self.metadata.hash = Some(hash);
        self.metadata.source_idx = source_idx;

        let mut keys = HashSet::default();
        for sub_group in self.sub_groups.values_mut() {
            keys.extend(sub_group.start_update(
                cache,
                &self.metadata.source_idx,
                update_lock,
                false,
            ));
        }
        CacheGroupUpdateResult::Updated(keys)
    }

    pub fn start_dependent_update(
        &mut self,
        key: &BoxedResKey,
        update_lock: &Arc<UpdateLock>,
        cache: &Arc<Cache>,
    ) -> CacheGroupUpdateResult {
        if let Some(sub_group) = self.sub_groups.get_mut(&key.loader_type()) {
            let keys = sub_group.start_update(
                cache,
                &self.metadata.source_idx,
                update_lock,
                true,
            );
            CacheGroupUpdateResult::Updated(keys)
        } else {
            CacheGroupUpdateResult::Skipped
        }
    }
}

#[derive(Default)]
pub struct CacheEntries(HashMap<ResourceId, smol::lock::RwLock<CacheGroup>>);

impl CacheEntries {
    #[inline]
    pub fn group(&self, id: ResourceId) -> Option<&smol::lock::RwLock<CacheGroup>> {
        self.0.get(&id)
    }

    #[inline]
    pub fn init_group(&mut self, id: ResourceId) {
        self.0.entry(id).or_default();
    }

    #[inline]
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

pub(in crate::resource) struct Cache {
    entries: smol::lock::RwLock<CacheEntries>,
    executor: Arc<ManagerExecutor>,
    sources: Arc<Sources>,
    dependencies: Arc<RwLock<DependencyGraph>>,
    #[cfg(test)]
    sync_load: Arc<super::tests::SyncContext<BoxedResKey>>,
}

impl Cache {
    #[inline]
    pub fn new(
        executor: Arc<ManagerExecutor>,
        sources: Arc<Sources>,
        dependencies: Arc<RwLock<DependencyGraph>>,
        #[cfg(test)]
        sync_load: Arc<super::tests::SyncContext<BoxedResKey>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            entries: smol::lock::RwLock::new(CacheEntries::default()),
            executor,
            sources,
            dependencies,
            #[cfg(test)]
            sync_load,
        })
    }

    #[inline]
    pub fn entries(&self) -> &smol::lock::RwLock<CacheEntries> {
        &self.entries
    }

    pub async fn entries_with_group(
        &self,
        id: ResourceId,
    ) -> smol::lock::RwLockReadGuard<'_, CacheEntries> {
        let entries = self.entries.upgradable_read().await;
        match entries.group(id) {
            Some(_) => smol::lock::RwLockUpgradableReadGuard::downgrade(entries),
            None => {
                let mut entries = smol::lock::RwLockUpgradableReadGuard::upgrade(entries).await;
                entries.init_group(id);
                smol::lock::RwLockWriteGuard::downgrade(entries)
            }
        }
    }

    pub fn entries_with_group_blocking(
        &self,
        id: ResourceId,
    ) -> smol::lock::RwLockReadGuard<'_, CacheEntries> {
        let entries = self.entries.upgradable_read_blocking();
        match entries.group(id) {
            Some(_) => smol::lock::RwLockUpgradableReadGuard::downgrade(entries),
            None => {
                let mut entries = smol::lock::RwLockUpgradableReadGuard::upgrade_blocking(entries);
                entries.init_group(id);
                smol::lock::RwLockWriteGuard::downgrade(entries)
            }
        }
    }

    async fn update_metadata(
        &self,
        id: ResourceId,
        hash: String,
        source_idx: SourceIndex,
    ) {
        let entries = self.entries.read().await;
        if let Some(group) = entries.group(id) {
            let mut group = group.write().await;
            if source_idx < group.metadata.source_idx {
                group.metadata.source_idx = source_idx;
                group.metadata.hash = Some(hash);
            }
        }
    }

    fn get<'lock, L>(
        self: &Arc<Self>,
        id: ResourceId,
        load_fn: &Arc<impl (Fn(
            ResourceLoadContext,
        ) -> BoxFuture<'static, Result<ResourceTaskData<L::Output>, ResourceLoadError>>) + Send + Sync + 'static>,
        load_params: &ResourceLoadParams<L>,
        force_retry: bool,
        group: smol::lock::RwLockUpgradableReadGuard<'lock, CacheGroup>,
    ) -> Result<GetOrLoad<L>, smol::lock::RwLockWriteGuard<'lock, CacheGroup>>
    where
        L: ?Sized + ResourceLoader,
    {
        if let Some(entry) = group.get(&load_params.loader) {
            if let Some(get_result) = entry.try_get() {
                if let Some(result) = get_result.try_result(force_retry) {
                    return Ok(GetOrLoad::Cached(result))
                } // else expired, start a new load task
            } else {
                // Entry exists, but needs to be mutated or is still loading
                let waker = Arc::new(AtomicWaker::new());
                let mut group = smol::lock::RwLockUpgradableReadGuard::upgrade_blocking(group);
                let entry = group.get_mut(&load_params.loader).unwrap();
                match entry.poll(Some(&waker)) {
                    Poll::Ready(get_result) => {
                        if let Some(result) = get_result.try_result(force_retry) {
                            return Ok(GetOrLoad::Cached(result))
                        } // else expired, start a new load task
                    },
                    Poll::Pending => {
                        // Yield future for existing load task
                        return Ok(GetOrLoad::Loading(ResourceLoadFuture {
                            cache: self.clone(),
                            id,
                            load_fn: load_fn.clone(),
                            load_params: load_params.clone(),
                            waker,
                        }))
                    },
                }
                return Err(group)
            }
        }

        Err(smol::lock::RwLockUpgradableReadGuard::upgrade_blocking(group))
    }

    pub fn get_or_load<L>(
        self: &Arc<Self>,
        id: ResourceId,
        load_params: ResourceLoadParams<L>,
    ) -> GetOrLoad<L>
    where
        L: ?Sized + ResourceLoader,
    {
        let load_fn = self.find_and_load_fn(id, &load_params);
        self.get_or_load_impl(id, load_params, load_fn, false)
    }

    pub fn load_default<L, DefaultFn>(
        self: &Arc<Self>,
        id: ResourceId,
        load_params: ResourceLoadParams<L>,
        default_fn: DefaultFn,
    ) -> GetOrLoad<L>
    where
        L: ?Sized + ResourceLoader,
        for<'any> DefaultFn: (Fn(&'any ResourceLoadContext)
            -> BoxFuture<'any, Result<Box<L::Output>, ResourceLoadError>>) + Send + Sync + 'static,
    {
        let load_fn = self.load_default_fn(id, &load_params, default_fn);
        self.get_or_load_impl(id, load_params, load_fn, true)
    }

    fn get_or_load_impl<L>(
        self: &Arc<Self>,
        id: ResourceId,
        load_params: ResourceLoadParams<L>,
        load_fn: impl (Fn(
            ResourceLoadContext,
        ) -> BoxFuture<'static, Result<ResourceTaskData<L::Output>, ResourceLoadError>>) + Send + Sync + 'static,
        force_retry: bool,
    ) -> GetOrLoad<L>
    where
        L: ?Sized + ResourceLoader,
    {
        let entries = self.entries_with_group_blocking(id);

        let load_fn = Arc::new(load_fn);

        let group = entries.group(id).unwrap();
        let mut group_lock = match self.get(
            id,
            &load_fn,
            &load_params,
            force_retry,
            group.upgradable_read_blocking(),
        ) {
            Ok(future) => return future,
            Err(group) => group,
        };

        // Expired or otherwise doesn't exist, start a new load task
        let waker = Arc::new(AtomicWaker::new());
        let poll = self.start_load_task(
            id,
            load_params.clone(),
            load_fn.clone(),
            &waker,
            &mut group_lock,
        );

        match poll {
            Poll::Ready(result) => GetOrLoad::Cached(result),
            Poll::Pending => GetOrLoad::Loading(ResourceLoadFuture {
                cache: self.clone(),
                id,
                load_fn: load_fn.clone(),
                load_params,
                waker,
            })
        }
    }

    fn load_default_fn<L, DefaultFn>(
        self: &Arc<Self>,
        id: ResourceId,
        load_params: &ResourceLoadParams<L>,
        load_default_fn: DefaultFn,
    ) -> impl (Fn(
        ResourceLoadContext,
    ) -> BoxFuture<'static, Result<ResourceTaskData<L::Output>, ResourceLoadError>>) + Send + Sync + 'static
    where
        L: ?Sized + ResourceLoader,
        for<'any> DefaultFn: (Fn(&'any ResourceLoadContext)
            -> BoxFuture<'any, Result<Box<L::Output>, ResourceLoadError>>) + Send + Sync + 'static,
    {
        let sources = self.sources.clone();
        let operation = load_params.operation;
        let load_default_fn = Arc::new(load_default_fn);

        move |ctx| {
            let sources = sources.clone();
            let load_default_fn = load_default_fn.clone();
            async move {
                if operation == ResourceLoadOperation::Load {
                    // If this is the original load, we need to start watching for updates
                    sources.watch_n(&id, &SourceIndex::max());
                }

                match load_default_fn(&ctx).await {
                    Ok(value) => {
                        let dependencies = ctx.dependencies.into_inner();
                        Ok(ResourceTaskData {
                            value,
                            dependencies,
                        })
                    },
                    Err(e) => Err(e),
                }
            }.boxed()
        }
    }

    fn find_and_load_fn<L>(
        self: &Arc<Self>,
        id: ResourceId,
        load_params: &ResourceLoadParams<L>,
    ) -> impl (Fn(
        ResourceLoadContext,
    ) -> BoxFuture<'static, Result<ResourceTaskData<L::Output>, ResourceLoadError>>) + Send + Sync + 'static
    where
        L: ?Sized + ResourceLoader,
    {
        let cache = Arc::downgrade(self);
        let sources = self.sources.clone();
        let operation = load_params.operation;
        let loader = load_params.loader.clone();

        move |ctx| {
            let cache = cache.clone();
            let sources = sources.clone();
            let loader = loader.clone();
            async move {
                let data = match loader.source_type() {
                    ResourceLoaderSourceType::FirstFound => {
                        match sources.find_data(&id).await {
                            Some(Ok(data)) => {
                                trace!(
                                    source_idx = %data.source.idx,
                                    hash = ?data.source.hash,
                                    "Found source data"
                                );
                                if let Some(cache) = cache.upgrade() {
                                    cache.update_metadata(
                                        id,
                                        data.source.hash,
                                        data.source.idx.clone(),
                                    ).await;
                                }

                                if operation == ResourceLoadOperation::Load {
                                    // If this is the original load, we need to start watching for updates
                                    sources.watch_n(&id, &data.source.idx)
                                }

                                data.data
                            },
                            Some(Err(e)) => return Err(e),
                            None => return Err(ResourceLoadError::NotFound(id.clone())),
                        }
                    },
                    ResourceLoaderSourceType::Virtual => {
                        Box::pin(smol::io::empty())
                    },
                };

                match loader.load(data, &ctx).await {
                    Ok(value) => {
                        let dependencies = ctx.dependencies.into_inner();
                        Ok(ResourceTaskData {
                            value,
                            dependencies,
                        })
                    },
                    Err(e) => Err(e),
                }
            }.boxed()
        }
    }

    fn create_load_task<L>(
        self: &Arc<Self>,
        id: ResourceId,
        load_params: ResourceLoadParams<L>,
        load_fn: Arc<impl (Fn(
            ResourceLoadContext,
        ) -> BoxFuture<'static, Result<ResourceTaskData<L::Output>, ResourceLoadError>>) + ?Sized + Send + Sync + 'static>,
    ) -> ManagerLoadTask<L::Output>
    where
        L: ?Sized + ResourceLoader,
    {
        trace!(%id, "Creating load task");

        let cache = self.clone();
        #[cfg(test)]
        let sync_load = self.sync_load.clone();

        let span = if load_params.parents.is_empty() {
            info_span!("resource_load", id = %id, priority = %load_params.priority)
        } else {
            info_span!("sub_load", id = %id)
        };

        self.executor.spawn(load_params.priority, async move {
            let key = res_key(id, &*load_params.loader);
            #[cfg(test)]
            let _test_ctx_lock = sync_load.run(key.clone()).await;

            let ctx = ResourceLoadContext::new(
                cache,
                key,
                load_params.priority,
                load_params.parents,
                load_params.resource_cache,
            );

            load_fn(ctx).await
        }.instrument(span))
    }

    fn start_load_task<L>(
        self: &Arc<Cache>,
        id: ResourceId,
        load_params: ResourceLoadParams<L>,
        load_fn: Arc<impl (Fn(
            ResourceLoadContext,
        ) -> BoxFuture<'static, Result<ResourceTaskData<L::Output>, ResourceLoadError>>) + ?Sized + Send + Sync + 'static>,
        waker: &Arc<AtomicWaker>,
        group: &mut CacheGroup,
    ) -> Poll<ResourceLoadResult<L::Output>>
    where
        L: ?Sized + ResourceLoader,
    {
        let loader = load_params.loader.clone();
        let task = self.create_load_task(
            id,
            load_params,
            load_fn,
        );

        let mut entry = CacheEntryTyped::<L>::from_load_task(
            task,
            id,
            loader,
            self.dependencies.clone(),
        );

        // Poll immediately so that the waker can be properly installed in the LoadTask
        let poll_result = entry.poll(Some(waker));
        group.insert(entry);

        match poll_result {
            Poll::Ready(get_result) => Poll::Ready(get_result.try_result(false)
                .expect("Resource load task should not expire before first poll()")),
            Poll::Pending => Poll::Pending,
        }
    }

    #[inline]
    pub fn clear(&self) {
        self.entries.write_blocking().clear();
    }
}

/// Future for awaiting a [Resource] load operation.
pub struct ResourceLoadFuture<L: ?Sized + ResourceLoader> {
    cache: Arc<Cache>,
    id: ResourceId,
    load_fn: Arc<dyn (Fn(
        ResourceLoadContext,
    ) -> BoxFuture<'static, Result<ResourceTaskData<L::Output>, ResourceLoadError>>) + Send + Sync + 'static>,
    load_params: ResourceLoadParams<L>,
    waker: Arc<AtomicWaker>,
}

impl<L> Future for ResourceLoadFuture<L>
where
    L: ?Sized + ResourceLoader,
{
    type Output = ResourceLoadResult<L::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.waker.register(cx.waker());

        let entries = self.cache.entries_with_group_blocking(self.id);
        let group_lock = entries.group(self.id).unwrap();
        let group = group_lock.upgradable_read_blocking();

        let maybe_result = match group.get(&self.load_params.loader) {
            Some(entry) => entry.try_get(),
            None => Some(EntryGetResult::Expired),
        };

        let (group, result) = match maybe_result {
            Some(result) => (group, result),
            None => {
                let mut group = smol::lock::RwLockUpgradableReadGuard::upgrade_blocking(group);
                let poll_result = match group.get_mut(&self.load_params.loader) {
                    Some(entry) => entry.poll(None),
                    None => Poll::Ready(EntryGetResult::Expired),
                };
                let result = ready!(poll_result);
                (smol::lock::RwLockWriteGuard::downgrade_to_upgradable(group), result)
            }
        };

        match result.try_result(false) {
            Some(result) => Poll::Ready(result),
            None => {
                trace!(
                    id = %self.id,
                    "Entry expired, starting new load task",
                );
                // Expired, start a new load task
                let mut group = smol::lock::RwLockUpgradableReadGuard::upgrade_blocking(group);
                self.cache.start_load_task(
                    self.id,
                    self.load_params.clone(),
                    self.load_fn.clone(),
                    &self.waker,
                    &mut group,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use macro_rules_attribute::apply;
    use rstest::rstest;
    use smol::io::AsyncReadExt;
    use smol_macros::test as smol_test;
    use std::fmt::Debug;
    use std::time::Duration;
    use test_log::test as test_log;

    use crate::resource::manager::tests::prelude::*;
    use crate::resource::{IdentityLoader, ResourceReadData};

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_manager_can_drop_while_loading(
        test_resource_ctx: TestResourceContext<1>,
    ) {
        test_resource_ctx.data_maps[0].lock().insert("key", b"value", "h_value");

        let sync_load = test_resource_ctx.manager.test_ctx().sync_load.clone();
        let _lock = sync_load.block().await;
        let fut = test_resource_ctx.manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        );
        drop(fut);

        timeout(
            assert_manager_drops(test_resource_ctx.manager),
            Duration::from_secs(5),
        ).await;
    }

    #[rstest]
    #[case::not_found(
        None,
        ExpectedLoadInfo {
            load_info: TestLoadInfo::new("key", TestResLoader::new("key")),
            expected: ExpectedLoadResult::<String>::NotFound(ResourceId::from("key")),
        },
        0,
    )]
    #[case::error(
        Some(TestDataEntry::new("invalid", b"\xC0", "h_invalid")),
        ExpectedLoadInfo {
            load_info: TestLoadInfo::new("invalid", TestResLoader::new("invalid")),
            expected: ExpectedLoadResult::<String>::ReadErr,
        },
        1,
    )]
    #[case::ok(
        Some(TestDataEntry::new("key", b"value", "h_value")),
        ExpectedLoadInfo {
            load_info: TestLoadInfo::new("key", TestResLoader::new("key")),
            expected: ExpectedLoadResult::Ok(String::from("value")),
        },
        1,
    )]
    #[case::ok_virtual(
        None,
        ExpectedLoadInfo {
            load_info: TestLoadInfo::new("key", TestVirtualLoader::new(String::from("value"))),
            expected: ExpectedLoadResult::Ok(String::from("value"))
        },
        0,
    )]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load<L, A>(
        test_resource_ctx: TestResourceContext<1>,
        #[case] start_data: Option<TestDataEntry>,
        #[case] expected_entry: ExpectedLoadInfo<L, A>,
        #[case] expected_load_count: usize,
    )
    where
        L: ResourceLoader + Clone,
        L::Output: Debug + Sized,
        A: Debug + PartialEq<L::Output>,
    {
        if let Some(start_data) = start_data {
            test_resource_ctx.data_maps[0].lock().insert(start_data.id, start_data.data, start_data.hash);
        }

        let key = expected_entry.load_info.key();
        let fut = {
            let _lock = test_resource_ctx.manager.test_ctx().sync_load.block().await;
            let fut = test_resource_ctx.manager.get_with_loader(
                expected_entry.load_info.id,
                expected_entry.load_info.loader.clone(),
            );
            fut.check().expect_err("Resource should not be loaded yet")
        };

        let result = fut.await;
        // Hold a reference to keep possibly loaded values from expiring
        let _maybe_value = expected_entry.expected.assert_matches(result);
        test_resource_ctx.manager.test_ctx().sync_load.assert_count(&key, 1);
        test_resource_ctx.data_maps[0].assert_load_count(
            expected_entry.load_info.id,
            expected_load_count,
        );

        let result = test_resource_ctx.manager.get_with_loader(
            expected_entry.load_info.id,
            expected_entry.load_info.loader.clone(),
        )
            .check()
            .expect("Result should be cached and pollable");
        expected_entry.expected.assert_matches(result);
        // No extra loads should have been made
        test_resource_ctx.manager.test_ctx().sync_load.assert_count(&key, 1);
        test_resource_ctx.data_maps[0].assert_load_count(
            expected_entry.load_info.id,
            expected_load_count,
        );
    }

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load_expired(
        test_resource_ctx: TestResourceContext<1>,
    ) {
        test_resource_ctx.data_maps[0].lock().insert("key", b"value", "h_value");

        test_resource_ctx.data_maps[0].assert_watch("key", false);
        let key = res_key("key", &TestResLoader::new("key"));
        let (fut1, fut2) = {
            let _lock = test_resource_ctx.manager.test_ctx().sync_load.block().await;
            let fut1 = test_resource_ctx.manager.get_with_loader("key", TestResLoader::new("key"));
            let fut2 = test_resource_ctx.manager.get_with_loader("key", TestResLoader::new("key"));
            (fut1, fut2)
        };

        let value = fut1.await.expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());
        test_resource_ctx.data_maps[0].assert_watch("key", true);
        test_resource_ctx.manager.test_ctx().sync_load.assert_count(&key, 1);

        drop(value);

        let value = fut2.await.expect("Resource should reload for 'key' after first handle was dropped");
        assert_eq!(*value.read(), "value".to_owned());
        test_resource_ctx.manager.test_ctx().sync_load.assert_count(&key, 2);
    }

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load_source_order(
        test_resource_ctx: TestResourceContext<3>,
    ) {
        test_resource_ctx.data_maps[1].lock().insert("key", b"value_1", "h_value_1");
        test_resource_ctx.data_maps[2].lock().insert("key", b"value_2", "h_value_2");

        // Load should retrieve "value_1", as it's earlier in the priority chain
        let value = test_resource_ctx.manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value_1".to_owned());
        test_resource_ctx.data_maps[0].assert_watch("key", true);
        test_resource_ctx.data_maps[1].assert_watch("key", true);
        test_resource_ctx.data_maps[2].assert_watch("key", false);
    }

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load_multiple_types(
        test_resource_ctx: TestResourceContext<1>,
    ) {
        test_resource_ctx.data_maps[0].lock().insert("key", b"value", "h_value");

        let string_value = test_resource_ctx.manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("String resource should load for 'key'");
        let id_value = test_resource_ctx.manager.get::<ResourceId>("key")
            .await.expect("ResourceId resource should load for 'key'");
        let virtual_value = test_resource_ctx.manager.get_with_loader(
            "key",
            TestVirtualLoader::new(String::from("value")),
        ).await.expect("Virtual String resource should load for 'key'");

        assert_eq!(*string_value.read(), String::from("value"));
        assert_eq!(*id_value.read(), ResourceId::from("key"));
        assert_eq!(*virtual_value.read(), String::from("value"));

        let string_loader_key = res_key("key", &TestResLoader::new("key"));
        let id_loader_key = res_key("key", &IdentityLoader);
        let virtual_loader_key = res_key("key", &TestVirtualLoader::new(String::from("value")));
        test_resource_ctx.manager.test_ctx().sync_load.assert_count(&string_loader_key, 1);
        test_resource_ctx.manager.test_ctx().sync_load.assert_count(&id_loader_key, 1);
        test_resource_ctx.manager.test_ctx().sync_load.assert_count(&virtual_loader_key, 1);
        test_resource_ctx.data_maps[0].assert_load_count("key", 2);
    }

    #[rstest]
    #[case::single(
        true, DependencyGraph::builder()
            .add(
                res_key("a", &ref_loader!(():TestResLoader::new("b"))),
                [res_key("b", &TestResLoader::new("b"))]
            )
            .build(),
        [TestDataEntry::new("b", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", ref_loader!(():TestResLoader::new("b"))),
            expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec!["value"])),
        }],
        [
            (res_key("a", &ref_loader!(():TestResLoader::new("b"))), 1),
            (res_key("b", &TestResLoader::new("b")), 1),
        ],
        [("a", 1), ("b", 1)],
    )]
    #[case::single_virtual(
        false, DependencyGraph::builder()
            .add(
                res_key("a", &ref_loader!(VRef("b"):TestResLoader::new("b"))),
                [res_key("b", &TestResLoader::new("b"))]
            )
            .build(),
        [TestDataEntry::new("b", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", ref_loader!(VRef("b"):TestResLoader::new("b"))),
            expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec!["value"])),
        }],
        [
            (res_key("a", &ref_loader!(VRef("b"):TestResLoader::new("b"))), 1),
            (res_key("b", &TestResLoader::new("b")), 1),
        ],
        [("a", 0), ("b", 1)],
    )]
    #[case::linear(
        true, DependencyGraph::builder()
            .add(
                res_key("a", &ref_loader!(Ref:Ref:TestResLoader::new("c"))),
                [res_key("b", &ref_loader!(():TestResLoader::new("c")))]
            )
            .add(
                res_key("b", &ref_loader!(():TestResLoader::new("c"))),
                [res_key("c", &TestResLoader::new("c"))]
            )
            .build(),
        [TestDataEntry::new("c", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", ref_loader!(():():TestResLoader::new("c"))),
            expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec![
                ExpectedResourceRef::new(vec!["value"]),
            ])),
        }],
        [
            (res_key("a", &ref_loader!(():():TestResLoader::new("c"))), 1),
            (res_key("b", &ref_loader!(():TestResLoader::new("c"))), 1),
            (res_key("c", &TestResLoader::new("c")), 1),
        ],
        [("a", 1), ("b", 1), ("c", 1)],
    )]
    #[case::tree(
        true, DependencyGraph::builder()
            .add(
                res_key("a1", &ref_loader!(():():StringLoader)),
                [
                    res_key("b1", &ref_loader!(():StringLoader)),
                    res_key("b2", &ref_loader!(():StringLoader)),
                ],
            )
            .add(
                res_key("a2", &ref_loader!(():():StringLoader)),
                [
                    res_key("b1", &ref_loader!(():StringLoader)),
                    res_key("b2", &ref_loader!(():StringLoader)),
                ],
            )
            .add(
                res_key("b1", &ref_loader!(():StringLoader)),
                [
                    res_key("c1", &StringLoader),
                    res_key("c2", &StringLoader),
                ],
            )
            .add(
                res_key("b2", &ref_loader!(():StringLoader)),
                [
                    res_key("c2", &StringLoader),
                    res_key("c3", &StringLoader),
                ],
            )
            .build(),
        [
            TestDataEntry::new("c1", b"value1", "h_c1"),
            TestDataEntry::new("c2", b"value2", "h_c2"),
            TestDataEntry::new("c3", b"value3", "h_c3"),
        ],
        [
            ExpectedLoadInfo {
                load_info: TestLoadInfo::new("a1", ref_loader!(():():StringLoader)),
                expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec![
                    ExpectedResourceRef::new(vec!["value1", "value2"]),
                    ExpectedResourceRef::new(vec!["value2", "value3"]),
                ])),
            },
            ExpectedLoadInfo {
                load_info: TestLoadInfo::new("a2", ref_loader!(():():StringLoader)),
                expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec![
                    ExpectedResourceRef::new(vec!["value1", "value2"]),
                    ExpectedResourceRef::new(vec!["value2", "value3"]),
                ])),
            },
        ],
        [
            // TestRefLoader returns resources that hold a reference to their sub-resource,
            // preventing them from expiring. This means all loads should be cached.
            (res_key("a1", &ref_loader!(():():StringLoader)), 1),
            (res_key("a2", &ref_loader!(():():StringLoader)), 1),
            (res_key("b1", &ref_loader!(():StringLoader)), 1),
            (res_key("b2", &ref_loader!(():StringLoader)), 1),
            (res_key("c1", &StringLoader), 1),
            (res_key("c2", &StringLoader), 1),
            (res_key("c3", &StringLoader), 1),
        ],
        [("a1", 1), ("a2", 1), ("b1", 1), ("b2", 1), ("c1", 1), ("c2", 1), ("c3", 1)],
    )]
    #[case::diamond(
        true, DependencyGraph::builder()
            .add(
                res_key("a", &TestResChainLoader),
                [
                    res_key("b1", &TestResChainLoader),
                    res_key("b2", &TestResChainLoader),
                    res_key("b3", &TestResChainLoader),
                ],
            )
            .add(res_key("b1", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .add(res_key("b2", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .add(res_key("b3", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .build(),
        [TestDataEntry::new("c", b"value", "h_c")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", TestResChainLoader::default()),
            expected: ExpectedLoadResult::Ok(vec![
                "a:b1:c:value",
                "a:b2:c:value",
                "a:b3:c:value",
            ]),
        }],
        [
            (res_key("a", &TestResChainLoader), 1),
            (res_key("b1", &TestResChainLoader), 1),
            (res_key("b2", &TestResChainLoader), 1),
            (res_key("b3", &TestResChainLoader), 1),
            // Even though b1..b3 loads this, it _should_ be cached during the overall "a" load
            (res_key("c", &TestResChainLoader), 1),
        ],
        [("a", 1), ("b1", 1), ("b2", 1), ("b3", 1), ("c", 1)],
    )]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load_dependencies<L, A>(
        test_resource_ctx: TestResourceContext<1>,
        #[case] insert_dependency_data: bool,
        #[case] expected_graph: DependencyGraph,
        #[case] data_entries: impl IntoIterator<Item=TestDataEntry>,
        #[case] expected_entries: impl IntoIterator<Item=ExpectedLoadInfo<L, A>>,
        #[case] expected_load_counts: impl IntoIterator<Item=(BoxedResKey, usize)>,
        #[case] expected_data_load_counts: impl IntoIterator<Item=(impl Into<ResourceId>, usize)>,
    )
    where
        L: ResourceLoader,
        L::Output: Debug + Sized,
        A: Debug + PartialEq<L::Output>,
    {
        if insert_dependency_data {
            setup_dependency_data(&test_resource_ctx.data_maps[0], &expected_graph);
        }
        {
            let mut data_map = test_resource_ctx.data_maps[0].lock();
            for entry in data_entries.into_iter() {
                data_map.insert(entry.id, entry.data, entry.hash);
            }
        }

        let mut values = Vec::new();
        for entry in expected_entries.into_iter() {
            let result = test_resource_ctx.manager.get_with_loader(
                entry.load_info.id,
                entry.load_info.loader,
            ).await;
            values.push(entry.expected.assert_matches(result));
        }

        assert_eq!(&*test_resource_ctx.manager.dependencies.read(), &expected_graph);
        for (key, expected_count) in expected_load_counts.into_iter() {
            test_resource_ctx.manager.test_ctx().sync_load.assert_count(&key, expected_count);
        }
        for (id, expected_count) in expected_data_load_counts.into_iter() {
            test_resource_ctx.data_maps[0].assert_load_count(id, expected_count);
        }

        // Drop any held resources and check again
        drop(values);
        assert_eq!(&*test_resource_ctx.manager.dependencies.read(), &expected_graph);
    }

    #[derive(Default)]
    pub struct TestResourceCyclicRefLoader;

    #[async_trait]
    impl ResourceLoader for TestResourceCyclicRefLoader {
        type Output = ();
        type Key = ();

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

        #[inline(always)]
        fn key(&self) -> Self::Key { () }
    }

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load_cyclic(
        test_resource_ctx: TestResourceContext<1>,
    ) {
        {
            let mut data_map = test_resource_ctx.data_maps[0].lock();
            data_map.insert("a", b"b", "h_a");
            data_map.insert("b", b"c", "h_b");
            data_map.insert("c", b"d", "h_c");
            data_map.insert("d", b"b", "h_d");
        }

        let err = test_resource_ctx.manager.get_with_loader("a", TestResourceCyclicRefLoader).await
            .expect_err("Cyclic load should be detected");
        let expected_key = res_key("b", &TestResourceCyclicRefLoader);
        let expected_parents = [
            res_key("a", &TestResourceCyclicRefLoader),
            res_key("b", &TestResourceCyclicRefLoader),
            res_key("c", &TestResourceCyclicRefLoader),
            res_key("d", &TestResourceCyclicRefLoader),
        ].into_iter().collect::<HashSet<_>>();
        assert_matches!(err, ResourceLoadError::CyclicLoad { parents, key } => {
            assert_eq!(&key, &expected_key);
            assert_eq!(parents, expected_parents);
        });
    }
}