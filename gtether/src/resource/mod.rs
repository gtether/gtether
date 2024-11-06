use async_trait::async_trait;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use smol::io::AsyncRead;
use std::any::TypeId;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::pin::Pin;
use std::sync::Arc;

use crate::resource::path::ResourcePath;

pub mod constant;
pub mod manager;
pub mod path;
pub mod source;

pub type ResourceLock<'a, T> = RwLockReadGuard<'a, T>;
pub type ResourceMutLock<'a, T> = RwLockWriteGuard<'a, T>;

#[derive(Debug)]
pub struct Resource<T: Send + Sync + 'static> {
    value: RwLock<T>,
}

impl<T: Send + Sync + 'static> Resource<T> {
    #[inline]
    pub fn new(value: T) -> Self {
        Self {
            value: RwLock::new(value),
        }
    }

    #[inline]
    pub fn read(&self) -> ResourceLock<'_, T> {
        self.value.read()
    }
}

pub struct ResourceMut<T: Send + Sync + 'static> {
    id: ResourcePath,
    inner: Arc<Resource<T>>,
}

impl<T: Send + Sync + 'static> ResourceMut<T> {
    #[inline]
    pub fn new(id: ResourcePath, value: T) -> (Self, Arc<Resource<T>>) {
        let resource = Arc::new(Resource::new(value));
        let resource_mut = Self {
            id,
            inner: resource.clone(),
        };
        (resource_mut, resource)
    }

    // An instance of Arc<Resource> should not be allowed to be arbitrarily mutated, so getting
    //  mutable access like this is only allowed within this module for inner mechanisms.
    #[inline]
    pub(in crate::resource) fn from_resource(id: ResourcePath, resource: Arc<Resource<T>>) -> Self {
        Self {
            id,
            inner: resource,
        }
    }

    #[inline]
    pub fn id(&self) -> &ResourcePath {
        &self.id
    }

    #[inline]
    pub fn resource(&self) -> &Arc<Resource<T>> {
        &self.inner
    }

    #[inline]
    pub fn read(&self) -> ResourceLock<'_, T> {
        self.inner.value.read()
    }

    #[inline]
    pub fn write(&self) -> ResourceMutLock<'_, T> {
        self.inner.value.write()
    }
}

#[derive(Clone, Debug)]
pub enum ResourceLoadError {
    NotFound(ResourcePath),
    TypeMismatch{
        requested: TypeId,
        actual: TypeId,
    },
    ReadError(Arc<dyn Error + Send + Sync + 'static>),
}

impl ResourceLoadError {
    #[inline]
    pub fn from_mismatch<T: Send + Sync + 'static>(actual: TypeId) -> Self {
        Self::TypeMismatch { actual, requested: TypeId::of::<T>() }
    }

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

pub type ResourceReadData = Pin<Box<dyn AsyncRead + Send>>;

#[async_trait]
pub trait ResourceLoader<T: Send + Sync + 'static>: Send + Sync + 'static {
    async fn load(&self, data: ResourceReadData) -> Result<T, ResourceLoadError>;

    async fn update(&self, resource: ResourceMut<T>, data: ResourceReadData)
        -> Result<(), ResourceLoadError>
    {
        *resource.write() = self.load(data).await?;
        Ok(())
    }
}