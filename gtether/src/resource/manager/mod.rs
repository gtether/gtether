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
//! # Loading
//!
//! When retrieving a [Resource] by [ID](ResourceId) and no existing value is already cached for
//! that ID, the given [ResourceLoader] will be used to load and cache a new [Resource]. Loading is
//! handled asynchronously, meaning an async task is enqueued to load the Resource via the internal
//! async executor. In the meantime, a [future](ResourceFuture) will be returned synchronously from
//! the relevant `get_*()` method. This future can be used to synchronously
//! [check](ResourceFuture::check) or asynchronously await the load result.
//!
//! If a [Resource] has already been loaded and cached, it will instead be returned wrapped in a
//! future that will immediately complete when polled.
//!
//! See also:
//!  * [`get()`](ResourceManager::get)
//!  * [`get_with_priority()`](ResourceManager::get_with_priority)
//!  * [`get_with_loader()`](ResourceManager::get_with_loader)
//!  * [`get_with_loader_priority()`](ResourceManager::get_with_loader_priority)
//!
//! ## Note around efficient loading
//!
//! [Resource futures](ResourceFuture) actually represent a load task that is enqueued in the
//! internal [ResourceManager] executor. This means that once the future is created, the work is
//! already concurrently started, even without awaiting the future. Because of this, when loading
//! multiple resources, it is more efficient to create all futures before awaiting any of them.
//!
//! For example, this loads resources sequentially:
//! ```
//! # use async_trait::async_trait;
//! # use smol::future;
//! # use gtether::resource::{ResourceLoadError, ResourceLoader, ResourceReadData};
//! # use gtether::resource::manager::{ResourceLoadContext, ResourceManager};
//! #
//! # #[derive(Default)]
//! # struct MyLoader(());
//! # #[async_trait]
//! # impl ResourceLoader<()> for MyLoader {
//! #     async fn load(&self, data: ResourceReadData, ctx: &ResourceLoadContext) -> Result<Box<()>, ResourceLoadError> {
//! #         Ok(Box::new(()))
//! #     }
//! # }
//! #
//! # let manager = ResourceManager::builder().build();
//! #
//! # future::block_on(async move {
//! let res_a = manager.get_with_loader("a", MyLoader::default()).await;
//! let res_b = manager.get_with_loader("b", MyLoader::default()).await;
//! let res_c = manager.get_with_loader("c", MyLoader::default()).await;
//! # })
//! ```
//!
//! Whereas this loads all three resources concurrently:
//! ```
//! # use async_trait::async_trait;
//! # use smol::future;
//! # use gtether::resource::{ResourceLoadError, ResourceLoader, ResourceReadData};
//! # use gtether::resource::manager::{ResourceLoadContext, ResourceManager};
//! #
//! # #[derive(Default)]
//! # struct MyLoader(());
//! # #[async_trait]
//! # impl ResourceLoader<()> for MyLoader {
//! #     async fn load(&self, data: ResourceReadData, ctx: &ResourceLoadContext) -> Result<Box<()>, ResourceLoadError> {
//! #         Ok(Box::new(()))
//! #     }
//! # }
//! #
//! # let manager = ResourceManager::builder().build();
//! #
//! # future::block_on(async move {
//! let (res_a, res_b, res_c) = {
//!     let fut_a = manager.get_with_loader("a", MyLoader::default());
//!     let fut_b = manager.get_with_loader("b", MyLoader::default());
//!     let fut_c = manager.get_with_loader("c", MyLoader::default());
//!
//!     (fut_a.await, fut_b.await, fut_c.await)
//! };
//! # })
//! ```
//!
//! # Examples
//! Load a [Resource][res]
//! ```
//! use gtether::resource::manager::LoadPriority;
//! # use gtether::resource::manager::ResourceManager;
//! # use gtether::resource::ResourceLoader;
//!
//! # async fn wrapper<T: Send + Sync + 'static>(manager: &ResourceManager, loader1: impl ResourceLoader<T>, loader2: impl ResourceLoader<T>) {
//! // Poll whether the resource is ready
//! let fut = manager.get_with_loader("key1", loader1);
//! match fut.check() {
//!     Ok(result) => { /* do something with the load result */ },
//!     Err(fut) => { /* do something with the unready handle */ },
//! }
//!
//! // Wait for the resource to be ready
//! let result = manager.get_with_loader("key2", loader2).await;
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
//! [rf]: ResourceFutureOld
use ahash::{HashMap, HashSet};
use educe::Educe;
use parking_lot::{RwLock, RwLockReadGuard};
use smol::prelude::*;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crate::resource::id::ResourceId;
use crate::resource::manager::dependency::DependencyGraph;
use crate::resource::manager::executor::ManagerExecutor;
use crate::resource::manager::load::{Cache, ResourceLoadFuture, ResourceLoadOperation, ResourceLoadParams};
use crate::resource::manager::source::Sources;
use crate::resource::manager::update::{ResourceUpdateFuture, UpdateManager};
use crate::resource::source::ResourceSource;
use crate::resource::{ResourceDefaultLoader, ResourceLoadError, ResourceLoadResult, ResourceLoader};

pub mod dependency;
mod executor;
mod load;
mod source;
mod update;

pub use executor::ManagerTask;
pub use load::{
    LoadPriority,
    ResourceLoadContext,
};

enum ResourceFutureInner<T: ?Sized + Send + Sync + 'static> {
    Cached(ResourceLoadResult<T>),
    Loading(ResourceLoadFuture<T>),
    Updating(ResourceUpdateFuture<T>),
    Or {
        base: Box<ResourceFutureInner<T>>,
        id: ResourceId,
        loader: Arc<dyn ResourceLoader<T>>,
    },
}

impl<T: ?Sized + Send + Sync + 'static> Debug for ResourceFutureInner<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cached(Ok(res)) => write!(f, "Cached(Ok({}))", res.id()),
            Self::Cached(Err(e)) => {
                write!(f, "Cached(Err(")?;
                Debug::fmt(e, f)?;
                write!(f, "))")
            },
            Self::Loading(_) => write!(f, "Loading(<load-future>)"),
            Self::Updating(_) => write!(f, "Updating(<update-future>)"),
            Self::Or { base, id, .. } => {
                Debug::fmt(base, f)?;
                write!(f, " | {}", id)
            }
        }
    }
}

impl<T: ?Sized + Send + Sync + 'static> ResourceFutureInner<T> {
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        cache: Arc<Cache>,
        load_params: Arc<ResourceLoadParams<T>>,
    ) -> Result<Poll<ResourceLoadResult<T>>, Self> {
        match self {
            Self::Cached(result) => Ok(Poll::Ready(result.clone())),
            Self::Loading(load_fut) => Ok(load_fut.poll(cx)),
            Self::Updating(update_fut) => Ok(update_fut.poll(cx)),
            Self::Or { base, id, loader } => {
                let poll_result = match base.poll(cx, cache.clone(), load_params.clone()) {
                    Ok(poll_result) => poll_result,
                    Err(new_inner) => {
                        *base = Box::new(new_inner);
                        return self.poll(cx, cache, load_params)
                    }
                };
                match poll_result {
                    Poll::Ready(result) => {
                        match result {
                            Err(ResourceLoadError::NotFound(_)) => {
                                let mut load_params = (*load_params).clone();
                                load_params.loader = loader.clone();
                                Err(cache.get_or_load(id.clone(), load_params))
                            },
                            result => Ok(Poll::Ready(result)),
                        }
                    },
                    poll_result => Ok(poll_result),
                }
            }
        }
    }
}

/// Future for retrieving results when loading a [Resource][res].
///
/// Two (or more) futures can be created that refer to the same load task / cached [Resource][res],
/// simply by asking the [ResourceManager][rm] for the same [id][rp] multiple times. In this case,
/// if one future resolves to a [Resource][res], and then drops that [Resource][res], the
/// [Resource][res] may be dropped in the [ResourceManager's][rm] cache. Any other futures that then
/// try to resolve will trigger a reload of said [Resource][res], using the [ResourceLoader][rl]
/// provided when that future was generated.
///
/// [res]: Resource
/// [id]: ResourceId
/// [rl]: ResourceLoader
/// [rm]: ResourceManager
#[derive(Educe)]
#[educe(Debug)]
pub struct ResourceFuture<'ctx, T: ?Sized + Send + Sync + 'static> {
    inner: ResourceFutureInner<T>,
    #[educe(Debug(ignore))]
    cache: Arc<Cache>,
    // Needs to be Arc<> due to some weird Pin semantics + borrow splitting in Future::poll()
    load_params: Arc<ResourceLoadParams<T>>,
    #[educe(Debug(ignore))]
    ctx: Option<&'ctx ResourceLoadContext>,
}

impl<T: ?Sized + Send + Sync + 'static> ResourceFuture<'static, T> {
    #[inline]
    fn new(
        inner: impl Into<ResourceFutureInner<T>>,
        cache: Arc<Cache>,
        load_params: ResourceLoadParams<T>,
    ) -> Self {
        Self {
            inner: inner.into(),
            cache,
            load_params: Arc::new(load_params),
            ctx: None,
        }
    }
}

impl<'ctx, T: ?Sized + Send + Sync + 'static> ResourceFuture<'ctx, T> {
    #[inline]
    fn with_context(
        inner: impl Into<ResourceFutureInner<T>>,
        cache: Arc<Cache>,
        load_params: ResourceLoadParams<T>,
        ctx: &'ctx ResourceLoadContext,
    ) -> Self {
        Self {
            inner: inner.into(),
            cache,
            load_params: Arc::new(load_params),
            ctx: Some(ctx),
        }
    }

    /// Synchronously check if the future is complete by polling once.
    ///
    /// If the future is not complete, yields itself as the `Err()`.
    #[inline]
    pub fn check(mut self) -> Result<ResourceLoadResult<T>, Self> {
        let mut cx = Context::from_waker(Waker::noop());
        match self.poll(&mut cx) {
            Poll::Ready(result) => Ok(result),
            Poll::Pending => Err(self),
        }
    }

    /// Compose this ResourceFuture with a fallback load configuration using a default
    /// [ResourceLoader].
    ///
    /// Creates and uses the [ResourceLoader] specified by [ResourceDefaultLoader].
    ///
    /// If this ResourceFuture would resolve to a [ResourceLoadError::NotFound], instead it attempts
    /// to load this fallback configuration instead.
    #[inline]
    pub fn or_get(
        self,
        id: impl Into<ResourceId>,
    ) -> Self
    where
        T: ResourceDefaultLoader,
    {
        self.or_get_with_loader(id, T::default_loader())
    }

    /// Compose this ResourceFuture with a fallback load configuration.
    ///
    /// If this ResourceFuture would resolve to a [ResourceLoadError::NotFound], instead it attempts
    /// to load this fallback configuration instead.
    #[inline]
    pub fn or_get_with_loader(
        mut self,
        id: impl Into<ResourceId>,
        loader: impl ResourceLoader<T>,
    ) -> Self {
        self.inner = ResourceFutureInner::Or {
            base: Box::new(self.inner),
            id: id.into(),
            loader: Arc::new(loader),
        };
        self
    }
}

impl<'ctx, T: ?Sized + Send + Sync + 'static> Future for ResourceFuture<'ctx, T> {
    type Output = ResourceLoadResult<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let cache = self.cache.clone();
        let load_params = self.load_params.clone();
        let poll_result = match self.inner.poll(cx, cache, load_params) {
            Ok(poll_result) => poll_result,
            Err(new_inner) => {
                self.inner = new_inner;
                return self.poll(cx)
            }
        };

        if let Poll::Ready(result) = &poll_result {
            if let Ok(resource) = &result {
                if let Some(ctx) = self.ctx {
                    ctx.add_dependency(resource.id().clone());
                    ctx.cache_resource(resource.clone());
                }
            }
        }

        poll_result
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
    #[allow(unused)] // Used in e.g. tests
    executor: Arc<ManagerExecutor>,
    cache: Arc<Cache>,
    dependencies: Arc<RwLock<DependencyGraph>>,
    update_manager: UpdateManager,
    #[cfg(test)]
    test_ctx: tests::ResourceManagerTestContext,
}

impl ResourceManager {
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
    /// behaviour for either the ResourceManager itself or user-defined [sources][src].
    ///
    /// [tc]: tests::ResourceManagerTestContext
    /// [src]: ResourceSource
    #[cfg(test)]
    #[inline]
    pub fn test_ctx(&self) -> &tests::ResourceManagerTestContext { &self.test_ctx }

    /// Graph of resource dependency relations in this ResourceManager.
    ///
    /// See the [dependency module](dependency) for more.
    #[inline]
    pub fn dependencies(&self) -> RwLockReadGuard<DependencyGraph> {
        self.dependencies.read()
    }

    /// Get/load a [Resource] with a default [ResourceLoader] and [LoadPriority].
    ///
    /// Creates and uses the [ResourceLoader] specified by [ResourceDefaultLoader].
    ///
    /// Uses [LoadPriority::Immediate].
    ///
    /// See also: [Self#loading]
    #[inline]
    pub fn get<T>(&self, id: impl Into<ResourceId>) -> ResourceFuture<'static, T>
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
    ) -> ResourceFuture<'static, T>
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
    ) -> ResourceFuture<'static, T>
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
    ) -> ResourceFuture<'static, T>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let load_params = ResourceLoadParams {
            loader: Arc::new(loader),
            priority: load_priority.into(),
            parents: vec![],
            resource_cache: Arc::new(RwLock::new(HashMap::default())),
            operation: ResourceLoadOperation::Load,
        };

        let inner = self.cache.get_or_load(
            id.into(),
            load_params.clone(),
        );
        ResourceFuture::new(inner, self.cache.clone(), load_params)
    }
}

impl Drop for ResourceManager {
    fn drop(&mut self) {
        // Stop any active updates first, so they can't queue any more load tasks
        self.update_manager.stop();
        // This will drop and cancel any active load tasks + their references, allowing subsystems
        // to properly drop and shutdown without waiting for all active tasks to complete.
        self.cache.clear();
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
    pub fn source(mut self, source: impl ResourceSource) -> Self {
        self.sources.push(Box::new(source));
        self
    }

    #[inline]
    pub fn build(self) -> Arc<ResourceManager> {
        let executor = Arc::new(ManagerExecutor::new());
        let mut update_manager = UpdateManager::new();
        let mut sources = self.sources;
        for (idx, source) in sources.iter_mut().enumerate() {
            source.set_updater(update_manager.create_watcher(idx.into()), HashSet::default());
        }
        let sources = Arc::new(Sources::new(sources));
        let dependencies = Arc::new(RwLock::new(DependencyGraph::default()));
        #[cfg(test)]
        let test_ctx = tests::ResourceManagerTestContext::default();
        let cache = Cache::new(
            executor.clone(),
            sources.clone(),
            dependencies.clone(),
            update_manager.updates().clone(),
            #[cfg(test)]
            test_ctx.sync_load.clone(),
        );

        update_manager.start(
            sources.clone(),
            cache.clone(),
            dependencies.clone(),
            #[cfg(test)]
            test_ctx.sync_update.clone(),
        );

        Arc::new(ResourceManager {
            executor,
            cache,
            dependencies,
            update_manager,
            #[cfg(test)]
            test_ctx,
        })
    }
}

/// Test-specific logic.
///
/// This module is public in order to provide common utilities that can be used to test resource
/// loading logic, such as custom implemented [sources](ResourceSource).
#[cfg(test)]
pub mod tests {
    use super::*;

    use ahash::{HashMap, HashSet};
    use async_trait::async_trait;
    use itertools::Itertools;
    use macro_rules_attribute::apply;
    use parking_lot::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
    use rstest::{fixture, rstest};
    use smol::Timer;
    use smol_macros::test as smol_test;
    use std::marker::PhantomData;
    use std::time::Duration;
    use test_log::test as test_log;

    use crate::resource::source::{ResourceData, ResourceDataResult, ResourceDataSource, ResourceSource, ResourceUpdate, ResourceUpdater, SourceIndex};
    use crate::resource::{Resource, ResourceReadData};

    pub(in crate::resource) async fn timeout<T>(fut: impl Future<Output = T>, time: Duration) -> T {
        let timeout_fn = async move {
            Timer::after(time).await;
            panic!("Timeout reached: {time:?}");
        };
        fut.or(timeout_fn).await
    }

    pub(in crate::resource) async fn assert_manager_drops(manager: Arc<ResourceManager>) {
        let weak_dependencies = Arc::downgrade(&manager.dependencies);
        let weak_cache = Arc::downgrade(&manager.cache);
        let weak_executor = Arc::downgrade(&manager.executor);
        let executor = manager.executor.clone();

        drop(manager);

        while !executor.is_empty() {
            Timer::after(Duration::from_millis(10)).await;
        }
        drop(executor);

        assert_eq!(weak_dependencies.strong_count(), 0);
        assert_eq!(weak_cache.strong_count(), 0);
        assert_eq!(weak_executor.strong_count(), 0);
    }

    pub struct SyncContextReadGuard<'a>{
        #[allow(unused)]
        inner: smol::lock::RwLockReadGuard<'a, ()>,
        count: &'a Mutex<(usize, usize)>,
    }

    impl Drop for SyncContextReadGuard<'_> {
        fn drop(&mut self) {
            let mut count = self.count.lock();
            count.0 += 1;
            count.1 += 1;
        }
    }

    pub struct SyncContextWriteGuard<'a>(
        #[allow(unused)]
        smol::lock::RwLockWriteGuard<'a, ()>
    );

    /// Context used to synchronize access to a particular code segment.
    ///
    /// This is useful for e.g. blocking a load task until a test reaches a certain point.
    ///
    /// Keeps track of both 'attempts' and 'counts'. An 'attempt' is where a code block would
    /// execute, but otherwise bails (e.g. the resource doesn't need to be updated because hashes
    /// are the same). A 'count' is where a code block actually executes. Each 'count' also
    /// generates an 'attempt'.
    pub struct SyncContext {
        inner: smol::lock::RwLock<()>,
        count: Mutex<(usize, usize)>,
    }

    impl Default for SyncContext {
        fn default() -> Self {
            Self {
                inner: smol::lock::RwLock::new(()),
                count: Mutex::new((0, 0)),
            }
        }
    }

    impl SyncContext {
        /// Marks where the code segment is intended to run.
        ///
        /// The returned guard should be held for the duration of the code segment. When the guard
        /// is dropped, both 'count' and 'attempts' will be incremented.
        ///
        /// ```
        /// use std::sync::Arc;
        /// use gtether::resource::manager::tests::SyncContext;
        ///
        /// let sync_context = Arc::new(SyncContext::default());
        /// let async_sync_ctx = sync_context.clone();
        /// let fut = async move {
        ///     let _guard = async_sync_ctx.run().await;
        ///
        ///     // execute relevant code...
        /// };
        /// ```
        pub async fn run(&self) -> SyncContextReadGuard<'_> {
            SyncContextReadGuard {
                inner: self.inner.read().await,
                count: &self.count,
            }
        }

        /// Block a sync context until a test is ready for it to execute.
        ///
        /// ```
        /// use std::sync::Arc;
        /// use gtether::resource::manager::tests::SyncContext;
        ///
        /// let sync_context = Arc::new(SyncContext::default());
        /// // Give a copy of `sync_context` to logic that needs to be tested...
        ///
        /// // In the test:
        /// async {
        ///     let _guard = sync_context.block().await;
        ///     // Do some prep work
        /// }
        ///
        /// // After awaiting the above, check your assertions / etc
        /// ```
        #[inline]
        pub async fn block(&self) -> SyncContextWriteGuard<'_> {
            SyncContextWriteGuard(self.inner.write().await)
        }

        /// Mark that an 'attempt' has been tried.
        #[inline]
        pub fn mark_attempt(&self) {
            self.count.lock().1 += 1;
        }

        /// Assert that 'count' equals a certain amount.
        #[inline]
        pub fn assert_count(&self, count: usize) {
            let actual = self.count.lock().0;
            assert_eq!(self.count.lock().0, count, "Expected {count} executions; was actually {actual}");
        }

        /// Assert that 'attempts' equals a certain amount.
        #[inline]
        pub fn assert_attempts(&self, attempts: usize) {
            assert_eq!(self.count.lock().1, attempts);
        }

        /// Wait for 'count' to reach at least the given amount.
        pub async fn wait_count(&self, count: usize) {
            while self.count.lock().0 < count {
                Timer::after(Duration::from_millis(50)).await;
            }
        }

        /// Wait for 'attempts' to reach at least the given amount.
        pub async fn wait_attempts(&self, attempts: usize) {
            while self.count.lock().1 < attempts {
                Timer::after(Duration::from_millis(50)).await;
            }
        }

        /// Clear both 'count' and 'attempts'.
        #[inline]
        pub fn clear(&self) {
            *self.count.lock() = (0, 0);
        }
    }

    /// Test context for the [ResourceManager].
    ///
    /// Bundles both a 'load' and 'update' [SyncContext].
    #[derive(Default)]
    pub struct ResourceManagerTestContext {
        pub sync_load: Arc<SyncContext>,
        pub sync_update: Arc<SyncContext>,
    }

    impl ResourceManagerTestContext {
        /// Clear all [SyncContexts](SyncContext).
        pub fn clear(&self) {
            self.sync_load.clear();
            self.sync_update.clear();
        }
    }

    #[derive(Default, Clone)]
    pub(in crate::resource) struct StringLoader(());

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
    pub(in crate::resource) struct TestResLoader {
        expected_id: ResourceId,
    }

    impl TestResLoader {
        #[inline]
        pub fn new(expected_id: impl Into<ResourceId>) -> Self {
            Self {
                expected_id: expected_id.into(),
            }
        }
    }

    #[async_trait]
    impl ResourceLoader<String> for TestResLoader {
        async fn load(
            &self,
            data: ResourceReadData,
            ctx: &ResourceLoadContext,
        ) -> Result<Box<String>, ResourceLoadError> {
            assert_eq!(ctx.id(), self.expected_id);
            StringLoader::default().load(data, ctx).await
        }
    }

    #[derive(Debug)]
    pub(in crate::resource) struct TestResRef<T: Send + Sync + 'static> {
        pub resources: Vec<Arc<Resource<T>>>,
    }

    #[derive(Clone)]
    pub(in crate::resource) struct TestRefLoader<T, L>
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T>,
    {
        loader: L,
        _phantom: PhantomData<T>,
    }

    impl<T, L> TestRefLoader<T, L>
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
    impl<T, L> ResourceLoader<TestResRef<T>> for TestRefLoader<T, L>
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T> + Clone,
    {
        async fn load(
            &self,
            mut data: ResourceReadData,
            ctx: &ResourceLoadContext,
        ) -> Result<Box<TestResRef<T>>, ResourceLoadError> {
            let mut ids = String::new();
            data.read_to_string(&mut ids).await?;

            let futs = ids.split(':')
                .map(|id| ctx.get_with_loader(id, self.loader.clone()))
                .collect::<Vec<_>>();
            let mut resources = Vec::with_capacity(futs.len());
            for fut in futs {
                resources.push(fut.await?);
            }

            Ok(Box::new(TestResRef {
                resources,
            }))
        }
    }

    #[derive(Default, Clone)]
    pub(in crate::resource) struct TestResChainLoader(());

    #[async_trait]
    impl ResourceLoader<Vec<String>> for TestResChainLoader {
        async fn load(
            &self,
            mut data: ResourceReadData,
            ctx: &ResourceLoadContext,
        ) -> Result<Box<Vec<String>>, ResourceLoadError> {
            let mut values_str = String::new();
            data.read_to_string(&mut values_str).await?;

            let mut values = Vec::new();
            for value in values_str.split(':') {
                match ctx.get_with_loader(value, Self::default()).await {
                    Ok(resource) => {
                        for sub_value in &*resource.read() {
                            values.push(ctx.id().to_string() + ":" + sub_value);
                        }
                    },
                    Err(ResourceLoadError::NotFound(_)) => {
                        values.push(ctx.id().to_string() + ":" + value);
                    },
                    Err(error) => return Err(error),
                }
            }

            Ok(Box::new(values))
        }
    }

    #[derive(Debug)]
    pub(in crate::resource) struct ExpectedResourceRef<A> {
        pub expected_resources: Vec<A>,
    }

    impl<A> ExpectedResourceRef<A> {
        #[inline]
        pub fn new(expected_resources: impl IntoIterator<Item=A>) -> Self {
            Self {
                expected_resources: expected_resources.into_iter().collect(),
            }
        }
    }

    impl<T: Send + Sync + 'static, A: PartialEq<T>> PartialEq<TestResRef<T>> for ExpectedResourceRef<A> {
        fn eq(&self, other: &TestResRef<T>) -> bool {
            if other.resources.len() != self.expected_resources.len() {
                return false
            }
            for (idx, res) in other.resources.iter().enumerate() {
                let expected = &self.expected_resources[idx];
                let lock = res.read();
                if expected != &*lock {
                    return false
                }
            }
            true
        }
    }

    /// Underlying raw data for [TestResourceSource].
    pub struct ResourceDataMap {
        raw: RwLock<HashMap<ResourceId, (Vec<u8>, String)>>,
        updater: RwLock<Option<Box<dyn ResourceUpdater>>>,
        watch_list: RwLock<HashSet<ResourceId>>,
    }

    impl ResourceDataMap {
        fn new() -> Self {
            Self {
                raw: Default::default(),
                updater: Default::default(),
                watch_list: Default::default(),
            }
        }

        fn get(&self, id: &ResourceId) -> Option<(Vec<u8>, String)> {
            self.raw.read().get(id).map(|(r, h)| (r.clone(), h.clone()))
        }

        /// Lock this data map to allow mutations.
        ///
        /// When the lock is dropped, a
        /// [BulkResourceUpdate](crate::resource::source::BulkResourceUpdate) will be generated with
        /// any changes that were made.
        pub fn lock(&self) -> ResourceDataMapLock<'_> {
            let raw = self.raw.write();
            let watch_list = self.watch_list.read();
            ResourceDataMapLock {
                raw,
                watch_list,
                updater: &self.updater,
                updates: HashMap::default(),
            }
        }

        fn watch(&self, id: ResourceId) {
            self.watch_list.write().insert(id);
        }

        fn unwatch(&self, id: &ResourceId) {
            self.watch_list.write().remove(id);
        }

        /// Assert that a given [ResourceId] is being watched or not.
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

    /// Mutable lock on a [ResourceDataMap].
    pub struct ResourceDataMapLock<'a> {
        raw: RwLockWriteGuard<'a, HashMap<ResourceId, (Vec<u8>, String)>>,
        watch_list: RwLockReadGuard<'a, HashSet<ResourceId>>,
        updater: &'a RwLock<Option<Box<dyn ResourceUpdater>>>,
        updates: HashMap<ResourceId, ResourceUpdate>,
    }

    impl<'a> ResourceDataMapLock<'a> {
        /// Insert the given raw data entry.
        pub fn insert(
            &mut self,
            id: impl Into<ResourceId>,
            data: &[u8],
            hash: impl Into<String>,
        ) {
            let id = id.into();
            let hash = hash.into();
            let update = match self.raw.insert(id.clone(), (Vec::from(data), hash.clone())).is_some() {
                true => ResourceUpdate::Modified(hash),
                false => ResourceUpdate::Added(hash),
            };
            if self.watch_list.contains(&id) {
                self.updates.insert(id, update);
            }
        }

        /// Remove the entry specified by the given `id`, if it exists.
        pub fn remove(&mut self, id: impl Into<ResourceId>) {
            let id = id.into();
            if self.raw.remove(&id).is_some() {
                if self.watch_list.contains(&id) {
                    self.updates.insert(id, ResourceUpdate::Removed);
                }
            }
        }
    }

    impl<'a> Drop for ResourceDataMapLock<'a> {
        fn drop(&mut self) {
            let mut updates = HashMap::default();
            std::mem::swap(&mut updates, &mut self.updates);
            let updater = self.updater.read();
            if let Some(updater) = &*updater {
                updater.notify_update(updates.into());
            }
        }
    }

    /// Simple [ResourceSource] suitable for use in testcases.
    pub struct TestResourceSource {
        inner: Arc<ResourceDataMap>,
    }

    impl TestResourceSource {
        /// Create a new TestResourceSource and the accompanying [ResourceDataMap].
        #[inline]
        pub fn new() -> (Self, Arc<ResourceDataMap>) {
            let data_map = Arc::new(ResourceDataMap::new());
            let source = Self {
                inner: data_map.clone()
            };
            (source, data_map)
        }
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
                Ok(ResourceData::new(Box::new(smol::io::Cursor::new(data)), hash))
            } else {
                Err(ResourceLoadError::NotFound(id.clone()))
            }
        }

        fn set_updater(&self, new_updater: Box<dyn ResourceUpdater>, new_watches: HashSet<ResourceId>) {
            let mut watches = self.inner.watch_list.write();
            let mut updater = self.inner.updater.write();
            let data = self.inner.raw.read();

            if let Some(updater) = &*updater {
                let update = watches.drain()
                    .map(|id| (id, ResourceUpdate::Removed))
                    .collect::<HashMap<_, _>>()
                    .into();
                updater.notify_update(update);
            }

            *watches = new_watches;
            let mut update = HashMap::default();
            for watch in &*watches {
                if let Some((_, hash)) = data.get(watch) {
                    update.insert(watch.clone(), ResourceUpdate::Added(hash.clone()));
                }
            }
            new_updater.notify_update(update.into());
            *updater = Some(new_updater);
        }

        fn watch(&self, id: ResourceId, _sub_idx: Option<SourceIndex>) {
            self.inner.watch(id);
        }

        fn unwatch(&self, id: &ResourceId) {
            self.inner.unwatch(id);
        }
    }

    /// Commonly used fixture to create a [ResourceManager] and any accompanying
    /// [ResourceDataMaps](ResourceDataMap).
    #[fixture]
    pub fn test_resource_manager<const N: usize>() -> (Arc<ResourceManager>, [Arc<ResourceDataMap>; N]) {
        let mut builder = ResourceManager::builder();
        let tuples = core::array::from_fn::<_, N, _>(|_| TestResourceSource::new());
        let data_maps = core::array::from_fn::<_, N, _>(|idx| {
            tuples[idx].1.clone()
        });
        for tuple in tuples {
            builder = builder.source(tuple.0);
        }
        let manager = builder.build();

        (manager, data_maps)
    }

    #[derive(Clone)]
    pub(in crate::resource) struct TestDataEntry {
        pub id: ResourceId,
        pub data: &'static [u8],
        pub hash: String,
    }

    impl TestDataEntry {
        #[inline]
        pub fn new(
            id: impl Into<ResourceId>,
            data: &'static [u8],
            hash: impl Into<String>,
        ) -> Self {
            Self {
                id: id.into(),
                data,
                hash: hash.into(),
            }
        }
    }

    pub(in crate::resource) struct TestLoadInfo<T: ?Sized + Send + Sync + 'static, L: ResourceLoader<T>> {
        pub key: ResourceId,
        pub loader: L,
        _phantom: PhantomData<T>,
    }

    impl<T: ?Sized + Send + Sync + 'static, L: ResourceLoader<T>> TestLoadInfo<T, L> {
        #[inline]
        pub fn new(key: impl Into<ResourceId>, loader: L) -> Self {
            Self {
                key: key.into(),
                loader,
                _phantom: PhantomData::default(),
            }
        }
    }

    pub(in crate::resource) enum ExpectedLoadResult<T> {
        Ok(T),
        NotFound(ResourceId),
        ReadErr,
    }

    impl<T> ExpectedLoadResult<T> {
        pub fn assert_matches<V>(&self, result: ResourceLoadResult<V>) -> Option<Arc<Resource<V>>>
        where
            T: Debug + PartialEq<V>,
            V: Debug + Send + Sync + 'static,
        {
            match self {
                Self::Ok(expected_value) => {
                    let value = result
                        .expect("Resource should load");
                    assert_eq!(expected_value, &*value.read());
                    Some(value)
                },
                Self::NotFound(expected_id) => {
                    let error = result
                        .expect_err("Resource should fail to load");
                    assert_matches!(error, ResourceLoadError::NotFound(id) => {
                        assert_eq!(&id, expected_id);
                    });
                    None
                }
                Self::ReadErr => {
                    let error = result
                        .expect_err("Resource should fail to load");
                    assert_matches!(error, ResourceLoadError::ReadError(_));
                    None
                }
            }
        }
    }

    pub(in crate::resource) struct ExpectedDataEntry<T> {
        pub entry: TestDataEntry,
        pub expected: ExpectedLoadResult<T>,
    }

    impl ExpectedDataEntry<String> {
        #[inline]
        pub fn ok(id: impl Into<ResourceId>) -> Self {
            Self {
                entry: TestDataEntry::new(id, b"value", "h_value"),
                expected: ExpectedLoadResult::Ok("value".to_string()),
            }
        }

        #[inline]
        pub fn ok_new(id: impl Into<ResourceId>) -> Self {
            Self {
                entry: TestDataEntry::new(id, b"new_value", "h_new_value"),
                expected: ExpectedLoadResult::Ok("new_value".to_string()),
            }
        }

        #[inline]
        pub fn read_error(id: impl Into<ResourceId>) -> Self {
            Self {
                entry: TestDataEntry::new(id, b"\xC0", "h_invalid"),
                expected: ExpectedLoadResult::ReadErr,
            }
        }

        #[inline]
        pub fn read_error_new(id: impl Into<ResourceId>) -> Self {
            Self {
                entry: TestDataEntry::new(id, b"new_\xC0", "h_new_invalid"),
                expected: ExpectedLoadResult::ReadErr,
            }
        }
    }

    pub(in crate::resource) struct ExpectedLoadInfo<T, L, A>
    where
        T: ?Sized + Send + Sync + 'static,
        L: ResourceLoader<T>,
        A: PartialEq<T>,
    {
        pub load_info: TestLoadInfo<T, L>,
        pub expected: ExpectedLoadResult<A>,
    }

    pub(in crate::resource) fn setup_dependency_data(
        data_map: &Arc<ResourceDataMap>,
        dependency_graph: &DependencyGraph,
    ) {
        let mut data_map = data_map.lock();

        for node in dependency_graph.iter() {
            let dep_str = node.dependencies()
                .map(|n| n.id())
                .sorted()
                .join(":");
            data_map.insert(node.id().clone(), dep_str.as_bytes(), format!("h_{}", node.id()));
        }
    }

    #[derive(Clone)]
    pub(in crate::resource) enum Update {
        Insert(TestDataEntry),
        Remove(ResourceId),
    }

    impl Update {
        pub fn apply(self, data_map: &mut ResourceDataMapLock<'_>) {
            match self {
                Self::Insert(entry) =>
                    data_map.insert(entry.id, entry.data, entry.hash),
                Self::Remove(id) =>
                    data_map.remove(id),
            }
        }
    }

    pub(in crate::resource) struct BulkUpdate {
        pub idx: usize,
        pub updates: Vec<Update>,
    }

    impl BulkUpdate {
        pub fn new(idx: usize, updates: impl IntoIterator<Item=Update>) -> Self {
            Self {
                idx,
                updates: updates.into_iter().collect(),
            }
        }

        pub fn apply(self, data_maps: &[Arc<ResourceDataMap>]) {
            let mut data_map = data_maps[self.idx].lock();
            for update in self.updates {
                update.apply(&mut data_map);
            }
        }
    }

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_manager_can_drop(
        #[from(test_resource_manager)] (manager, _): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
    ) {
        timeout(
            assert_manager_drops(manager),
            Duration::from_secs(5),
        ).await;
    }

    #[rstest]
    #[case::first_ok(
        [
            TestDataEntry::new("a", b"value1", "h_a"),
            TestDataEntry::new("b", b"value2", "h_b"),
            TestDataEntry::new("c", b"value3", "h_c"),
        ],
        [
            TestLoadInfo::new("a", TestResLoader::new("a")),
            TestLoadInfo::new("b", TestResLoader::new("b")),
            TestLoadInfo::new("c", TestResLoader::new("c")),
        ],
        ExpectedLoadResult::Ok("value1".to_string()),
    )]
    #[case::first_not_found(
        [
            TestDataEntry::new("b", b"value2", "h_b"),
            TestDataEntry::new("c", b"value3", "h_c"),
        ],
        [
            TestLoadInfo::new("a", TestResLoader::new("a")),
            TestLoadInfo::new("b", TestResLoader::new("b")),
            TestLoadInfo::new("c", TestResLoader::new("c")),
        ],
        ExpectedLoadResult::Ok("value2".to_string()),
    )]
    #[case::nested_not_found(
        [
            TestDataEntry::new("c", b"value3", "h_c"),
        ],
        [
            TestLoadInfo::new("a", TestResLoader::new("a")),
            TestLoadInfo::new("b", TestResLoader::new("b")),
            TestLoadInfo::new("c", TestResLoader::new("c")),
        ],
        ExpectedLoadResult::Ok("value3".to_string()),
    )]
    #[case::none_found(
        [],
        [
            TestLoadInfo::new("a", TestResLoader::new("a")),
            TestLoadInfo::new("b", TestResLoader::new("b")),
            TestLoadInfo::new("c", TestResLoader::new("c")),
        ],
        ExpectedLoadResult::NotFound(ResourceId::from("c")),
    )]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_future_or_get(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
        #[case] initial_data: impl IntoIterator<Item=TestDataEntry>,
        #[case] load_info: impl IntoIterator<Item=TestLoadInfo<String, TestResLoader>>,
        #[case] expected_load_result: ExpectedLoadResult<String>,
    ) {
        {
            let mut lock = data_maps[0].lock();
            for data in initial_data {
                lock.insert(data.id, data.data, data.hash);
            }
        }

        let mut load_info = load_info.into_iter();
        let mut fut = {
            let first = load_info.next().expect("'load_info' should have at least 1 item");
            manager.get_with_loader(first.key, first.loader)
        };
        for next in load_info {
            fut = fut.or_get_with_loader(next.key, next.loader);
        }

        let result = fut.await;
        expected_load_result.assert_matches(result);
    }

    pub mod prelude {
        pub use super::{
            test_resource_manager,
            ResourceDataMap,
            ResourceDataMapLock,
        };

        pub(in crate::resource) use super::{
            assert_manager_drops,
            setup_dependency_data,
            timeout,
            BulkUpdate,
            ExpectedDataEntry,
            ExpectedLoadInfo,
            ExpectedLoadResult,
            ExpectedResourceRef,
            StringLoader,
            TestDataEntry,
            TestLoadInfo,
            TestRefLoader,
            TestResChainLoader,
            TestResLoader,
            Update,
        };
    }
}