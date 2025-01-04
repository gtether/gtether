//! Resource loading and management.
//!
//! This module contains all the logic needed to load, process, and centrally manage any amount
//! of arbitrary resources, such as textures, models, or any other data-based object.
//!
//! There are several subsections of this module, which are responsible for different parts of the
//! whole system:
//!  * [Resources][res], which wrap individual resource types, and provide read-only access.
//!  * [ResourceLoaders][rl], which are user-defined types that describe how to load resource types
//!    from raw byte data.
//!  * [ResourceSources][rs], which are user-defined types that provide raw byte data from a source,
//!    such as from disk or from a remote endpoint.
//!  * The [ResourceManager][rm], which is a centralized manager of all of the above. The
//!    [ResourceManager][rm] is usually accessed via a reference retrieved from the main
//!    [Engine][eng].
//!
//! [res]: Resource
//! [rl]: ResourceLoader
//! [rs]: source
//! [rm]: manager
//! [eng]: crate::Engine

use async_trait::async_trait;
use parking_lot::{MappedRwLockReadGuard, MappedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use smol::future;
use smol::io::AsyncRead;
use std::any::TypeId;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::debug;

use crate::resource::manager::ResourceLoadResult;
use crate::resource::path::ResourcePath;

pub mod manager;
pub mod path;
pub mod source;

struct SubResourceRef<P: ?Sized + Send + Sync + 'static> {
    type_id: Option<TypeId>,
    update_fn: Box<dyn (
        Fn(ResourcePath, Arc<Resource<P>>) -> Box<dyn Future<Output = Result<(), ResourceLoadError>> + Send + 'static>
    ) + Send + Sync + 'static>,
}

impl<P: ?Sized + Send + Sync + 'static> SubResourceRef<P> {
    fn from_sub_resource<T: ?Sized + Send + Sync + 'static>(
        resource: &Arc<Resource<T>>,
        loader: Arc<dyn SubResourceLoader<T, P>>,
    ) -> Self {
        let weak = Arc::downgrade(resource);
        Self {
            type_id: Some(TypeId::of::<T>()),
            update_fn: Box::new(move |id, parent| Box::new({
                let async_weak = weak.clone();
                let async_loader = loader.clone();
                async move {
                    if let Some(resource) = async_weak.upgrade() {
                        // Note: This is a non-async lock, held across await boundaries. Normally
                        // this would be VERY bad, as it can result in deadlocks. However, this is
                        // a read lock, which doesn't prevent other reads, and the only time a write
                        // lock is acquired is when the update lock is held, which it should be
                        // while in this scope.
                        let parent_value = parent.read();
                        let (resource_mut, drop_checker) = ResourceMut::from_resource(id.clone(), resource.clone());
                        async_loader.update(resource_mut, &parent_value).await?;
                        match drop_checker.recv().await {
                            Ok(_) => { debug!("Drop-checker received an unexpected message"); }
                            Err(_) => { /* sender was dropped, this is expected */ }
                        }
                        resource.update_sub_resources(&id).await;
                    }
                    Ok(())
                }
            }))
        }
    }

    fn from_callback<F, Fut>(callback: F) -> Self
    where
        F: (Fn(Arc<Resource<P>>) -> Fut) + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let callback = Arc::new(callback);
        Self {
            type_id: None,
            update_fn: Box::new(move |_, parent| {
                let callback = callback.clone();
                Box::new(async move {
                    callback(parent).await;
                    Ok(())
                })
            })
        }
    }

    async fn update(&self, id: ResourcePath, parent: Arc<Resource<P>>) -> Result<(), ResourceLoadError> {
        Box::into_pin((self.update_fn)(id, parent)).await
    }
}

impl<T: ?Sized + Send + Sync + 'static> Debug for SubResourceRef<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut ds = f.debug_struct("SubResource");
        match &self.type_id {
            Some(type_id) => ds
                .field("type", &"SubResource")
                .field("type_id", &type_id),
            None => ds.field("type", &"Callback"),
        }.finish_non_exhaustive()
    }
}

/// Read-only lock guard for getting [Resource] data.
///
/// Note: These locks are not async safe, and may result in deadlocks if held across an async await
/// point. It is recommended to only hold the lock within synchronous code.
pub type ResourceLock<'a, T> = MappedRwLockReadGuard<'a, T>;

/// Writeable lock guard for mutating [Resource] data.
///
/// Generally only accessible from within [ResourceLoader] update logic.
///
/// Note: These locks are not async safe, and may result in deadlocks if held across an async await
/// point. It is recommended to only hold the lock within synchronous code, and prepare any mutable
/// changes as much as possible before requiring write access to the resource.
///
/// # Example
/// ```
/// use gtether::resource::{ResourceLoadError, ResourceMut, ResourceReadData};
/// use smol::prelude::*;
///
/// # struct Loader {}
/// # impl Loader {
/// async fn update(&self, resource: ResourceMut<String>, mut data: ResourceReadData)
///         -> Result<(), ResourceLoadError> {
///     let mut output = String::new();
///     data.read_to_string(&mut output).await
///         .map_err(|err| ResourceLoadError::from_error(err))?;
///
///     // Grab the lock _after_ all async awaiting is done
///     *resource.write() = output;
///     Ok(())
/// }
/// # }
/// ```
pub type ResourceMutLock<'a, T> = MappedRwLockWriteGuard<'a, T>;

/// Handle for an individual resource.
///
/// Resources wrap other data types, and provide read-only access through [Resource::read()]. The
/// vast majority of the time, Resource handles will be accessed through `Arc<Resource<T>>`, and
/// retrieved through a [ResourceManager][rm].
///
/// If the underlying data that a Resource is created from changes (e.g. by a file it was loaded
/// from changing its contents), then the [ResourceManager][rm] will update and swap out the data
/// that a Resource wraps. For this reason, it is not recommended to keep long-term read locks from
/// Resources.
///
/// If some sort of synchronization is required during a Resource update, that can be achieved via
/// the user-defined [ResourceLoader][rl] used to load Resources.
///
/// # Sub-resources
///
/// Not all resources may be sourced from raw data; some resources may be derived from other
/// existing resources. In this case, resources can be [attached to another resource][ar] as a
/// "sub-resource". These sub-resources will be updated when their parent resource is updated,
/// although they use a slightly different [loader][srl] to do so.
///
/// Example of attaching a sub-resource:
/// ```
/// # use std::sync::Arc;
/// #
/// # use gtether::resource::{Resource, ResourceLoadError, SubResourceLoader};
/// #
/// # fn wrapper(resource: Arc<Resource<String>>, sub_loader: impl SubResourceLoader<String, String>) -> Result<(), ResourceLoadError> {
/// let sub_resource: Arc<Resource<String>> = resource.attach_sub_resource_blocking(sub_loader)?;
/// # Ok(())
/// # }
/// ```
///
/// [rm]: manager::ResourceManager
/// [rl]: ResourceLoader
/// [srl]: SubResourceLoader
/// [ar]: Resource::attach_sub_resource
#[derive(Debug)]
pub struct Resource<T: ?Sized + Send + Sync + 'static> {
    value: RwLock<Box<T>>,
    sub_resources: smol::lock::Mutex<Vec<SubResourceRef<T>>>,
    update_lock: smol::lock::Mutex<()>,
}

impl<T: ?Sized + Send + Sync + 'static> Resource<T> {
    #[inline]
    pub(in crate::resource) fn new(value: Box<T>) -> Self {
        Self {
            value: RwLock::new(value),
            sub_resources: smol::lock::Mutex::new(Vec::new()),
            update_lock: smol::lock::Mutex::new(()),
        }
    }

    pub(in crate::resource) async fn update_sub_resources(self: &Arc<Self>, id: &ResourcePath) {
        let sub_resources = self.sub_resources.lock().await;
        for sub_resource in &*sub_resources {
            let sub_id = id.clone() + ":<sub-resource>";
            match sub_resource.update(sub_id.clone(), self.clone()).await {
                Ok(_) => {},
                Err(error) => debug!(id = %sub_id, %error, "Failed to update sub-resource"),
            }
        }
    }

    /// Retrieve a read-only [lock guard][lg] for this Resource.
    ///
    /// [lg]: ResourceLock
    #[inline]
    pub fn read(&self) -> ResourceLock<'_, T> {
        RwLockReadGuard::map(self.value.read(), |b| b.as_ref())
    }

    /// Create a sub-resource for this resource, which is updated when this resource is updated.
    ///
    /// See [sub-resources](Self#sub-resources) for more information.
    pub async fn attach_sub_resource<S: ?Sized + Send + Sync + 'static>(
        &self,
        loader: impl SubResourceLoader<S, T>,
    ) -> ResourceLoadResult<S> {
        let sub_value = loader.load(&self.read()).await?;
        let mut sub_resources = self.sub_resources.lock().await;
        let sub_resource = Arc::new(Resource::new(sub_value));
        sub_resources.push(SubResourceRef::from_sub_resource(&sub_resource, Arc::new(loader)));
        Ok(sub_resource)
    }

    /// Create a sub-resource for this resource, which is updated when this resource is updated.
    ///
    /// Same as [Self::attach_sub_resource()], but blocking.
    ///
    /// See [sub-resources](Self#sub-resources) for more information.
    #[inline]
    pub fn attach_sub_resource_blocking<S: ?Sized + Send + Sync + 'static>(
        &self,
        loader: impl SubResourceLoader<S, T>,
    ) -> ResourceLoadResult<S> {
        future::block_on(self.attach_sub_resource(loader))
    }

    /// Attach a callback to this resource, which is executed when this resource is updated.
    pub async fn attach_update_callback<F, Fut>(&self, callback: F)
    where
        F: (Fn(Arc<Resource<T>>) -> Fut) + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut sub_resources = self.sub_resources.lock().await;
        sub_resources.push(SubResourceRef::from_callback(callback));
    }

    /// Attach a callback to this resource, which is executed when this resource is updated.
    ///
    /// Same as [Self::attach_update_callback()], but blocking.
    #[inline]
    pub fn attach_update_callback_blocking<F, Fut>(&self, callback: F)
    where
        F: (Fn(Arc<Resource<T>>) -> Fut) + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        future::block_on(self.attach_update_callback(callback))
    }
}

/// Mutable handle for a [Resource].
///
/// Mutable handles are created internally, and generally only accessible during resource update
/// logic. They have an associated [id][rp], since they represent a resource that is currently
/// [managed][rm].
///
/// [id]: ResourcePath
/// [rm]: manager::ResourceManager
pub struct ResourceMut<T: ?Sized + Send + Sync + 'static> {
    id: ResourcePath,
    inner: Arc<Resource<T>>,
    #[allow(dead_code)] // This is simply being held for the duration of this struct
    drop_notice: smol::channel::Sender<()>,
}

impl<T: ?Sized + Send + Sync + 'static> ResourceMut<T> {
    // An instance of Arc<Resource> should not be allowed to be arbitrarily mutated, so getting
    //  mutable access like this is only allowed within this module for inner mechanisms.
    #[inline]
    pub(in crate::resource) fn from_resource(
        id: ResourcePath,
        resource: Arc<Resource<T>>
    ) -> (Self, smol::channel::Receiver<()>) {
        let (drop_notice, drop_checker) = smol::channel::bounded(1);
        (Self {
            id,
            inner: resource,
            drop_notice,
        }, drop_checker)
    }

    /// The [id][rp] associated with this mutable handle.
    ///
    /// [id]: ResourcePath
    #[inline]
    pub fn id(&self) -> &ResourcePath {
        &self.id
    }

    /// The underlying read-only [Resource] handle.
    #[inline]
    pub fn resource(&self) -> &Arc<Resource<T>> {
        &self.inner
    }

    /// Retrieve a read-only [lock guard][lg] for this resource.
    ///
    /// Note: see the [lock guard][lg] docs for async safety.
    ///
    /// [lg]: ResourceLock
    #[inline]
    pub fn read(&self) -> ResourceLock<'_, T> {
        RwLockReadGuard::map(self.inner.value.read(), |b| b.as_ref())
    }

    /// Retrieve a writeable [lock guard][lg] for this resource.
    ///
    /// Note: see the [lock guard][lg] docs for async safety.
    ///
    /// [lg]: ResourceMutLock
    #[inline]
    pub fn write(&self) -> ResourceMutLock<'_, T> {
        RwLockWriteGuard::map(self.inner.value.write(), |b| b.as_mut())
    }

    /// Directly replace the value of this resource.
    ///
    /// Useful for if the value type is unsized, or a simple wholesale replacement is needed.
    #[inline]
    pub fn replace(&self, value: Box<T>) {
        *self.inner.value.write() = value;
    }
}

/// Errors that can occur while loading [Resources][res].
///
/// [res]: Resource
#[derive(Clone, Debug)]
pub enum ResourceLoadError {
    /// There was no resource or resource data found for the given [id].
    ///
    /// [id]: ResourcePath
    NotFound(ResourcePath),

    /// The existing resource type did not match the requested resource type.
    TypeMismatch{
        requested: TypeId,
        actual: TypeId,
    },

    /// Some other error occurred while reading/loading resource data.
    ReadError(Arc<dyn Error + Send + Sync + 'static>),
}

impl ResourceLoadError {
    /// Convenience method for creating a [ResourceLoadError::TypeMismatch].
    #[inline]
    pub fn from_mismatch<T: ?Sized + Send + Sync + 'static>(actual: TypeId) -> Self {
        Self::TypeMismatch { actual, requested: TypeId::of::<T>() }
    }

    /// Convenience method for creating a [ResourceLoadError::ReadError].
    #[inline]
    pub fn from_error(e: impl Error + Send + Sync + 'static) -> Self {
        Self::ReadError(Arc::new(e))
    }
}

impl Display for ResourceLoadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ResourceLoadError::NotFound(id) =>
                write!(f, "No resource found that is associated with ID: '{id}'"),
            ResourceLoadError::TypeMismatch { requested, actual } =>
                write!(f, "Requested type ({requested:?}) does not match previously loaded type ({actual:?})"),
            ResourceLoadError::ReadError(err) =>
                std::fmt::Display::fmt(&err, f),
        }
    }
}

impl Error for ResourceLoadError {}

/// Raw data type for loading resources from.
pub type ResourceReadData = Pin<Box<dyn AsyncRead + Send>>;

/// User-defined interface for loading [Resources][res] from [raw data][rd].
///
/// ResourceLoaders are used to define the behavior for loading and updating [Resources][res] from
/// [raw data][rd]. ResourceLoaders are given to the [ResourceManager][rm] when a [Resource][res]
/// load is requested, and the [ResourceManager][rm] will utilize the ResourceLoader as needed
/// during its lifecycle.
///
/// The [ResourceManager][rm] takes sole ownership of the ResourceLoader, which should be kept in
/// mind when implementing a ResourceLoader. In addition, the [ResourceManager][rm] may call both
/// [ResourceLoader::load()] and [ResourceLoader::update()] multiple times during its lifecycle,
/// so both should be idempotent actions.
///
/// # Examples
/// ```
/// use async_trait::async_trait;
/// use gtether::resource::{ResourceLoadError, ResourceLoader, ResourceMut, ResourceReadData};
/// use smol::prelude::*;
///
/// struct StringLoader {}
///
/// #[async_trait]
/// impl ResourceLoader<String> for StringLoader {
///     async fn load(&self, mut data: ResourceReadData) -> Result<Box<String>, ResourceLoadError> {
///         let mut output = String::new();
///         data.read_to_string(&mut output).await
///             .map_err(ResourceLoadError::from_error)
///             .map(|_| Box::new(output))
///     }
///
///     // update() does not need to be implemented if you don't have any custom synchronization logic
/// }
/// ```
///
/// [res]: Resource
/// [rd]: ResourceReadData
/// [rm]: manager::ResourceManager
#[async_trait]
pub trait ResourceLoader<T: ?Sized + Send + Sync + 'static>: Send + Sync + 'static {
    /// Load and create a value from [raw data][rd].
    ///
    /// [rd]: ResourceReadData
    async fn load(&self, data: ResourceReadData) -> Result<Box<T>, ResourceLoadError>;

    /// Given a [mutable resource handle][rm], load and update a resource from [raw data][rd].
    ///
    /// The default implementation simply re-uses [ResourceLoader::load()], and replaces the
    /// resource value with the new value.
    ///
    /// [rm]: ResourceMut
    /// [rd]: ResourceReadData
    async fn update(&self, resource: ResourceMut<T>, data: ResourceReadData)
        -> Result<(), ResourceLoadError>
    {
        resource.replace(self.load(data).await?);
        Ok(())
    }
}

/// User-defined interface for loading [Resources][res] from parent [Resources][res].
///
/// SubResourceLoaders are very similar to [ResourceLoaders][rl], but are designed to load/update
/// [Resources][res] from other [Resources][res], creating parent/child like relationships.
/// SubResourceLoaders are given to [Resources][res] via [attach_sub_resource()][asr], which will
/// create and return the sub-resource.
///
/// Like normal [ResourceLoaders][rl], [attach_sub_resource()][asr] takes sole ownership of the
/// SubResourceLoader, and both [SubResourceLoader::load()] and [SubResourceLoader::update()] may
/// be called multiple times, so they should be idempotent.
///
/// See also [Resource#sub-resources].
///
/// # Examples
/// ```
/// use std::sync::Arc;
/// use async_trait::async_trait;
///
/// use gtether::resource::{Resource, ResourceLoadError, SubResourceLoader};
///
/// #[derive(Default)]
/// struct SubStringLoader {}
///
/// #[async_trait]
/// impl SubResourceLoader<String, String> for SubStringLoader {
///     async fn load(&self, parent: &String) -> Result<Box<String>, ResourceLoadError> {
///         Ok(Box::new(parent.clone() + "-<substring>"))
///     }
/// }
///
/// fn make_substring(parent: &Arc<Resource<String>>) -> Arc<Resource<String>> {
///     parent.attach_sub_resource_blocking(SubStringLoader::default()).unwrap()
/// }
/// ```
///
/// [res]: Resource
/// [rl]: ResourceLoader
/// [asr]: Resource::attach_sub_resource
#[async_trait]
pub trait SubResourceLoader<T, P>: Send + Sync + 'static
where
    T: ?Sized + Send + Sync + 'static,
    P: ?Sized + Send + Sync + 'static,
{
    async fn load(&self, parent: &P) -> Result<Box<T>, ResourceLoadError>;

    async fn update(&self, resource: ResourceMut<T>, parent: &P)
        -> Result<(), ResourceLoadError>
    {
        resource.replace(self.load(parent).await?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use crate::resource::manager::LoadPriority;
    use super::*;
    use super::manager::tests::*;

    #[derive(Clone, Debug)]
    enum SubStringLoaderError {
        NoSuffix,
        NoUpdate,
    }

    impl Display for SubStringLoaderError {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            match self {
                SubStringLoaderError::NoSuffix => f.write_str("No suffix specified"),
                SubStringLoaderError::NoUpdate => f.write_str("Updates are not allowed"),
            }
        }
    }

    impl Error for SubStringLoaderError {}

    struct SubStringLoader {
        suffix: Option<String>,
        no_update: bool,
    }

    impl SubStringLoader {
        fn new(suffix: impl Into<String>) -> Self {
            Self { suffix: Some(suffix.into()), no_update: false }
        }

        fn err() -> Self {
            Self { suffix: None, no_update: true }
        }

        fn no_update(suffix: impl Into<String>) -> Self {
            Self { suffix: Some(suffix.into()), no_update: true }
        }
    }

    #[async_trait]
    impl SubResourceLoader<String, String> for SubStringLoader {
        async fn load(&self, parent: &String) -> Result<Box<String>, ResourceLoadError> {
            match &self.suffix {
                Some(suffix) => Ok(Box::new(parent.clone() + "-" + &suffix)),
                None => Err(ResourceLoadError::from_error(SubStringLoaderError::NoSuffix)),
            }
        }

        async fn update(&self, resource: ResourceMut<String>, parent: &String) -> Result<(), ResourceLoadError> {
            if self.no_update {
                Err(ResourceLoadError::from_error(SubStringLoaderError::NoUpdate))
            } else {
                resource.replace(self.load(parent).await?);
                Ok(())
            }
        }
    }

    #[test]
    fn test_sub_resource_load_error() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"value", "h_value");

        let fut = manager.get_or_load("key", TestResourceLoader::default(), LoadPriority::Immediate);
        let value = future::block_on(timeout(fut.wait(), Duration::from_secs(1)))
            .expect("Resource should load for 'key'");

        value.attach_sub_resource_blocking(SubStringLoader::err())
            .expect_err("Sub-resource should fail to load");
    }

    #[test]
    fn test_sub_resource_update() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"value", "h_value");

        let fut = manager.get_or_load("key", TestResourceLoader::default(), LoadPriority::Immediate);
        let value = future::block_on(timeout(fut.wait(), Duration::from_secs(1)))
            .expect("Resource should load for 'key'");

        let sub_value = value.attach_sub_resource_blocking(SubStringLoader::new("subvalue"))
            .expect("Sub-resource should load");
        assert_eq!(*sub_value.read(), "value-subvalue".to_owned());

        data_maps[0].insert("key", b"new_value", "h_new_value");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        assert_eq!(*sub_value.read(), "new_value-subvalue");
    }

    #[test]
    fn test_multi_sub_resource() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"value", "h_value");

        let fut = manager.get_or_load("key", TestResourceLoader::default(), LoadPriority::Immediate);
        let value = future::block_on(timeout(fut.wait(), Duration::from_secs(1)))
            .expect("Resource should load for 'key'");

        let sub_value_1 = value.attach_sub_resource_blocking(SubStringLoader::no_update("subvalue1"))
            .expect("Sub-resource should load");
        let sub_value_2 = value.attach_sub_resource_blocking(SubStringLoader::new("subvalue2"))
            .expect("Sub-resource should load");
        assert_eq!(*sub_value_1.read(), "value-subvalue1".to_owned());
        assert_eq!(*sub_value_2.read(), "value-subvalue2".to_owned());

        // sub_value_1 should fail an update, but that shouldn't prevent sub_value_2 from updating
        data_maps[0].insert("key", b"new_value", "h_new_value");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        assert_eq!(*sub_value_1.read(), "value-subvalue1".to_owned());
        assert_eq!(*sub_value_2.read(), "new_value-subvalue2".to_owned());
    }

    #[test]
    fn test_chained_sub_resource() {
        let (manager, data_maps) = create_resource_manager::<1>();
        data_maps[0].insert("key", b"value", "h_value");

        let fut = manager.get_or_load("key", TestResourceLoader::default(), LoadPriority::Immediate);
        let value = future::block_on(timeout(fut.wait(), Duration::from_secs(1)))
            .expect("Resource should load for 'key'");

        let sub_value = value.attach_sub_resource_blocking(SubStringLoader::new("subvalue"))
            .expect("Sub-resource should load");
        let sub_sub_value = sub_value.attach_sub_resource_blocking(SubStringLoader::new("subsubvalue"))
            .expect("Sub-sub-resource should load");
        assert_eq!(*sub_sub_value.read(), "value-subvalue-subsubvalue".to_owned());

        data_maps[0].insert("key", b"new_value", "h_new_value");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        assert_eq!(*sub_sub_value.read(), "new_value-subvalue-subsubvalue");
    }
}