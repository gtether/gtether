use educe::Educe;
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
use crate::resource::{ResourceDefaultLoader, ResourceLoadError, ResourceLoadResult, ResourceLoader};

#[derive(Educe)]
#[educe(Clone)]
#[doc(hidden)]
pub struct _ResourceFutureInternals<T: ?Sized + Send + Sync + 'static> {
    cache: Arc<Cache>,
    load_params: Arc<ResourceLoadParams<T>>,
}

pub type ResourceFutureBoxed<'ctx, T> = ResourceFuture<'ctx, T, Box<dyn Future<Output=ResourceLoadResult<T>> + Unpin>>;

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
pub struct ResourceFuture<'ctx, T, Fut>
where
    T: ?Sized + Send + Sync + 'static,
    Fut: Future<Output=ResourceLoadResult<T>>,
{
    #[pin] future: Fut,
    #[educe(Debug(ignore))]
    _internals: _ResourceFutureInternals<T>,
    #[educe(Debug(ignore))]
    ctx: Option<&'ctx ResourceLoadContext>,
}

impl<T, Fut> ResourceFuture<'static, T, Fut>
where
    T: ?Sized + Send + Sync + 'static,
    Fut: Future<Output=ResourceLoadResult<T>>,
{
    pub(super) fn new(
        future: Fut,
        cache: Arc<Cache>,
        load_params: ResourceLoadParams<T>,
    ) -> Self {
        Self {
            future,
            _internals: _ResourceFutureInternals {
                cache,
                load_params: Arc::new(load_params),
            },
            ctx: None,
        }
    }
}

impl<'ctx, T, Fut> ResourceFuture<'ctx, T, Fut>
where
    T: ?Sized + Send + Sync + 'static,
    Fut: Future<Output=ResourceLoadResult<T>>,
{
    pub(super) fn with_context(
        future: Fut,
        cache: Arc<Cache>,
        load_params: ResourceLoadParams<T>,
        ctx: &'ctx ResourceLoadContext,
    ) -> Self {
        Self {
            future,
            _internals: _ResourceFutureInternals {
                cache,
                load_params: Arc::new(load_params),
            },
            ctx: Some(ctx),
        }
    }

    /// Convenience method to synchronously check if the future is complete by polling once.
    ///
    /// If the future is not complete, yields itself as the `Err()`.
    pub fn check(mut self) -> Result<ResourceLoadResult<T>, Self>
    where
        Fut: Unpin,
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
    /// # use async_trait::async_trait;
    /// use gtether::resource::manager::ResourceFutureBoxed;
    /// # use gtether::resource::manager::{ResourceLoadContext, ResourceManager};
    /// # use gtether::resource::{ResourceLoadError, ResourceLoader, ResourceReadData};
    /// # use gtether::worker::WorkerPool;
    /// #
    /// # #[derive(Default)]
    /// # struct StringLoader(());
    /// #
    /// # #[async_trait]
    /// # impl ResourceLoader<String> for StringLoader {
    /// #     async fn load(
    /// #         &self,
    /// #         data: ResourceReadData,
    /// #         ctx: &ResourceLoadContext,
    /// #     ) -> Result<Box<String>, ResourceLoadError> {
    /// #         Ok(Box::new(ctx.id().to_string()))
    /// #     }
    /// # }
    /// #
    /// # let workers = WorkerPool::single().start();
    /// # let manager = ResourceManager::builder().worker_config((), &workers).build();
    ///
    /// let mut tasks: Vec<ResourceFutureBoxed<String>> = vec![];
    ///
    /// // Given some `StringLoader` that yields Strings:
    /// tasks.push(
    ///     manager.get_with_loader("a", StringLoader::default())
    ///         .to_boxed()
    /// );
    /// tasks.push(
    ///     // This would normally have a different type
    ///     manager.get_with_loader("b", StringLoader::default())
    ///         .or_get_with_loader("c", StringLoader::default())
    ///         .to_boxed()
    /// );
    /// ```
    pub fn to_boxed(self) -> ResourceFutureBoxed<'ctx, T>
    where
        Fut: Unpin + 'static,
    {
        ResourceFuture {
            future: Box::new(self.future),
            _internals: self._internals,
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
    pub fn or_get(self, id: impl Into<ResourceId>) -> ResourceFuture<'ctx, T, OrGetOrLoad<T, Fut>>
    where
        T: ResourceDefaultLoader,
    {
        self.or_get_with_loader(id, T::default_loader())
    }

    /// Compose this ResourceFuture with a fallback load configuration.
    ///
    /// If this ResourceFuture would resolve to a [ResourceLoadError::NotFound], instead it attempts
    /// to load this fallback configuration instead.
    pub fn or_get_with_loader(
        self,
        id: impl Into<ResourceId>,
        loader: impl ResourceLoader<T>
    ) -> ResourceFuture<'ctx, T, OrGetOrLoad<T, Fut>> {
        let future = OrGetOrLoad::First {
            future: self.future,
            id: id.into(),
            loader: Arc::new(loader),
            _internals: self._internals.clone(),
        };

        ResourceFuture {
            future,
            _internals: self._internals,
            ctx: self.ctx,
        }
    }
}

impl<'ctx, T, Fut> Future for ResourceFuture<'ctx, T, Fut>
where
    T: ?Sized + Send + Sync + 'static,
    Fut: Future<Output=ResourceLoadResult<T>>,
{
    type Output = ResourceLoadResult<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();
        let result = ready!(this.future.poll(cx));

        if let Ok(resource) = &result && let Some(ctx) = this.ctx {
            ctx.add_dependency(resource.id().clone());
            ctx.cache_resource(resource.clone());
        }

        Poll::Ready(result)
    }
}

impl<'ctx, T, Fut> FusedFuture for ResourceFuture<'ctx, T, Fut>
where
    T: ?Sized + Send + Sync + 'static,
    Fut: Future<Output=ResourceLoadResult<T>> + FusedFuture,
{
    #[inline]
    fn is_terminated(&self) -> bool {
        self.future.is_terminated()
    }
}

/// Base future for [`ResourceManager::get_*()`](crate::resource::manager::ResourceManager::get).
///
/// This future is not directly accessible, but is available for type naming with [ResourceFuture].
#[pin_project(project = GetOrLoadProj, project_replace = GetOrLoadProjReplace)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub enum GetOrLoad<T: ?Sized + Send + Sync + 'static> {
    Cached(ResourceLoadResult<T>),
    Loading(#[pin] ResourceLoadFuture<T>),
    Updating(#[pin] ResourceUpdateFuture<T>),
    Complete,
}

impl<T: ?Sized + Send + Sync + 'static> Debug for GetOrLoad<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ResourceFuture(")?;
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
        write!(f, ")")
    }
}

impl<T: ?Sized + Send + Sync + 'static> FusedFuture for GetOrLoad<T> {
    fn is_terminated(&self) -> bool {
        match self {
            Self::Complete => true,
            _ => false,
        }
    }
}

impl<T: ?Sized + Send + Sync + 'static> Future for GetOrLoad<T> {
    type Output = ResourceLoadResult<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
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
}

/// Adapter future for [`ResourceFuture::or_get_*()`](ResourceFuture::or_get).
///
/// This future is not directly accessible, but is available for type naming with [ResourceFuture].
#[pin_project(project = OrGetOrLoadProj, project_replace = OrGetOrLoadProjReplace)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub enum OrGetOrLoad<T, Fut>
where
    T: ?Sized + Send + Sync + 'static,
    Fut: Future<Output=ResourceLoadResult<T>>,
{
    First {
        #[pin] future: Fut,
        id: ResourceId,
        loader: Arc<dyn ResourceLoader<T>>,
        _internals: _ResourceFutureInternals<T>,
    },
    Second {
        #[pin] future: GetOrLoad<T>,
    },
    Complete,
}

impl<T, Fut> FusedFuture for OrGetOrLoad<T, Fut>
where
    T: ?Sized + Send + Sync + 'static,
    Fut: Future<Output=ResourceLoadResult<T>>,
{
    fn is_terminated(&self) -> bool {
        match self {
            Self::Complete => true,
            _ => false,
        }
    }
}

impl<T, Fut> Future for OrGetOrLoad<T, Fut>
where
    T: ?Sized + Send + Sync + 'static,
    Fut: Future<Output=ResourceLoadResult<T>>,
{
    type Output = Fut::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.as_mut().project() {
            OrGetOrLoadProj::First {
                future,
                id,
                loader,
                _internals,
            } => {
                let output = ready!(future.poll(cx));
                match output {
                    Err(ResourceLoadError::NotFound(_)) => {
                        let mut load_params = (*_internals.load_params).clone();
                        load_params.loader = loader.clone();
                        let future = _internals.cache.get_or_load(id.clone(), load_params);
                        match self.project_replace(Self::Second { future }) {
                            OrGetOrLoadProjReplace::First { .. } => {
                                // We want to re-poll immediately
                                cx.waker().wake_by_ref();
                                Poll::Pending
                            },
                            _ => unreachable!(),
                        }
                    },
                    result => {
                        match self.project_replace(Self::Complete) {
                            OrGetOrLoadProjReplace::First { .. } => Poll::Ready(result),
                            _ => unreachable!(),
                        }
                    },
                }
            },
            OrGetOrLoadProj::Second { future } => {
                let output = ready!(future.poll(cx));
                match self.project_replace(Self::Complete) {
                    OrGetOrLoadProjReplace::Second { .. } => Poll::Ready(output),
                    _ => unreachable!(),
                }
            },
            OrGetOrLoadProj::Complete => {
                panic!("OrLoad must not be polled after it returned `Poll::Ready`")
            }
        }
    }
}