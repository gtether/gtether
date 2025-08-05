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
use futures_core::task::__internal::AtomicWaker;
use parking_lot::{Mutex, MutexGuard};
use smol::prelude::*;
use smol::{future, Executor, Task};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Wake};
use std::thread;
use std::thread::JoinHandle;
use strum::EnumCount;
use tracing::{debug, info_span, warn, Instrument};

use crate::resource::id::ResourceId;
use crate::resource::source::{ResourceSource, ResourceUpdate, ResourceWatcher, SealedResourceData, SealedResourceDataResult, SealedResourceDataSource, SourceIndex};
use crate::resource::{Resource, ResourceDefaultLoader, ResourceLoadError, ResourceLoader, ResourceMut};

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

#[repr(usize)]
#[derive(Debug, Clone, Copy, EnumCount)]
enum TaskPriority {
    Immediate = 0,
    Delayed = 1,
    Update = 2,
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

pub type ManagerTask<T> = Task<T>;

struct ManagerExecutor {
    execs: Arc<[Executor<'static>; TaskPriority::COUNT]>,
    worker: Option<(smol::channel::Sender<()>, JoinHandle<()>)>,
}

impl ManagerExecutor {
    fn new() -> Self {
        let execs = Arc::new(std::array::from_fn(|_| Executor::<'static>::new()));
        let worker_execs = execs.clone();
        let (signal, shutdown) = smol::channel::unbounded::<()>();

        let join_handle = thread::Builder::new()
            .name("resource-manager".to_string())
            .spawn(move || future::block_on(async {
                let run_forever = async {
                    loop {
                        for _ in 0..200 {
                            let t0 = worker_execs[TaskPriority::Immediate as usize].tick();
                            let t1 = worker_execs[TaskPriority::Delayed as usize].tick();
                            let t2 = worker_execs[TaskPriority::Update as usize].tick();

                            // Wait until one of the ticks completes, trying them in order from highest
                            // priority to lowest priority
                            t0.or(t1).or(t2).await;
                        }

                        // Yield every now and then
                        future::yield_now().await;
                    }
                };

                let _ = shutdown.recv().or(run_forever).await;
            })).unwrap();

        Self {
            execs,
            worker: Some((signal, join_handle)),
        }
    }

    fn spawn<T: Send + 'static>(
        &self,
        priority: TaskPriority,
        future: impl Future<Output = T> + Send + 'static,
    ) -> ManagerTask<T> {
        self.execs[priority as usize].spawn(future)
    }
}

impl Drop for ManagerExecutor {
    fn drop(&mut self) {
        if let Some((signal, join_handle)) = self.worker.take() {
            drop(signal);
            match join_handle.join() {
                Ok(()) => (),
                Err(error) =>
                    warn!(?error, "ResourceManager background thread errored"),
            }
        } else {
            warn!("ResourceManager executor join handle already taken");
        }
    }
}

struct ResourceTaskData<T: ?Sized + Send + Sync + 'static> {
    id: ResourceId,
    result: Result<(Arc<Resource<T>>, String), ResourceLoadError>,
    source_idx: SourceIndex,
    loader: Arc<dyn ResourceLoader<T>>,
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
        load_priority: LoadPriority,
    ) -> Self {
        Self {
            state: ResourceFutureState::Delayed {
                cache: entry,
                load_fn: Box::new(move || manager.get_or_load_impl(id, loader, load_priority)),
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

enum CacheEntryGetResult<T: ?Sized + Send + Sync + 'static> {
    Ok(Arc<Resource<T>>),
    Expired,
    Err(ResourceLoadError),
}

impl<T: ?Sized + Send + Sync + 'static> From<Result<(Arc<Resource<T>>, String), ResourceLoadError>> for CacheEntryGetResult<T> {
    #[inline]
    fn from(value: Result<(Arc<Resource<T>>, String), ResourceLoadError>) -> Self {
        match value {
            Ok((r, _)) => CacheEntryGetResult::Ok(r),
            Err(e) => CacheEntryGetResult::Err(e),
        }
    }
}

enum CacheEntryUpdateFutResult<'fut> {
    Skip,
    Evict,
    Run(Pin<Box<dyn Future<Output = CacheEntryUpdateResult> + Send + 'fut>>)
}

enum CacheEntryUpdateResult {
    Ok(SealedResourceDataSource),
    Expired,
    Err(ResourceLoadError),
}

type CacheEntryStateUpdateFn = Arc<dyn (Fn(&Arc<ResourceManager>, SealedResourceData)
    -> Box<dyn Future<Output = CacheEntryUpdateResult> + Send>) + Send + Sync>;

#[derive(Default)]
struct MultiWaker(Mutex<Vec<Weak<AtomicWaker>>>);

impl Wake for MultiWaker {
    fn wake(self: Arc<Self>) {
        self.0.lock().retain(|weak| {
            if let Some(waker) = weak.upgrade() {
                waker.wake();
                true
            } else {
                false
            }
        })
    }
}

impl MultiWaker {
    #[inline]
    fn push(&self, waker: &Arc<AtomicWaker>) {
        self.0.lock().push(Arc::downgrade(waker))
    }
}

struct CacheEntryStateValue {
    resource: Weak<dyn Any + Send + Sync>, // Weak<Resource<T>>
    resource_type: TypeId,
    hash: String,
    source_idx: SourceIndex,
}

enum CacheEntryState {
    Loading {
        task: Box<dyn Any + Send>, // Box<Task<ResourceTaskData<T>>>
        wakers: Arc<MultiWaker>,
        resource_type: TypeId,
        manager: Arc<ResourceManager>,
    },
    CachedValue {
        value: CacheEntryStateValue,
        update: CacheEntryStateUpdateFn,
    },
    Updating {
        value: CacheEntryStateValue,
        update: CacheEntryStateUpdateFn,
    },
    CachedError(ResourceLoadError),
    Uninit,
}

impl CacheEntryState {
    #[inline]
    fn from_task<T>(task: Task<ResourceTaskData<T>>, manager: Arc<ResourceManager>) -> Self
    where
        T: ?Sized + Send + Sync + 'static,
    {
        Self::Loading {
            task: Box::new(task) as Box<dyn Any + Send>,
            wakers: Arc::new(MultiWaker::default()),
            resource_type: TypeId::of::<T>(),
            manager,
        }
    }

    fn poll<T>(self) -> (Self, Option<CacheEntryGetResult<T>>)
    where
        T: ?Sized + Send + Sync + 'static,
    {
        match self {
            Self::Loading { task, resource_type, manager, wakers } => {
                match task.downcast::<Task<ResourceTaskData<T>>>() {
                    Ok(mut task) => {
                        let waker = wakers.clone().into();
                        let mut cx = Context::from_waker(&waker);
                        match task.poll(&mut cx) {
                            Poll::Ready(task_result) => {
                                manager.watch_n(&task_result.id, &task_result.source_idx);
                                let result = task_result.result.clone().into();
                                (task_result.into(), Some(result))
                            },
                            Poll::Pending => (
                                Self::Loading {
                                    task: task as Box<dyn Any + Send>,
                                    wakers,
                                    resource_type,
                                    manager,
                                },
                                None,
                            )
                        }
                    },
                    Err(task) => (
                        Self::Loading { task, resource_type, manager, wakers },
                        Some(CacheEntryGetResult::Err(ResourceLoadError::from_mismatch::<T>(resource_type))),
                    )
                }
            },
            Self::CachedValue { ref value, .. } | Self::Updating { ref value, .. } => {
                if let Some(resource) = value.resource.upgrade() {
                    match resource.downcast::<Resource<T>>() {
                        Ok(resource) => (self, Some(CacheEntryGetResult::Ok(resource))),
                        Err(_) => {
                            let resource_type = value.resource_type.clone();
                            (self, Some(CacheEntryGetResult::Err(ResourceLoadError::from_mismatch::<T>(resource_type))))
                        },
                    }
                } else {
                    (self, Some(CacheEntryGetResult::Expired))
                }
            },
            Self::CachedError(ref error) => {
                let error = error.clone();
                (self, Some(CacheEntryGetResult::Err(error)))
            },
            Self::Uninit =>
                panic!("Found uninit state in inner CacheEntry"),
        }
    }

    fn insert_waker(&self, waker: &Arc<AtomicWaker>) {
        match self {
            Self::Loading { wakers, .. } => {
                wakers.push(waker);
            },
            _ => {},
        }
    }

    fn update<'fut>(
        self,
        manager: &'fut Arc<ResourceManager>,
        id: &'fut ResourceId,
        source: &'fut Box<dyn ResourceSource>,
        new_idx: &'fut SourceIndex,
        ignore_priority: bool,
    ) -> (Self, CacheEntryUpdateFutResult<'fut>) {
        match self {
            Self::Loading { .. } => {
                // TODO: Queue/replace existing load
                warn!(%id, "Tried to update currently loading Resource");
                (self, CacheEntryUpdateFutResult::Skip)
            },
            Self::CachedValue { value, update } => {
                if ignore_priority || new_idx <= &value.source_idx {
                    let fut_update = update.clone();
                    let fut = Box::pin(async move {
                        let sub_data_result = match new_idx.sub_idx() {
                            Some(sub_idx) => source.sub_load(id, sub_idx).await,
                            None => source.load(id).await,
                        };
                        // TODO: Replace with custom ?/Try when that is stabilized
                        let data = match sub_data_result {
                            Ok(sub_data) => sub_data.seal(new_idx.idx()),
                            Err(e) => return CacheEntryUpdateResult::Err(e),
                        };
                        Box::into_pin(fut_update(manager, data)).await
                    });
                    (Self::Updating { value, update }, CacheEntryUpdateFutResult::Run(fut))
                } else {
                    warn!(
                        %id,
                        %new_idx,
                        source_idx = %value.source_idx,
                        "Tried to update Resource from lower priority source",
                    );
                    (Self::CachedValue { value, update }, CacheEntryUpdateFutResult::Skip)
                }
            },
            Self::Updating { .. } => {
                // TODO: Queue/replace existing update
                warn!(%id, "Tried to update currently updating Resource");
                (self, CacheEntryUpdateFutResult::Skip)
            },
            CacheEntryState::CachedError(_) => (self, CacheEntryUpdateFutResult::Evict),
            CacheEntryState::Uninit =>
                panic!("Found uninit state in inner CacheEntry"),
        }
    }

    fn finalize_update(
        self,
        data_source: SealedResourceDataSource,
    ) -> (Self, ()) {
        match self {
            Self::Updating { mut value, update } => {
                value.hash = data_source.hash;
                value.source_idx = data_source.idx;
                (Self::CachedValue { value, update }, ())
            },
            _ => {
                warn!("Tried to finalize update when not in Updating state! Possibly have inconsistent state.");
                (self, ())
            },
        }
    }

    fn hash(&self) -> Option<String> {
        match self {
            Self::Loading { .. } | Self::CachedError(_) => None,
            Self::CachedValue { value, .. } | Self::Updating { value, .. }
                => Some(value.hash.clone()),
            Self::Uninit =>
                panic!("Found uninit state in inner CacheEntry"),
        }
    }
}

impl<T> From<ResourceTaskData<T>> for CacheEntryState
where
    T: ?Sized + Send + Sync + 'static
{
    fn from(value: ResourceTaskData<T>) -> Self {
        match value.result {
            Ok((resource, hash)) => {
                let weak = Arc::downgrade(&resource);
                let id = value.id;
                let loader = value.loader;
                let source_idx = value.source_idx;
                drop(resource);
                Self::CachedValue {
                    value: CacheEntryStateValue {
                        resource: weak.clone(),
                        resource_type: TypeId::of::<T>(),
                        hash,
                        source_idx,
                    },
                    update: Arc::new(move |manager, data| Box::new({
                        let async_manager = manager.clone();
                        let async_weak = weak.clone();
                        let async_id = id.clone();
                        let async_loader = loader.clone();
                        async move {
                            // Use a weak reference here to allow the resource to be released if needed
                            if let Some(strong) = async_weak.upgrade() {
                                let _update_lock = strong.update_lock.lock().await;

                                let (resource_mut, drop_checker) = ResourceMut::from_resource(
                                    strong.clone(),
                                );
                                let ctx = ResourceLoadContext::new(
                                    async_manager.clone(),
                                    async_id.clone(),
                                    // TODO: Do Delayed and Update need to be different?
                                    LoadPriority::Delayed,
                                );
                                match async_loader.update(resource_mut, data.data, &ctx).await {
                                    Ok(_) => {
                                        match drop_checker.recv().await {
                                            Ok(_) => { debug!("Drop-checker received an unexpected message"); }
                                            Err(_) => { /* sender was dropped, this is expected */ }
                                        }

                                        strong.update_sub_resources().await;

                                        CacheEntryUpdateResult::Ok(data.source)
                                    },
                                    Err(e) => CacheEntryUpdateResult::Err(e),
                                }
                            } else {
                                CacheEntryUpdateResult::Expired
                            }
                        }
                    })),
                }
            },
            Err(err) => {
                Self::CachedError(err)
            }
        }
    }
}

impl From<ResourceLoadError> for CacheEntryState {
    #[inline]
    fn from(value: ResourceLoadError) -> Self {
        Self::CachedError(value)
    }
}

struct CacheEntry {
    state: Mutex<CacheEntryState>,
    id: ResourceId,
}

impl CacheEntry {
    #[inline]
    fn from_task<T: ?Sized + Send + Sync + 'static>(
        id: ResourceId,
        task: Task<ResourceTaskData<T>>,
        manager: Arc<ResourceManager>,
    ) -> Self {
        Self {
            state: Mutex::new(CacheEntryState::from_task(task, manager)),
            id,
        }
    }

    fn with_state<R>(&self, func: impl FnOnce(CacheEntryState) -> (CacheEntryState, R)) -> R {
        let mut lock = self.state.lock();
        let mut state = CacheEntryState::Uninit;
        std::mem::swap(&mut *lock, &mut state);
        let (mut state, result) = func(state);
        std::mem::swap(&mut *lock, &mut state);
        result
    }

    #[inline]
    fn poll<T: ?Sized + Send + Sync + 'static>(&self) -> Option<CacheEntryGetResult<T>> {
        self.with_state(CacheEntryState::poll)
    }

    #[inline]
    fn wait<T: ?Sized + Send + Sync + 'static>(&self) -> CacheEntryWaitFuture<T> {
        let fut = CacheEntryWaitFuture {
            entry: self,
            waker: Arc::new(AtomicWaker::new()),
            _phantom: PhantomData::default(),
        };
        self.state.lock().insert_waker(&fut.waker);
        fut
    }

    #[inline]
    fn update<'fut>(
        &'fut self,
        manager: &'fut Arc<ResourceManager>,
        source: &'fut Box<dyn ResourceSource>,
        new_idx: &'fut SourceIndex,
        ignore_priority: bool,
    ) -> CacheEntryUpdateFutResult<'fut> {
        self.with_state(|state| state.update(
            manager,
            &self.id,
            source,
            new_idx,
            ignore_priority,
        ))
    }

    #[inline]
    fn finalize_update(&self, data_source: SealedResourceDataSource) {
        self.with_state(|state| state.finalize_update(data_source))
    }

    #[inline]
    fn hash(&self) -> Option<String> {
        self.state.lock().hash()
    }
}

struct CacheEntryWaitFuture<'a, T: ?Sized + Send + Sync + 'static> {
    entry: &'a CacheEntry,
    waker: Arc<AtomicWaker>,
    _phantom: PhantomData<T>,
}

impl<'a, T: ?Sized + Send + Sync + 'static> Future for CacheEntryWaitFuture<'a, T> {
    type Output = CacheEntryGetResult<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.waker.register(cx.waker());
        match self.entry.poll() {
            Some(result) => Poll::Ready(result),
            None => Poll::Pending,
        }
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
        load_priority: LoadPriority,
    ) -> ResourceFuture<T>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let strong_self = self.weak.upgrade()
            .expect("ResourceManager cyclic reference should not be broken");
        let task_self = strong_self.clone();
        let task_id = id.clone();
        let task_loader = loader.clone();
        let task = self.executor.spawn(load_priority.into(), async move {
            #[cfg(test)]
            let _test_ctx_lock = task_self.test_ctx.sync_load.run().await;
            let (result, source_idx) = match task_self.find_data(&task_id).await {
                Some(Ok(data)) => {
                    let ctx = ResourceLoadContext::new(
                        task_self.clone(),
                        task_id.clone(),
                        load_priority,
                    );
                    match task_loader.load(data.data, &ctx).await {
                        Ok(v) => (
                            Ok((Arc::new(Resource::new(task_id.clone(), v)), data.source.hash)),
                            data.source.idx,
                        ),
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
        }.instrument(info_span!("resource_load", id = %id.clone(), %load_priority)));
        let entry = Arc::new(CacheEntry::from_task(id.clone(), task, strong_self.clone()));
        let weak_entry = Arc::downgrade(&entry);
        cache.insert(id.clone(), entry);
        ResourceFuture::from_cache_entry(weak_entry, strong_self, id, loader, load_priority)
    }

    fn get_or_load_impl<T>(
        &self,
        id: ResourceId,
        loader: Arc<dyn ResourceLoader<T>>,
        load_priority: LoadPriority,
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
                Some(CacheEntryGetResult::Expired) => self.load(&mut cache, id, loader, load_priority),
                // Entry is still loading, return a delayed result
                None => ResourceFuture::from_cache_entry(
                    Arc::downgrade(entry),
                    self.weak.upgrade()
                        .expect("ResourceManager cyclic reference should not be broken"),
                    id,
                    loader,
                    load_priority,
                )
            }
        } else {
            // No existing entry, load a new one
            self.load(&mut cache, id, loader, load_priority)
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
        self.get_or_load_impl(id.into(), Arc::new(loader), load_priority)
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
    priority: LoadPriority,
}

impl ResourceLoadContext {
    #[inline]
    pub(in crate::resource) fn new(
        manager: Arc<ResourceManager>,
        id: ResourceId,
        priority: LoadPriority,
    ) -> Self {
        Self {
            manager,
            id,
            priority,
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
        self.manager.get_with_loader_priority(id, loader, self.priority)
    }
}

#[cfg(test)]
pub(in crate::resource) mod tests {
    use super::*;

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
            mut data: ResourceReadData,
            ctx: &ResourceLoadContext,
        ) -> Result<Box<String>, ResourceLoadError> {
            assert_eq!(ctx.id(), self.expected_id);
            let mut output = String::new();
            data.read_to_string(&mut output).await?;
            Ok(Box::new(output))
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
}