use ahash::HashMap;
use educe::Educe;
use futures_core::future::BoxFuture;
use futures_util::task::AtomicWaker;
use parking_lot::{Mutex, RwLock};
use smol::future::FutureExt;
use std::any::{Any, TypeId};
use std::fmt::{Display, Formatter};
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tracing::{debug, error, info_span, trace, warn, Instrument};

use crate::resource::id::ResourceId;
use crate::resource::manager::dependency::{DependencyGraph, DependencyGraphLayer};
use crate::resource::manager::executor::{ManagerExecutor, ManagerTask, TaskPriority};
use crate::resource::manager::source::Sources;
use crate::resource::manager::update::{UpdateOutput, UpdateOutputUntyped, Updates};
use crate::resource::manager::{ResourceFuture, ResourceFutureInner};
use crate::resource::source::SourceIndex;
use crate::resource::{Resource, ResourceDefaultLoader, ResourceLoadError, ResourceLoadResult, ResourceLoader, ResourceMut};
use crate::util::waker::MultiWaker;

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

impl From<LoadPriority> for TaskPriority {
    #[inline]
    fn from(value: LoadPriority) -> Self {
        match value {
            LoadPriority::Immediate => Self::Immediate,
            LoadPriority::Delayed => Self::Delayed,
        }
    }
}

// This exists primarily to keep duplicate loads from expiring during complex
// load dependency trees
pub type ResourceLoadContextCache = HashMap<ResourceId, Arc<dyn Any + Send + Sync + 'static>>;

/// Contextual data for loading [Resources](Resource).
///
/// This is created by the [ResourceManager], and passed to resource
/// [`load()`](ResourceLoader::load) methods. It provides information on the current load process,
/// and allows for loading sub-resources as dependencies.
pub struct ResourceLoadContext {
    cache: Arc<Cache>,
    id: ResourceId,
    priority: TaskPriority,
    dependencies: RwLock<DependencyGraphLayer>,
    resource_cache: Arc<RwLock<ResourceLoadContextCache>>,
}

impl ResourceLoadContext {
    fn new(
        cache: Arc<Cache>,
        id: ResourceId,
        priority: TaskPriority,
        parents: impl IntoIterator<Item=ResourceId>,
        resource_cache: Arc<RwLock<ResourceLoadContextCache>>,
    ) -> Self {
        let dependencies = DependencyGraphLayer::new(id.clone(), parents);
        Self {
            cache,
            id,
            priority,
            dependencies: RwLock::new(dependencies),
            resource_cache,
        }
    }

    /// The resource [ID](ResourceId) that is currently being loaded.
    #[inline]
    pub fn id(&self) -> ResourceId {
        self.id.clone()
    }

    /// Mark a [Resource] as a dependency of the currently loading Resource.
    ///
    /// Note that [`get()`](Self::get)/[`get_with_loader()`](Self::get_with_loader) automatically
    /// mark the loaded Resource as a dependency if it succeeds in loading.
    #[inline]
    pub fn add_dependency(&self, id: impl Into<ResourceId>) {
        self.dependencies.write().insert(id.into());
    }

    #[inline]
    pub(in crate::resource) fn cache_resource<T>(&self, resource: Arc<Resource<T>>)
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.resource_cache.write().insert(
            resource.id().clone(),
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
            let deps = self.dependencies.read();
            deps.parents()
                .chain([deps.id()])
                .cloned()
                .collect()
        };

        let load_params = ResourceLoadParams {
            loader: Arc::new(loader),
            priority: self.priority,
            parents: parents.clone(),
            resource_cache: self.resource_cache.clone(),
            operation: ResourceLoadOperation::Load,
        };

        if parents.contains(&id) {
            let result = Err(ResourceLoadError::CyclicLoad {
                parents,
                id,
            });
            ResourceFuture::with_context(
                ResourceFutureInner::Cached(result),
                self.cache.clone(),
                load_params,
                self,
            )
        } else {
            let inner = self.cache.get_or_load(
                id,
                load_params.clone(),
            );
            ResourceFuture::with_context(inner, self.cache.clone(), load_params, self)
        }
    }
}

#[derive(Educe)]
#[educe(Clone, Debug)]
pub struct ResourceLoadParams<T: ?Sized + Send + Sync + 'static> {
    #[educe(Debug(ignore))]
    pub loader: Arc<dyn ResourceLoader<T>>,
    pub priority: TaskPriority,
    pub parents: Vec<ResourceId>,
    #[educe(Debug(ignore))]
    pub resource_cache: Arc<RwLock<ResourceLoadContextCache>>,
    pub operation: ResourceLoadOperation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceLoadOperation {
    Load,
    Update,
}

type ManagerLoadTask<T> = ManagerTask<ResourceTaskDataResult<T>>;

pub struct ResourceTaskData<T: ?Sized + Send + Sync + 'static> {
    pub value: Box<T>,
    pub dependencies: DependencyGraphLayer,
    pub hash: String,
    pub source_idx: SourceIndex,
}

pub struct ResourceTaskDataResult<T: ?Sized + Send + Sync + 'static> {
    pub id: ResourceId,
    pub result: Result<ResourceTaskData<T>, ResourceLoadError>,
    pub loader: Arc<dyn ResourceLoader<T>>,
}

pub struct UpdateParams {
    pub prev_output: Option<UpdateOutputUntyped>,
    #[cfg(test)]
    pub sync_update: Arc<super::tests::SyncContext>,
}

pub type CreateUpdateFutFn = Arc<dyn (
    Fn(
        &Arc<Cache>,
        UpdateParams,
    ) -> (BoxFuture<'static, ()>, UpdateOutputUntyped)
) + Send + Sync>;

#[derive(Debug)]
pub enum CacheEntryMetadata {
    Loading(CacheLoadingMetadata),
    Cached(CachedValueMetadata),
}

impl CacheEntryMetadata {
    #[inline]
    pub fn create_update_future(
        &self,
        cache: &Arc<Cache>,
        params: UpdateParams,
    ) -> (BoxFuture<'static, ()>, UpdateOutputUntyped) {
        match self {
            Self::Loading(metadata) => (metadata.create_update_fut_fn)(
                cache,
                params,
            ),
            Self::Cached(metadata) => (metadata.create_update_fut_fn)(
                cache,
                params,
            ),
        }
    }

    #[inline]
    pub fn hash(&self) -> Option<&str> {
        match self {
            Self::Loading(metadata) => metadata.hash.as_ref().map(String::as_str),
            Self::Cached(metadata) => Some(&metadata.hash),
        }
    }

    #[inline]
    pub fn source_idx(&self) -> &SourceIndex {
        match self {
            Self::Loading(metadata) => &metadata.source_idx,
            Self::Cached(metadata) => &metadata.source_idx,
        }
    }

    #[inline]
    pub fn valid_for_update(&self, new_hash: &str, new_source_idx: &SourceIndex) -> bool {
        if let Some(hash) = self.hash() && hash == new_hash {
            return false
        }
        new_source_idx <= self.source_idx()
    }
}

#[derive(Educe)]
#[educe(Debug)]
pub struct CacheLoadingMetadata {
    #[educe(Debug(ignore))]
    create_update_fut_fn: CreateUpdateFutFn,
    pub resource_type: TypeId,
    pub hash: Option<String>,
    pub source_idx: SourceIndex,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct CachedValueMetadata {
    #[educe(Debug(ignore))]
    _resource: Option<Arc<dyn Any + Send + Sync>>,
    #[educe(Debug(ignore))]
    create_update_fut_fn: CreateUpdateFutFn,
    pub resource_type: TypeId,
    pub hash: String,
    pub source_idx: SourceIndex,
}

enum EntryGetResult<T: ?Sized + Send + Sync + 'static> {
    Ok(Arc<Resource<T>>),
    Err(ResourceLoadError),
    Expired,
}

impl<T: ?Sized + Send + Sync + 'static> EntryGetResult<T> {
    #[inline]
    fn from_strong(strong: Arc<dyn Any + Send + Sync>, actual_resource_type: TypeId) -> Self {
        match strong.downcast::<Resource<T>>() {
            Ok(resource) => Self::Ok(resource),
            Err(_) => Self::Err(ResourceLoadError::from_mismatch::<T>(actual_resource_type)),
        }
    }

    #[inline]
    fn from_weak(weak: &Weak<dyn Any + Send + Sync>, actual_resource_type: TypeId) -> Self {
        match weak.upgrade() {
            Some(strong) => Self::from_strong(strong, actual_resource_type),
            None => Self::Expired,
        }
    }

    fn try_result(self) -> Option<ResourceLoadResult<T>> {
        match self {
            Self::Ok(resource) => Some(Ok(resource)),
            Self::Err(error) => Some(Err(error)),
            Self::Expired => None,
        }
    }
}

enum CacheEntryInner {
    Loading {
        task: Box<dyn Any + Send + Sync>, // Box<ManagerLoadTask<T>>
        wakers: Arc<MultiWaker>,
        dependencies: Arc<RwLock<DependencyGraph>>,
        hash_source_idx: Arc<Mutex<(Option<String>, SourceIndex)>>,
    },
    // This is a special edge-case state where loading has been completed, but a strong reference
    // has not yet been retrieved, so we need to keep one to keep it from being dropped. This mostly
    // only occurs during certain race conditions when updating an actively loading resource.
    Loaded {
        strong: Arc<dyn Any + Send + Sync>, // Arc<Resource<T>>,
        hash: String,
        source_idx: SourceIndex,
    },
    Value {
        weak: Weak<dyn Any + Send + Sync>, // Weak<Resource<T>>,
        hash: String,
        source_idx: SourceIndex,
    },
    Error(ResourceLoadError),
}

pub struct CacheEntry {
    resource_type: TypeId,
    inner: CacheEntryInner,
    create_update_fut_fn: CreateUpdateFutFn,
}

impl CacheEntry {
    // TODO: Can this move to the update module?
    async fn update_entry<T>(
        cache: Arc<Cache>,
        result: ResourceTaskDataResult<T>,
        output: Arc<UpdateOutput<T>>,
        #[cfg(test)]
        sync_update: Arc<super::tests::SyncContext>,
    )
    where
        T: ?Sized + Send + Sync + 'static,
    {
        #[cfg(test)]
        let _test_ctx_lock = sync_update.run().await;

        let task_data = match result.result {
            Ok(task_data) => task_data,
            Err(error) => {
                warn!(
                    id = %result.id,
                    ?error,
                    "Resource failed to load while updating; will be left unchanged",
                );
                // TODO: Should the old value be used instead?
                output.set(Err(error));
                return
            }
        };

        let check_output_requested = |
            entries: &mut HashMap<ResourceId, CacheEntry>,
            id: ResourceId,
            task_data: ResourceTaskData<T>,
            loader: Arc<dyn ResourceLoader<T>>,
        | {
            if output.has_futures() {
                // Entry was expired, but there are update futures waiting on it, so re-cache
                let resource = Arc::new(Resource::new(
                    id.clone(),
                    task_data.value,
                ));
                let entry = Self::from_resource(
                    &resource,
                    loader,
                    task_data.hash,
                    task_data.source_idx,
                );
                entries.insert(id, entry);
                output.set(Ok(resource));
            }
        };

        let handle_expired = |
            entries: &mut HashMap<ResourceId, CacheEntry>,
            id: ResourceId,
            task_data: ResourceTaskData<T>,
            loader: Arc<dyn ResourceLoader<T>>,
        | {
            debug!(%id, "Cache entry expired before resource finished updating");
            check_output_requested(entries, id, task_data, loader);
        };

        let handle_mismatch = |id: ResourceId, actual: TypeId| {
            let requested = TypeId::of::<T>();
            warn!(
                %id,
                ?requested,
                ?actual,
                "Tried to update resource with mismatched types",
            );
            output.set(Err(ResourceLoadError::TypeMismatch { requested, actual }));
        };

        let resource = {
            let mut entries = cache.entries.write();
            let entry = match entries.get_mut(&result.id) {
                Some(entry) => entry,
                None => {
                    handle_expired(&mut entries, result.id, task_data, result.loader);
                    return
                }
            };

            match &mut entry.inner {
                CacheEntryInner::Loading { .. } => {
                    let resource = Arc::new(Resource::new(result.id.clone(), task_data.value));
                    entry.inner = CacheEntryInner::Loaded {
                        strong: resource.clone(),
                        hash: task_data.hash,
                        source_idx: task_data.source_idx.clone(),
                    };
                    task_data.dependencies.apply(&mut cache.dependencies.write());
                    cache.sources.watch_n(&result.id, &task_data.source_idx);
                    output.set(Ok(resource));
                    return
                },
                CacheEntryInner::Loaded { strong, hash, source_idx } => {
                    if let Ok(resource) = strong.clone().downcast::<Resource<T>>() {
                        *hash = task_data.hash;
                        *source_idx = task_data.source_idx.clone();
                        resource
                    } else {
                        handle_mismatch(result.id, entry.resource_type);
                        return
                    }
                },
                CacheEntryInner::Value { weak, hash, source_idx } => {
                    if let Some(strong) = weak.upgrade() {
                        if let Ok(resource) = strong.downcast::<Resource<T>>() {
                            *hash = task_data.hash;
                            *source_idx = task_data.source_idx.clone();
                            resource
                        } else {
                            handle_mismatch(result.id, entry.resource_type);
                            return
                        }
                    } else {
                        handle_expired(&mut entries, result.id, task_data, result.loader);
                        return
                    }
                },
                CacheEntryInner::Error(_) => {
                    debug!(
                        id = %result.id,
                        "Tried to update errored Resource, ignoring",
                    );
                    check_output_requested(&mut entries, result.id, task_data, result.loader);
                    return
                },
            }
        };

        task_data.dependencies.apply(&mut cache.dependencies.write());
        cache.sources.watch_n(&result.id, &task_data.source_idx);

        let (resource_mut, drop_checker) = ResourceMut::from_resource(resource.clone());
        result.loader.update(resource_mut, task_data.value).await;

        // Drop checker is used to wait until ResourceMut is no longer used, in case the update
        // needed to be delayed beyond the resolution of loader.update()
        match drop_checker.recv().await {
            Ok(_) => { debug!("Drop-checker received an unexpected message"); },
            Err(_) => { /* sender was dropped, this is expected */ },
        }

        resource.update_sub_resources().await;

        output.set(Ok(resource));
    }

    fn create_update_closure<T>(
        id: ResourceId,
        loader: Arc<dyn ResourceLoader<T>>,
    ) -> CreateUpdateFutFn
    where
        T: ?Sized + Send + Sync + 'static,
    {
        // Use a closure to type-erase T
        Arc::new(move |
            cache,
            params,
        | {
            let load_task = cache.create_load_task(
                id.clone(),
                ResourceLoadParams {
                    loader: loader.clone(),
                    priority: TaskPriority::Update,
                    parents: vec![], // TODO: Do parents need to be set for updates?
                    resource_cache: Arc::new(RwLock::new(HashMap::default())),
                    operation: ResourceLoadOperation::Update,
                },
                None,
            );

            let output = match params.prev_output {
                Some(prev_output) => match prev_output.into_typed() {
                    Ok(prev_output) => prev_output,
                    Err(error) => {
                        error!(
                            %id,
                            ?error,
                            "Mismatched types between previous update and current update; existing futures may be waiting indefinitely"
                        );
                        Arc::new(UpdateOutput::default())
                    },
                },
                None => Arc::new(UpdateOutput::default()),
            };
            let async_output = output.clone();

            let cache = cache.clone();
            let fut = async move {
                let result = load_task.await;
                Self::update_entry(
                    cache,
                    result,
                    async_output,
                    #[cfg(test)]
                    params.sync_update,
                ).await;
            };

            (fut.boxed(), output.into())
        })
    }

    fn from_load_task<T>(
        task: ManagerLoadTask<T>,
        id: ResourceId,
        loader: Arc<dyn ResourceLoader<T>>,
        hash_source_idx: Arc<Mutex<(Option<String>, SourceIndex)>>,
        dependencies: Arc<RwLock<DependencyGraph>>,
    ) -> Self
    where
        T: ?Sized + Send + Sync + 'static,
    {
        Self {
            resource_type: TypeId::of::<T>(),
            inner: CacheEntryInner::Loading {
                task: Box::new(task) as Box<dyn Any + Send + Sync>,
                wakers: Arc::new(MultiWaker::default()),
                dependencies,
                hash_source_idx,
            },
            create_update_fut_fn: Self::create_update_closure(id, loader),
        }
    }

    fn from_resource<T>(
        resource: &Arc<Resource<T>>,
        loader: Arc<dyn ResourceLoader<T>>,
        hash: String,
        source_idx: SourceIndex,
    ) -> Self
    where
        T: ?Sized + Send + Sync + 'static,
    {
        Self {
            resource_type: TypeId::of::<T>(),
            inner: CacheEntryInner::Value {
                weak: Arc::downgrade(resource) as Weak<dyn Any + Send + Sync>,
                hash,
                source_idx,
            },
            create_update_fut_fn: Self::create_update_closure(resource.id().clone(), loader),
        }
    }

    fn try_get<T>(&self) -> Option<EntryGetResult<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        match &self.inner {
            CacheEntryInner::Loading { .. } => None,
            // Technically this can be polled, but we need it be mutated to transform into ::Value
            CacheEntryInner::Loaded { .. } => None,
            CacheEntryInner::Value{ weak, .. } =>
                Some(EntryGetResult::from_weak(weak, self.resource_type)),
            CacheEntryInner::Error(error) =>
                Some(EntryGetResult::Err(error.clone())),
        }
    }

    fn poll<T>(&mut self, new_waker: Option<&Arc<AtomicWaker>>) -> Poll<EntryGetResult<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        match &mut self.inner {
            CacheEntryInner::Loading { task, wakers, dependencies, .. } => {
                match task.downcast_mut::<ManagerLoadTask<T>>() {
                    Some(task) => {
                        if let Some(new_waker) = new_waker {
                            wakers.push(new_waker);
                        }
                        let waker = wakers.clone().into();
                        let mut cx = Context::from_waker(&waker);
                        match task.poll(&mut cx) {
                            Poll::Ready(result) => {
                                match result.result {
                                    Ok(data) => {
                                        let resource = Arc::new(Resource::new(
                                            result.id.clone(),
                                            data.value,
                                        ));
                                        data.dependencies.apply(&mut dependencies.write());
                                        self.inner = CacheEntryInner::Value {
                                            weak: Arc::downgrade(&resource) as Weak<dyn Any + Send + Sync>,
                                            hash: data.hash,
                                            source_idx: data.source_idx,
                                        };
                                        Poll::Ready(EntryGetResult::Ok(resource))
                                    },
                                    Err(error) => {
                                        self.inner = CacheEntryInner::Error(error.clone());
                                        Poll::Ready(EntryGetResult::Err(error))
                                    },
                                }
                            },
                            Poll::Pending => Poll::Pending,
                        }
                    },
                    None => Poll::Ready(EntryGetResult::Err(
                        ResourceLoadError::from_mismatch::<T>(self.resource_type)
                    )),
                }
            },
            CacheEntryInner::Loaded { strong, hash, source_idx } => {
                let strong = strong.clone();
                self.inner = CacheEntryInner::Value {
                    weak: Arc::downgrade(&strong),
                    hash: hash.clone(),
                    source_idx: source_idx.clone(),
                };
                Poll::Ready(EntryGetResult::from_strong(strong, self.resource_type))
            },
            CacheEntryInner::Value{ weak, .. } =>
                Poll::Ready(EntryGetResult::from_weak(weak, self.resource_type)),
            CacheEntryInner::Error(error) =>
                Poll::Ready(EntryGetResult::Err(error.clone())),
        }
    }

    fn metadata(&self) -> Option<CacheEntryMetadata> {
        match &self.inner {
            CacheEntryInner::Loading { hash_source_idx, .. } => {
                let lock = hash_source_idx.lock();
                Some(CacheEntryMetadata::Loading(CacheLoadingMetadata {
                    create_update_fut_fn: self.create_update_fut_fn.clone(),
                    resource_type: self.resource_type,
                    hash: lock.0.clone(),
                    source_idx: lock.1.clone(),
                }))
            },
            CacheEntryInner::Loaded { strong, hash, source_idx } => {
                Some(CacheEntryMetadata::Cached(CachedValueMetadata {
                    _resource: Some(strong.clone()),
                    create_update_fut_fn: self.create_update_fut_fn.clone(),
                    resource_type: self.resource_type,
                    hash: hash.clone(),
                    source_idx: source_idx.clone(),
                }))
            },
            CacheEntryInner::Value{ weak, hash, source_idx } => {
                weak.upgrade().map(|value| CacheEntryMetadata::Cached(CachedValueMetadata {
                    _resource: Some(value),
                    create_update_fut_fn: self.create_update_fut_fn.clone(),
                    resource_type: self.resource_type,
                    hash: hash.clone(),
                    source_idx: source_idx.clone(),
                }))
            },
            CacheEntryInner::Error(_) => None,
        }
    }
}

pub struct Cache {
    entries: RwLock<HashMap<ResourceId, CacheEntry>>,
    executor: Arc<ManagerExecutor>,
    sources: Arc<Sources>,
    dependencies: Arc<RwLock<DependencyGraph>>,
    updates: Arc<Updates>,
    #[cfg(test)]
    sync_load: Arc<super::tests::SyncContext>,
}

impl Cache {
    #[inline]
    pub fn new(
        executor: Arc<ManagerExecutor>,
        sources: Arc<Sources>,
        dependencies: Arc<RwLock<DependencyGraph>>,
        updates: Arc<Updates>,
        #[cfg(test)]
        sync_load: Arc<super::tests::SyncContext>,
    ) -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(Default::default()),
            executor,
            sources,
            dependencies,
            updates,
            #[cfg(test)]
            sync_load,
        })
    }

    pub fn get_or_load<T>(
        self: &Arc<Self>,
        id: ResourceId,
        load_params: ResourceLoadParams<T>,
    ) -> ResourceFutureInner<T>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        if load_params.operation == ResourceLoadOperation::Update {
            if let Some(fut) = self.updates.get_future(&id) {
                return fut
            }
        }

        let mut entries = self.entries.upgradable_read();

        if let Some(entry) = entries.get(&id) {
            if let Some(get_result) = entry.try_get() {
                if let Some(result) = get_result.try_result() {
                    return ResourceFutureInner::Cached(result)
                } // else expired, start a new load task
            } else {
                // Entry exists, but needs to be mutated or is still loading
                let waker = Arc::new(AtomicWaker::new());
                let poll_result = entries.with_upgraded(|entries| {
                    let entry = entries.get_mut(&id).unwrap();
                    entry.poll(Some(&waker))
                });
                match poll_result {
                    Poll::Ready(get_result) => {
                        if let Some(result) = get_result.try_result() {
                            return ResourceFutureInner::Cached(result)
                        } // else expired, start a new load task
                    },
                    Poll::Pending => {
                        // Yield future for existing load task
                        return ResourceFutureInner::Loading(ResourceLoadFuture {
                            cache: self.clone(),
                            id,
                            load_params,
                            waker,
                        })
                    },
                }
            }
        }

        // Expired or otherwise doesn't exist, start a new load task
        let waker = Arc::new(AtomicWaker::new());
        let poll = entries.with_upgraded(|entries| {
            self.start_load_task(
                id.clone(),
                load_params.clone(),
                &waker,
                entries,
            )
        });

        match poll {
            Poll::Ready(result) => ResourceFutureInner::Cached(result),
            Poll::Pending => ResourceFutureInner::Loading(ResourceLoadFuture {
                cache: self.clone(),
                id,
                load_params,
                waker,
            })
        }
    }

    fn create_load_task<T>(
        self: &Arc<Self>,
        id: ResourceId,
        load_params: ResourceLoadParams<T>,
        hash_source_idx: Option<Arc<Mutex<(Option<String>, SourceIndex)>>>,
    ) -> ManagerLoadTask<T>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        trace!(%id, "Creating load task");

        let cache = self.clone();
        let sources = self.sources.clone();
        #[cfg(test)]
        let sync_load = self.sync_load.clone();

        let span = if load_params.parents.is_empty() {
            info_span!("resource_load", id = %id.clone(), priority = %load_params.priority)
        } else {
            info_span!("sub_load", id = %id.clone())
        };

        self.executor.spawn(load_params.priority, async move {
            #[cfg(test)]
            let _test_ctx_lock = sync_load.run().await;

            let result = match sources.find_data(&id).await {
                Some(Ok(data)) => {
                    trace!(
                        source_idx = %data.source.idx,
                        hash = ?data.source.hash,
                        "Found source data"
                    );
                    if let Some(loading_hash_source_idx) = hash_source_idx {
                        let mut lock = loading_hash_source_idx.lock();
                        lock.0 = Some(data.source.hash.clone());
                        lock.1 = data.source.idx.clone();
                    }

                    if load_params.operation == ResourceLoadOperation::Load {
                        // If this is the original load, we need to start watching for updates
                        sources.watch_n(&id, &data.source.idx)
                    }

                    let ctx = ResourceLoadContext::new(
                        cache,
                        id.clone(),
                        load_params.priority,
                        load_params.parents,
                        load_params.resource_cache.clone(),
                    );

                    match load_params.loader.load(data.data, &ctx).await {
                        Ok(value) => {
                            let dependencies = ctx.dependencies.into_inner();
                            Ok(ResourceTaskData {
                                value,
                                dependencies,
                                hash: data.source.hash,
                                source_idx: data.source.idx,
                            })
                        },
                        Err(e) => Err(e),
                    }
                },
                Some(Err(e)) => Err(e),
                None => Err(ResourceLoadError::NotFound(id.clone())),
            };

            ResourceTaskDataResult {
                id,
                result,
                loader: load_params.loader,
            }
        }.instrument(span))
    }

    fn start_load_task<T>(
        self: &Arc<Cache>,
        id: ResourceId,
        load_params: ResourceLoadParams<T>,
        waker: &Arc<AtomicWaker>,
        entries: &mut HashMap<ResourceId, CacheEntry>,
    ) -> Poll<ResourceLoadResult<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let loader = load_params.loader.clone();
        let hash_source_idx = Arc::new(Mutex::new((None, SourceIndex::min())));
        let task = self.create_load_task(
            id.clone(),
            load_params,
            Some(hash_source_idx.clone()),
        );

        let mut entry = CacheEntry::from_load_task(
            task,
            id.clone(),
            loader,
            hash_source_idx,
            self.dependencies.clone(),
        );

        // Poll immediately so that the waker can be properly installed in the LoadTask
        let poll_result = entry.poll(Some(waker));
        entries.insert(id, entry);

        match poll_result {
            Poll::Ready(get_result) => Poll::Ready(get_result.try_result()
                .expect("Resource load task should not expire before first poll()")),
            Poll::Pending => Poll::Pending,
        }
    }

    #[inline]
    pub fn metadata(&self, id: &ResourceId) -> Option<CacheEntryMetadata> {
        self.entries.read().get(id).and_then(CacheEntry::metadata)
    }

    #[inline]
    pub fn remove(&self, id: &ResourceId) -> bool {
        self.entries.write().remove(id).is_some()
    }

    #[inline]
    pub fn clear(&self) {
        self.entries.write().clear();
    }
}

pub struct ResourceLoadFuture<T: ?Sized + Send + Sync + 'static> {
    cache: Arc<Cache>,
    id: ResourceId,
    load_params: ResourceLoadParams<T>,
    waker: Arc<AtomicWaker>,
}

impl<T: ?Sized + Send + Sync + 'static> Future for ResourceLoadFuture<T> {
    type Output = ResourceLoadResult<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.waker.register(cx.waker());

        let mut entries = self.cache.entries.upgradable_read();

        let maybe_result = match entries.get(&self.id) {
            Some(entry) => entry.try_get(),
            None => Some(EntryGetResult::Expired),
        };

        let result = match maybe_result {
            Some(result) => result,
            None => {
                let poll_result = entries.with_upgraded(|entries| {
                    match entries.get_mut(&self.id) {
                        Some(entry) => entry.poll(None),
                        None => Poll::Ready(EntryGetResult::Expired),
                    }
                });
                match poll_result {
                    Poll::Ready(result) => result,
                    Poll::Pending => return Poll::Pending,
                }
            },
        };

        match result.try_result() {
            Some(result) => Poll::Ready(result),
            None => {
                trace!(
                    id = %self.id,
                    "Entry expired, starting new load task",
                );
                // Expired, start a new load task
                entries.with_upgraded(|entries| {
                    self.cache.start_load_task(
                        self.id.clone(),
                        self.load_params.clone(),
                        &self.waker,
                        entries,
                    )
                })
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
    use crate::resource::manager::ResourceManager;
    use crate::resource::ResourceReadData;

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_manager_can_drop_while_loading(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
    ) {
        data_maps[0].lock().insert("key", b"value", "h_value");

        let sync_load = manager.test_ctx().sync_load.clone();
        let _lock = sync_load.block().await;
        let fut = manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        );
        drop(fut);

        timeout(
            assert_manager_drops(manager),
            Duration::from_secs(5),
        ).await;
    }

    #[rstest]
    #[case::not_found("key", None, ExpectedLoadResult::NotFound(ResourceId::from("key")))]
    #[case::error("invalid", Some(TestDataEntry::new("invalid", b"\xC0", "h_invalid")), ExpectedLoadResult::ReadErr)]
    #[case::ok("key", Some(TestDataEntry::new("key", b"value", "h_value")), ExpectedLoadResult::Ok("value".to_string()))]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
        #[case] key: &str,
        #[case] start_data: Option<TestDataEntry>,
        #[case] expected_load_result: ExpectedLoadResult<String>,
    ) {
        if let Some(start_data) = start_data {
            data_maps[0].lock().insert(start_data.id, start_data.data, start_data.hash);
        }

        let fut = {
            let _lock = manager.test_ctx().sync_load.block().await;
            let fut = manager.get_with_loader(key, TestResLoader::new(key));
            fut.check().expect_err("Resource should not be loaded yet")
        };

        let result = fut.await;
        // Hold a reference to keep possibly loaded values from expiring
        let _maybe_value = expected_load_result.assert_matches(result);
        manager.test_ctx().sync_load.assert_count(1);

        let result = manager.get_with_loader(key, TestResLoader::new(key))
            .check()
            .expect("Result should be cached and pollable");
        expected_load_result.assert_matches(result);
        // No extra loads should have been made
        manager.test_ctx().sync_load.assert_count(1);
    }

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load_expired(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
    ) {
        data_maps[0].lock().insert("key", b"value", "h_value");

        data_maps[0].assert_watch("key", false);
        let (fut1, fut2) = {
            debug!("1");
            let _lock = manager.test_ctx().sync_load.block().await;
            let fut1 = manager.get_with_loader("key", TestResLoader::new("key"));
            let fut2 = manager.get_with_loader("key", TestResLoader::new("key"));
            (fut1, fut2)
        };

        let value = fut1.await.expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());
        data_maps[0].assert_watch("key", true);
        manager.test_ctx().sync_load.assert_count(1);

        drop(value);

        let value = fut2.await.expect("Resource should reload for 'key' after first handle was dropped");
        assert_eq!(*value.read(), "value".to_owned());
        manager.test_ctx().sync_load.assert_count(2);
    }

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load_source_order(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 3]),
    ) {
        data_maps[1].lock().insert("key", b"value_1", "h_value_1");
        data_maps[2].lock().insert("key", b"value_2", "h_value_2");

        // Load should retrieve "value_1", as it's earlier in the priority chain
        let value = manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value_1".to_owned());
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", true);
        data_maps[2].assert_watch("key", false);
    }

    #[rstest]
    #[case::single(
        DependencyGraph::builder().add("a", ["b"]).build(),
        [TestDataEntry::new("b", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", TestRefLoader::new(TestResLoader::new("b"))),
            expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec!["value"])),
        }],
        2,
    )]
    #[case::linear(
        DependencyGraph::builder().add("a", ["b"]).add("b", ["c"]).build(),
        [TestDataEntry::new("c", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new(
                "a",
                TestRefLoader::new(TestRefLoader::new(TestResLoader::new("c"))),
            ),
            expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec![
                ExpectedResourceRef::new(vec!["value"]),
            ])),
        }],
        3,
    )]
    #[case::tree(
        DependencyGraph::builder()
            .add("a1", ["b1", "b2"]).add("a2", ["b1", "b2"])
            .add("b1", ["c1", "c2"]).add("b2", ["c2", "c3"])
            .build(),
        [
            TestDataEntry::new("c1", b"value1", "h_c1"),
            TestDataEntry::new("c2", b"value2", "h_c2"),
            TestDataEntry::new("c3", b"value3", "h_c3"),
        ],
        [
            ExpectedLoadInfo {
                load_info: TestLoadInfo::new(
                    "a1",
                    TestRefLoader::new(TestRefLoader::new(StringLoader::default())),
                ),
                expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec![
                    ExpectedResourceRef::new(vec!["value1", "value2"]),
                    ExpectedResourceRef::new(vec!["value2", "value3"]),
                ])),
            },
            ExpectedLoadInfo {
                load_info: TestLoadInfo::new(
                    "a2",
                    TestRefLoader::new(TestRefLoader::new(StringLoader::default())),
                ),
                expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec![
                    ExpectedResourceRef::new(vec!["value1", "value2"]),
                    ExpectedResourceRef::new(vec!["value2", "value3"]),
                ])),
            },
        ],
        7,
    )]
    #[case::diamond(
        DependencyGraph::builder()
            .add("a", ["b1", "b2", "b3"])
            .add("b1", ["c"]).add("b2", ["c"]).add("b3", ["c"])
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
        6, // TestResChainLoader attempts to load 'value' as well, generating an extra load count
    )]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load_dependencies<T, L, A>(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
        #[case] expected_graph: DependencyGraph,
        #[case] data_entries: impl IntoIterator<Item=TestDataEntry>,
        #[case] expected_entries: impl IntoIterator<Item=ExpectedLoadInfo<T, L, A>>,
        #[case] expected_load_count: usize,
    )
    where
        T: Debug + Send + Sync + 'static,
        L: ResourceLoader<T>,
        A: Debug + PartialEq<T>,
    {
        setup_dependency_data(&data_maps[0], &expected_graph);
        {
            let mut data_map = data_maps[0].lock();
            for entry in data_entries.into_iter() {
                data_map.insert(entry.id, entry.data, entry.hash);
            }
        }

        let mut values = Vec::new();
        for entry in expected_entries.into_iter() {
            let result = manager.get_with_loader(
                entry.load_info.key,
                entry.load_info.loader,
            ).await;
            values.push(entry.expected.assert_matches(result));
        }
        manager.test_ctx().sync_load.assert_count(expected_load_count);

        assert_eq!(&*manager.dependencies.read(), &expected_graph);

        // Drop any held resources and check again
        drop(values);
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

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_load_cyclic(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
    ) {
        {
            let mut data_map = data_maps[0].lock();
            data_map.insert("a", b"b", "h_a");
            data_map.insert("b", b"c", "h_b");
            data_map.insert("c", b"d", "h_c");
            data_map.insert("d", b"b", "h_d");
        }

        let err = manager.get_with_loader("a", TestResourceCyclicRefLoader::default()).await
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