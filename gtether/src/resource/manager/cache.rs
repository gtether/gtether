use futures_core::task::__internal::AtomicWaker;
use parking_lot::Mutex;
use smol::Task;
use std::any::{Any, TypeId};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tracing::{debug, warn};

use crate::resource::id::ResourceId;
use crate::resource::manager::executor::{ManagerLoadTask, ResourceTaskData};
use crate::resource::manager::{LoadPriority, ResourceLoadContext, ResourceManager};
use crate::resource::source::{ResourceSource, SealedResourceData, SealedResourceDataSource, SourceIndex};
use crate::resource::{Resource, ResourceLoadError, ResourceMut};
use crate::util::waker::MultiWaker;

pub enum CacheEntryGetResult<T: ?Sized + Send + Sync + 'static> {
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

pub enum CacheEntryUpdateFutResult<'fut> {
    Skip,
    Evict,
    Run(Pin<Box<dyn Future<Output = CacheEntryUpdateResult> + Send + 'fut>>)
}

pub enum CacheEntryUpdateResult {
    Ok(SealedResourceDataSource),
    Expired,
    Err(ResourceLoadError),
}

type CacheEntryStateUpdateFn = Arc<dyn (Fn(&Arc<ResourceManager>, SealedResourceData)
    -> Box<dyn Future<Output = CacheEntryUpdateResult> + Send>) + Send + Sync>;

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

pub struct CacheEntry {
    state: Mutex<CacheEntryState>,
    id: ResourceId,
}

impl CacheEntry {
    #[inline]
    pub fn from_task<T: ?Sized + Send + Sync + 'static>(
        id: ResourceId,
        task: ManagerLoadTask<T>,
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
    pub fn poll<T: ?Sized + Send + Sync + 'static>(&self) -> Option<CacheEntryGetResult<T>> {
        self.with_state(CacheEntryState::poll)
    }

    #[inline]
    pub fn wait<T: ?Sized + Send + Sync + 'static>(&self) -> CacheEntryWaitFuture<T> {
        let fut = CacheEntryWaitFuture {
            entry: self,
            waker: Arc::new(AtomicWaker::new()),
            _phantom: PhantomData::default(),
        };
        self.state.lock().insert_waker(&fut.waker);
        fut
    }

    #[inline]
    pub fn update<'fut>(
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
    pub fn finalize_update(&self, data_source: SealedResourceDataSource) {
        self.with_state(|state| state.finalize_update(data_source))
    }

    #[inline]
    pub fn hash(&self) -> Option<String> {
        self.state.lock().hash()
    }
}

pub struct CacheEntryWaitFuture<'a, T: ?Sized + Send + Sync + 'static> {
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