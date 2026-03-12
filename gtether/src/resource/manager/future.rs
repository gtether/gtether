//! Futures and their adapters for resource loading.
//!
//! The primary type provided this module is [ResourceFuture], which is yielded by
//! [ResourceManager](super::ResourceManager) [`get*()`](super::ResourceManager::get) methods, and
//! can be awaited to retrieve [Resource](crate::resource::Resource) values.
//!
//! In addition, ResourceFuture provides a [`check()`](ResourceFuture::check) method that can be
//! used to synchronously check if the resource has finished loading. See that method for more
//! details.
//!
//! # Adapters
//!
//! Resource futures can be composed with adapters to modify their functionality, similar to `std`
//! iterators. Unlike `std` iterators, however, resource future adapters are internally implemented,
//! and not meant to be extended by users, meaning there is a limited amount of valid adapters.
//!
//! Adapters are specified using generics on [ResourceFuture] itself. This can make it hard to e.g.
//! store collections of resource futures with different adapters. To handle this,
//! [`ResourceFuture::to_boxed()`] can be used to acquire a resource future with a dyn typed
//! adapter.

use educe::Educe;
use futures_core::future::BoxFuture;
use futures_core::FusedFuture;
use pin_project::pin_project;
use smol::future::FutureExt;
use std::fmt::{Debug, Formatter};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll, Waker};

use crate::resource::id::ResourceId;
use crate::resource::manager::load::{Cache, ResourceLoadParams};
use crate::resource::manager::{ResourceLoadContext, ResourceLoadFuture, ResourceUpdateFuture};
use crate::resource::{ResourceLoadError, ResourceLoadResult, ResourceLoader, ResourceLoaderDefault};

pub struct ResourceFutureMetadata {
    cache: Arc<Cache>,
}

pub type ResourceFutureBoxed<'ctx, BaseLoader> = ResourceFuture<'ctx, Box<dyn ResourceFutureAdapter<BaseLoader=BaseLoader> + Unpin>>;

/// Future for retrieving results when loading a [Resource][res].
///
/// Two (or more) futures can be created that refer to the same load task / cached [Resource][res],
/// simply by asking the [ResourceManager][rm] for the same [id][rp] multiple times. In this case,
/// if one future resolves to a [Resource][res], and then drops that [Resource][res], the
/// [Resource][res] may be dropped in the [ResourceManager's][rm] cache. Any other futures that then
/// try to resolve will trigger a reload of said [Resource][res], using the [ResourceLoader][rl]
/// provided when that future was generated.
///
/// [res]: crate::resource::Resource
/// [id]: crate::resource::ResourceId
/// [rl]: crate::resource::ResourceLoader
/// [rm]: crate::resource::manager::ResourceManager
#[derive(Educe)]
#[educe(Debug)]
#[pin_project]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ResourceFuture<'ctx, A: ResourceFutureAdapter> {
    #[pin] adapter: A,
    primary_id: ResourceId,
    #[educe(Debug(ignore))]
    metadata: ResourceFutureMetadata,
    load_params: ResourceLoadParams<A::BaseLoader>,
    #[educe(Debug(ignore))]
    ctx: Option<&'ctx ResourceLoadContext>,
}

impl<A: ResourceFutureAdapter> ResourceFuture<'static, A> {
    pub(super) fn new(
        adapter: A,
        primary_id: ResourceId,
        cache: Arc<Cache>,
        load_params: ResourceLoadParams<A::BaseLoader>,
    ) -> Self {
        Self {
            adapter,
            primary_id,
            metadata: ResourceFutureMetadata {
                cache,
            },
            load_params,
            ctx: None,
        }
    }
}

impl<'ctx, A: ResourceFutureAdapter> ResourceFuture<'ctx, A> {
    pub(super) fn with_context(
        adapter: A,
        primary_id: ResourceId,
        cache: Arc<Cache>,
        load_params: ResourceLoadParams<A::BaseLoader>,
        ctx: &'ctx ResourceLoadContext,
    ) -> Self {
        Self {
            adapter,
            primary_id,
            metadata: ResourceFutureMetadata {
                cache,
            },
            load_params,
            ctx: Some(ctx),
        }
    }

    /// Convenience method to synchronously check if the future is complete by polling once.
    ///
    /// If the future is not complete, yields itself as the `Err()`.
    pub fn check(mut self) -> Result<ResourceLoadResult<<A::BaseLoader as ResourceLoader>::Output>, Self>
    where
        A: Unpin,
    {
        let mut cx = Context::from_waker(Waker::noop());
        match self.poll(&mut cx) {
            Poll::Ready(result) => Ok(result),
            Poll::Pending => Err(self),
        }
    }

    /// Type-erase the inner future by converting it to a boxed future.
    ///
    /// This allows for shared storage of different adapter patterns. For example:
    /// ```
    /// use gtether::resource::manager::{ResourceFutureBoxed, ResourceManager};
    /// use gtether::resource::IdentityLoader;
    /// use gtether::worker::WorkerPool;
    ///
    /// let workers = WorkerPool::single().start();
    /// let manager = ResourceManager::builder().worker_config((), &workers).build();
    ///
    /// let mut tasks: Vec<ResourceFutureBoxed<IdentityLoader>> = vec![];
    ///
    /// tasks.push(
    ///     manager.get_with_loader("a", IdentityLoader::default())
    ///         .to_boxed()
    /// );
    /// tasks.push(
    ///     // This would normally have a different type
    ///     manager.get_with_loader("b", IdentityLoader::default())
    ///         .or_get_with_loader("c", IdentityLoader::default())
    ///         .to_boxed()
    /// );
    /// ```
    pub fn to_boxed(self) -> ResourceFutureBoxed<'ctx, A::BaseLoader>
    where
        A: Unpin + 'static,
    {
        ResourceFuture {
            adapter: Box::new(self.adapter),
            primary_id: self.primary_id,
            metadata: self.metadata,
            load_params: self.load_params,
            ctx: self.ctx,
        }
    }

    /// Compose this ResourceFuture with a fallback load configuration using a default
    /// [ResourceLoader].
    ///
    /// Creates and uses the [ResourceLoader] specified by [ResourceDefaultLoader].
    ///
    /// If this ResourceFuture would resolve to a [ResourceLoadError::NotFound], instead it attempts
    /// to load this fallback configuration instead.
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::ResourceManager;
    /// use gtether::resource::source::constant::ConstantResourceSource;
    /// use gtether::worker::WorkerPool;
    /// # use smol::future;
    ///
    /// static BYTES: [u8; 0] = [];
    ///
    /// let workers = WorkerPool::single().start();
    /// let manager = ResourceManager::builder()
    ///     .worker_config((), &workers)
    ///     .source(ConstantResourceSource::builder()
    ///         .resource("b", &BYTES)
    ///         .build())
    ///     .build();
    ///
    /// # future::block_on(async move {
    /// // This uses IdentityLoader based on ResourceDefaultLoader implemented for ResourceId
    /// let id = manager.get::<ResourceId>("a")
    ///     .or_get("b")
    ///     .await.expect("a resource should be found");
    /// assert_eq!(*id.read(), ResourceId::from("b"));
    /// # });
    /// ```
    pub fn or_get(
        self,
        id: impl Into<ResourceId>,
    ) -> ResourceFuture<'ctx, OrGetOrLoad<A, A::BaseLoader>>
    where
        A::BaseLoader: Clone,
    {
        let loader = (*self.load_params.loader).clone();
        self.or_get_with_loader(id, loader)
    }

    /// Compose this ResourceFuture with a fallback load configuration.
    ///
    /// If this ResourceFuture would resolve to a [ResourceLoadError::NotFound], instead it attempts
    /// to load this fallback configuration instead.
    ///
    /// ```
    /// use gtether::resource::IdentityLoader;
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::ResourceManager;
    /// use gtether::resource::source::constant::ConstantResourceSource;
    /// use gtether::worker::WorkerPool;
    /// # use smol::future;
    ///
    /// static BYTES: [u8; 0] = [];
    ///
    /// let workers = WorkerPool::single().start();
    /// let manager = ResourceManager::builder()
    ///     .worker_config((), &workers)
    ///     .source(ConstantResourceSource::builder()
    ///         .resource("b", &BYTES)
    ///         .build())
    ///     .build();
    ///
    /// # future::block_on(async move {
    /// let id = manager.get_with_loader("a", IdentityLoader)
    ///     .or_get_with_loader("b", IdentityLoader)
    ///     .await.expect("a resource should be found");
    /// assert_eq!(*id.read(), ResourceId::from("b"));
    /// # });
    /// ```
    pub fn or_get_with_loader<L>(
        self,
        id: impl Into<ResourceId>,
        loader: L,
    ) -> ResourceFuture<'ctx, OrGetOrLoad<A, L>>
    where
        L: ResourceLoader<Output=<A::BaseLoader as ResourceLoader>::Output>,
    {
        let adapter = OrGetOrLoad(AdapterState::Primary {
            adapter: self.adapter,
            data: Some(OrGetOrLoadData {
                id: id.into(),
                load_params: self.load_params.clone()
                    .with_loader(Arc::new(loader)),
            }),
        });

        ResourceFuture {
            adapter,
            primary_id: self.primary_id,
            metadata: self.metadata,
            load_params: self.load_params,
            ctx: self.ctx,
        }
    }

    /// Compose this ResourceFuture with a fallback default.
    ///
    /// If this ResourceFuture would resolve to a [ResourceLoadError::NotFound], instead it creates
    /// a default value instead, using the resource values [Default] implementation.
    ///
    /// ```
    /// use gtether::resource::MaybeIdentityLoader;
    /// use gtether::resource::manager::ResourceManager;
    /// use gtether::worker::WorkerPool;
    /// # use smol::future;
    ///
    /// let workers = WorkerPool::single().start();
    /// let manager = ResourceManager::builder()
    ///     .worker_config((), &workers)
    ///     .build();
    ///
    /// # future::block_on(async move {
    /// let id = manager.get_with_loader("a", MaybeIdentityLoader)
    ///     .or_default()
    ///     .await.expect("a default should be loaded");
    /// assert_eq!(*id.read(), None);
    /// # });
    /// ```
    pub fn or_default(self) -> ResourceFuture<'ctx, OrDefault<A>>
    where
        <A::BaseLoader as ResourceLoader>::Output: Default,
    {
        let adapter = OrDefault(AdapterState::Primary {
            adapter: self.adapter,
            data: Some(OrDefaultData {
                id: self.primary_id,
                load_params: self.load_params.clone(),
            }),
        });

        ResourceFuture {
            adapter,
            primary_id: self.primary_id,
            metadata: self.metadata,
            load_params: self.load_params,
            ctx: self.ctx,
        }
    }

    /// Compose this ResourceFuture with a fallback default.
    ///
    /// If this ResourceFuture would resolve to a [ResourceLoadError::NotFound], instead it creates
    /// a default value instead, using the loader's [default implementation](ResourceLoaderDefault).
    ///
    /// ```
    /// use gtether::resource::MaybeIdentityLoader;
    /// use gtether::resource::manager::ResourceManager;
    /// use gtether::worker::WorkerPool;
    /// # use smol::future;
    ///
    /// let workers = WorkerPool::single().start();
    /// let manager = ResourceManager::builder()
    ///     .worker_config((), &workers)
    ///     .build();
    ///
    /// # future::block_on(async move {
    /// let id = manager.get_with_loader("a", MaybeIdentityLoader)
    ///     .or_loader_default()
    ///     .await.expect("a default should be loaded");
    /// assert_eq!(*id.read(), None);
    /// # });
    /// ```
    pub fn or_loader_default(self) -> ResourceFuture<'ctx, OrLoaderDefault<A>>
    where
        A::BaseLoader: ResourceLoaderDefault,
    {
        let adapter = OrLoaderDefault(AdapterState::Primary {
            adapter: self.adapter,
            data: Some(OrLoaderDefaultData {
                id: self.primary_id,
                load_params: self.load_params.clone(),
            }),
        });

        ResourceFuture {
            adapter,
            primary_id: self.primary_id,
            metadata: self.metadata,
            load_params: self.load_params,
            ctx: self.ctx,
        }
    }

    /// Compose this ResourceFuture with a fallback default.
    ///
    /// If this ResourceFuture would resolve to a [ResourceLoadError::NotFound], instead it creates
    /// a default value instead, using the loader's [default implementation](ResourceLoaderDefault).
    ///
    /// ```
    /// use futures_util::FutureExt;
    /// use gtether::resource::MaybeIdentityLoader;
    /// use gtether::resource::manager::ResourceManager;
    /// use gtether::worker::WorkerPool;
    /// # use smol::future;
    ///
    /// let workers = WorkerPool::single().start();
    /// let manager = ResourceManager::builder()
    ///     .worker_config((), &workers)
    ///     .build();
    ///
    /// # future::block_on(async move {
    /// let id = manager.get_with_loader("a", MaybeIdentityLoader)
    ///     .or_default_with(|_| async move {
    ///         Ok(Box::new(None))
    ///     }.boxed())
    ///     .await.expect("a default should be loaded");
    /// assert_eq!(*id.read(), None);
    /// # });
    /// ```
    pub fn or_default_with<DefaultFn>(
        self,
        default_fn: DefaultFn,
    ) -> ResourceFuture<'ctx, OrDefaultWith<A, DefaultFn>>
    where
            for<'any> DefaultFn: (Fn(&'any ResourceLoadContext)
        -> BoxFuture<'any, Result<Box<<A::BaseLoader as ResourceLoader>::Output>, ResourceLoadError>>) + Send + Sync + 'static,
    {
        let adapter = OrDefaultWith(AdapterState::Primary {
            adapter: self.adapter,
            data: Some(OrDefaultWithData {
                id: self.primary_id,
                load_params: self.load_params.clone(),
                default_fn,
            }),
        });

        ResourceFuture {
            adapter,
            primary_id: self.primary_id,
            metadata: self.metadata,
            load_params: self.load_params,
            ctx: self.ctx,
        }
    }
}

impl<'ctx, A: ResourceFutureAdapter> Future for ResourceFuture<'ctx, A> {
    type Output = ResourceLoadResult<<A::BaseLoader as ResourceLoader>::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();
        let result = ready!(this.adapter.poll(cx, &this.metadata));

        if let Ok(resource) = &result && let Some(ctx) = this.ctx {
            ctx.add_dependency(resource.id().clone());
            ctx.cache_resource(resource.clone());
        }

        Poll::Ready(result)
    }
}

impl<'ctx, A: ResourceFutureAdapter> FusedFuture for ResourceFuture<'ctx, A> {
    #[inline]
    fn is_terminated(&self) -> bool {
        self.adapter.is_terminated()
    }
}

/// Trait implemented by [adapters](super::future#adapters).
///
/// This trait is not intended to be implemented externally, and it would be difficult if not
/// impossible to properly do so. It is instead used as a bound for provided adapters to function
/// generically.
pub trait ResourceFutureAdapter {
    type BaseLoader: ?Sized + ResourceLoader;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        metadata: &ResourceFutureMetadata,
    ) -> Poll<ResourceLoadResult<<Self::BaseLoader as ResourceLoader>::Output>>;

    fn is_terminated(&self) -> bool;
}

impl<A: ?Sized + ResourceFutureAdapter + Unpin> ResourceFutureAdapter for Box<A> {
    type BaseLoader = A::BaseLoader;

    #[inline]
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        metadata: &ResourceFutureMetadata,
    ) -> Poll<ResourceLoadResult<<Self::BaseLoader as ResourceLoader>::Output>> {
        Pin::new(&mut (**self)).poll(cx, metadata)
    }

    #[inline]
    fn is_terminated(&self) -> bool {
        (**self).is_terminated()
    }
}

macro_rules! impl_pinned_adapter_wrapper {
    () => {
        type BaseLoader = A::BaseLoader;

        fn poll(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            metadata: &ResourceFutureMetadata,
        ) -> Poll<ResourceLoadResult<<Self::BaseLoader as ResourceLoader>::Output>> {
            self.as_mut().project().0.poll(cx, metadata)
        }

        fn is_terminated(&self) -> bool {
            self.0.is_terminated()
        }
    };
}

/// Base future adapter for [`ResourceManager::get_*()`](crate::resource::manager::ResourceManager::get).
///
/// This adapter is not directly accessible, but is available for type naming with [ResourceFuture].
#[pin_project(project = GetOrLoadProj, project_replace = GetOrLoadProjReplace)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub enum GetOrLoad<Loader: ?Sized + ResourceLoader> {
    Cached(ResourceLoadResult<Loader::Output>),
    Loading(#[pin] ResourceLoadFuture<Loader>),
    Updating(#[pin] ResourceUpdateFuture<Loader::Output>),
    Complete,
}

impl<Loader: ?Sized + ResourceLoader> Debug for GetOrLoad<Loader> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "GetOrLoad::")?;
        match self {
            Self::Cached(Ok(res)) => write!(f, "Cached(Ok({}))", res.id())?,
            Self::Cached(Err(e)) => {
                write!(f, "Cached(Err(")?;
                Debug::fmt(e, f)?;
                write!(f, "))")?
            },
            Self::Loading(_) => write!(f, "Loading(<load-future>)")?,
            Self::Updating(_) => write!(f, "Updating(<update-future>)")?,
            Self::Complete => write!(f, "Complete")?,
        }
        Ok(())
    }
}

impl<Loader: ?Sized + ResourceLoader> ResourceFutureAdapter for GetOrLoad<Loader> {
    type BaseLoader = Loader;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _metadata: &ResourceFutureMetadata,
    ) -> Poll<ResourceLoadResult<<Self::BaseLoader as ResourceLoader>::Output>> {
        match self.as_mut().project() {
            GetOrLoadProj::Cached(_) => {
                match self.project_replace(Self::Complete) {
                    GetOrLoadProjReplace::Cached(result) => {
                        Poll::Ready(result)
                    },
                    _ => unreachable!(),
                }
            },
            GetOrLoadProj::Loading(fut) => {
                let output = ready!(fut.poll(cx));
                match self.project_replace(Self::Complete) {
                    GetOrLoadProjReplace::Loading(_) => Poll::Ready(output),
                    _ => unreachable!(),
                }
            },
            GetOrLoadProj::Updating(fut) => {
                let output = ready!(fut.poll(cx));
                match self.project_replace(Self::Complete) {
                    GetOrLoadProjReplace::Updating(_) => Poll::Ready(output),
                    _ => unreachable!(),
                }
            },
            GetOrLoadProj::Complete => {
                panic!("GetOrLoad must not be polled after it returned `Poll::Ready`")
            }
        }
    }

    fn is_terminated(&self) -> bool {
        match self {
            Self::Complete => true,
            _ => false,
        }
    }
}

trait AdapterData {
    type Adapter;

    fn into_adapter(self, cache: &Arc<Cache>) -> Self::Adapter
    where
        Self: Sized;
}

#[pin_project(project = AdapterStateProj, project_replace = AdapterStateProjReplace)]
enum AdapterState<A1, A2, D> {
    Primary {
        #[pin] adapter: A1,
        data: Option<D>,
    },
    Secondary {
        #[pin] adapter: A2,
    },
    Complete,
}

impl<A1, A2, D> Debug for AdapterState<A1, A2, D>
where
    A1: Debug,
    A2: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "AdapterState::")?;
        match self {
            Self::Primary { adapter, .. } => {
                write!(f, "Primary(")?;
                Debug::fmt(adapter, f)?;
                write!(f, ")")?;
            },
            Self::Secondary { adapter } => {
                write!(f, "Secondary(")?;
                Debug::fmt(adapter, f)?;
                write!(f, ")")?;
            },
            Self::Complete => write!(f, "Complete")?,
        }
        Ok(())
    }
}

impl<A1, A2, D> ResourceFutureAdapter for AdapterState<A1, A2, D>
where
    A1: ResourceFutureAdapter,
    A2: ResourceFutureAdapter,
    A2::BaseLoader: ResourceLoader<Output=<A1::BaseLoader as ResourceLoader>::Output>,
    D: AdapterData<Adapter=A2>,
{
    type BaseLoader = A1::BaseLoader;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        metadata: &ResourceFutureMetadata,
    ) -> Poll<ResourceLoadResult<<Self::BaseLoader as ResourceLoader>::Output>> {
        match self.as_mut().project() {
            AdapterStateProj::Primary {
                adapter,
                data,
            } => {
                let output = ready!(adapter.poll(cx, &metadata));
                match output {
                    Err(ResourceLoadError::NotFound(_)) => {
                        let adapter = data.take()
                            .expect("transition from Primary => Secondary should only occur once")
                            .into_adapter(&metadata.cache);
                        match self.project_replace(Self::Secondary { adapter }) {
                            AdapterStateProjReplace::Primary { .. } => {
                                // We want to re-poll immediately
                                cx.waker().wake_by_ref();
                                Poll::Pending
                            },
                            _ => unreachable!(),
                        }
                    },
                    result => {
                        match self.project_replace(Self::Complete) {
                            AdapterStateProjReplace::Primary { .. } => Poll::Ready(result),
                            _ => unreachable!(),
                        }
                    },
                }
            },
            AdapterStateProj::Secondary {
                adapter,
            } => {
                let output = ready!(adapter.poll(cx, metadata));
                match self.project_replace(Self::Complete) {
                    AdapterStateProjReplace::Secondary { .. } => Poll::Ready(output),
                    _ => unreachable!(),
                }
            },
            AdapterStateProj::Complete => {
                panic!("adapter future must not be polled after it returned `Poll::Ready`")
            },
        }
    }

    fn is_terminated(&self) -> bool {
        match self {
            Self::Complete => true,
            _ => false,
        }
    }
}

struct OrGetOrLoadData<NewLoader: ?Sized> {
    id: ResourceId,
    load_params: ResourceLoadParams<NewLoader>,
}

impl<NewLoader: ?Sized + ResourceLoader> AdapterData for OrGetOrLoadData<NewLoader> {
    type Adapter = GetOrLoad<NewLoader>;

    fn into_adapter(self, cache: &Arc<Cache>) -> Self::Adapter
    where
        Self: Sized,
    {
        cache.get_or_load(self.id, self.load_params)
    }
}

/// Adapter future for [`ResourceFuture::or_get_*()`](ResourceFuture::or_get).
///
/// This future is not directly accessible, but is available for type naming with [ResourceFuture].
#[pin_project]
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Educe)]
#[educe(Debug)]
pub struct OrGetOrLoad<A, L: ?Sized + ResourceLoader>(#[pin] AdapterState<
    A,
    GetOrLoad<L>,
    OrGetOrLoadData<L>,
>);

impl<A, L> ResourceFutureAdapter for OrGetOrLoad<A, L>
where
    A: ResourceFutureAdapter,
    L: ?Sized + ResourceLoader<Output=<A::BaseLoader as ResourceLoader>::Output>,
{
    impl_pinned_adapter_wrapper!();
}

struct OrDefaultData<L: ?Sized> {
    id: ResourceId,
    load_params: ResourceLoadParams<L>,
}

impl<L> AdapterData for OrDefaultData<L>
where
    L: ?Sized + ResourceLoader,
    L::Output: Default,
{
    type Adapter = GetOrLoad<L>;

    fn into_adapter(self, cache: &Arc<Cache>) -> Self::Adapter
    where
        Self: Sized,
    {
        cache.load_default(
            self.id,
            self.load_params,
            |_| { async move {
                Ok(Box::new(L::Output::default()))
            }.boxed() },
        )
    }
}

/// Adapter future for [`ResourceFuture::or_default()`].
///
/// This future is not directly accessible, but is available for type naming with [ResourceFuture].
#[pin_project]
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Educe)]
#[educe(Debug)]
pub struct OrDefault<A: ResourceFutureAdapter>(#[pin] AdapterState<
    A,
    GetOrLoad<A::BaseLoader>,
    OrDefaultData<A::BaseLoader>,
>);

impl<A> ResourceFutureAdapter for OrDefault<A>
where
    A: ResourceFutureAdapter,
    <A::BaseLoader as ResourceLoader>::Output: Default,
{
    impl_pinned_adapter_wrapper!();
}

struct OrLoaderDefaultData<L: ?Sized> {
    id: ResourceId,
    load_params: ResourceLoadParams<L>,
}

impl<L> AdapterData for OrLoaderDefaultData<L>
where
    L: ?Sized + ResourceLoaderDefault,
{
    type Adapter = GetOrLoad<L>;

    fn into_adapter(self, cache: &Arc<Cache>) -> Self::Adapter
    where
        Self: Sized,
    {
        let loader = self.load_params.loader.clone();
        cache.load_default(
            self.id,
            self.load_params,
            move |ctx| {
                let loader = loader.clone();
                async move {
                    loader.load_default(ctx).await
                }.boxed()
            },
        )
    }
}

/// Adapter future for [`ResourceFuture::or_loader_default()`].
///
/// This future is not directly accessible, but is available for type naming with [ResourceFuture].
#[pin_project]
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Educe)]
#[educe(Debug)]
pub struct OrLoaderDefault<A: ResourceFutureAdapter>(#[pin] AdapterState<
    A,
    GetOrLoad<A::BaseLoader>,
    OrLoaderDefaultData<A::BaseLoader>,
>);

impl<A> ResourceFutureAdapter for OrLoaderDefault<A>
where
    A: ResourceFutureAdapter,
    A::BaseLoader: ResourceLoaderDefault,
{
    impl_pinned_adapter_wrapper!();
}

struct OrDefaultWithData<L: ?Sized, DefaultFn> {
    id: ResourceId,
    load_params: ResourceLoadParams<L>,
    default_fn: DefaultFn,
}

impl<L, DefaultFn> AdapterData for OrDefaultWithData<L, DefaultFn>
where
    L: ?Sized + ResourceLoader,
    for<'any> DefaultFn: (Fn(&'any ResourceLoadContext)
        -> BoxFuture<'any, Result<Box<L::Output>, ResourceLoadError>>) + Send + Sync + 'static,
{
    type Adapter = GetOrLoad<L>;

    fn into_adapter(self, cache: &Arc<Cache>) -> Self::Adapter
    where
        Self: Sized,
    {
        cache.load_default(
            self.id,
            self.load_params,
            self.default_fn,
        )
    }
}

/// Adapter future for [`ResourceFuture::or_default_with()`].
///
/// This future is not directly accessible, but is available for type naming with [ResourceFuture].
#[pin_project]
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Educe)]
#[educe(Debug)]
pub struct OrDefaultWith<A: ResourceFutureAdapter, DefaultFn>(#[pin] AdapterState<
    A,
    GetOrLoad<A::BaseLoader>,
    OrDefaultWithData<A::BaseLoader, DefaultFn>,
>);

impl<A, DefaultFn> ResourceFutureAdapter for OrDefaultWith<A, DefaultFn>
where
    A: ResourceFutureAdapter,
    for<'any> DefaultFn: (Fn(&'any ResourceLoadContext)
        -> BoxFuture<'any, Result<Box<<A::BaseLoader as ResourceLoader>::Output>, ResourceLoadError>>) + Send + Sync + 'static,
{
    impl_pinned_adapter_wrapper!();
}