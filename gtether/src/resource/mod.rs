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
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use smol::io::AsyncRead;
use std::any::TypeId;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::pin::Pin;
use std::sync::Arc;

use crate::resource::path::ResourcePath;

pub mod manager;
pub mod path;
pub mod source;

/// Read-only lock guard for getting [Resource] data.
///
/// Note: These locks are not async safe, and may result in deadlocks if held across an async await
/// point. It is recommended to only hold the lock within synchronous code.
pub type ResourceLock<'a, T> = RwLockReadGuard<'a, T>;

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
pub type ResourceMutLock<'a, T> = RwLockWriteGuard<'a, T>;

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
/// [rm]: manager::ResourceManager
/// [rl]: ResourceLoader
#[derive(Debug)]
pub struct Resource<T: Send + Sync + 'static> {
    value: RwLock<T>,
}

impl<T: Send + Sync + 'static> Resource<T> {
    #[inline]
    pub(in crate::resource) fn new(value: T) -> Self {
        Self {
            value: RwLock::new(value),
        }
    }

    /// Retrieve a read-only [lock guard][lg] for this Resource.
    ///
    /// [lg]: ResourceLock
    #[inline]
    pub fn read(&self) -> ResourceLock<'_, T> {
        self.value.read()
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
pub struct ResourceMut<T: Send + Sync + 'static> {
    id: ResourcePath,
    inner: Arc<Resource<T>>,
}

impl<T: Send + Sync + 'static> ResourceMut<T> {
    // An instance of Arc<Resource> should not be allowed to be arbitrarily mutated, so getting
    //  mutable access like this is only allowed within this module for inner mechanisms.
    #[inline]
    pub(in crate::resource) fn from_resource(id: ResourcePath, resource: Arc<Resource<T>>) -> Self {
        Self {
            id,
            inner: resource,
        }
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
        self.inner.value.read()
    }

    /// Retrieve a writeable [lock guard][lg] for this resource.
    ///
    /// Note: see the [lock guard][lg] docs for async safety.
    ///
    /// [lg]: ResourceMutLock
    #[inline]
    pub fn write(&self) -> ResourceMutLock<'_, T> {
        self.inner.value.write()
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
    pub fn from_mismatch<T: Send + Sync + 'static>(actual: TypeId) -> Self {
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
///     async fn load(&self, mut data: ResourceReadData) -> Result<String, ResourceLoadError> {
///         let mut output = String::new();
///         data.read_to_string(&mut output).await
///             .map_err(|err| ResourceLoadError::from_error(err))
///             .map(|_| output)
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
pub trait ResourceLoader<T: Send + Sync + 'static>: Send + Sync + 'static {
    /// Load and create a value from [raw data][rd].
    ///
    /// [rd]: ResourceReadData
    async fn load(&self, data: ResourceReadData) -> Result<T, ResourceLoadError>;

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
        *resource.write() = self.load(data).await?;
        Ok(())
    }
}