use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use crate::resource::path::ResourcePath;
use crate::resource::source::{ResourceData, ResourceDataSource, ResourceWatcher};
use crate::resource::source::{ResourceSource, ResourceDataResult, SourceIndex};
use crate::resource::ResourceLoadError;

/// [ResourceSource][rs] that contains static global data.
///
/// This source contains static byte data references, that never change. This is useful for
/// embedding resource data in built binaries, and can often be used for defaults or error-condition
/// resources if proper resource data can't be found or loaded.
///
/// [rs]: ResourceSource
pub struct ConstantResourceSource {
    raw: HashMap<ResourcePath, (&'static [u8], String)>,
}

impl ConstantResourceSource {
    /// Create a new ConstantResourceSource from a pre-built hashmap of static data.
    ///
    /// The values of the hashmap are expected to be pairs of static data references and string
    /// hashes. See the [source module-level documentation][smod] for more info on hashing.
    ///
    /// It is recommended to instead use [Self::builder()] to create new ConstantResourceSources.
    ///
    /// [smod]: super#data-hashing
    #[inline]
    pub fn new(raw: HashMap<ResourcePath, (&'static [u8], String)>) -> Self {
        Self { raw }
    }

    /// Create a ConstantResourceSourceBuilder, used to build a ConstantResourceSource.
    ///
    /// This is the recommended way to create ConstantResourceSources.
    #[inline]
    pub fn builder() -> ConstantResourceSourceBuilder {
        ConstantResourceSourceBuilder::new()
    }
}

#[async_trait]
impl ResourceSource for ConstantResourceSource {
    fn hash(&self, id: &ResourcePath) -> Option<ResourceDataSource> {
        if let Some((_, hash)) = self.raw.get(id) {
            Some(ResourceDataSource::new(hash.clone()))
        } else {
            None
        }
    }

    async fn load(&self, id: &ResourcePath) -> ResourceDataResult {
        if let Some((data, hash)) = self.raw.get(id) {
            Ok(ResourceData::new(Box::new(*data), hash.clone()))
        } else {
            Err(ResourceLoadError::NotFound(id.clone()))
        }
    }

    fn watch(&self, _id: ResourcePath, _watcher: Box<dyn ResourceWatcher>, _sub_idx: Option<SourceIndex>) {
        // Noop because constant data can't change
    }

    fn unwatch(&self, _id: &ResourcePath) {
        // Noop because constant data can't change
    }
}

/// Builder pattern for [ConstantResourceSources][crs].
///
/// This builder allows defining static data for individual resources at a time, using
/// [ConstantResourceSource::resource()].
///
/// [crs]: ConstantResourceSource
pub struct ConstantResourceSourceBuilder {
    raw: HashMap<ResourcePath, (&'static [u8], String)>,
}

impl ConstantResourceSourceBuilder {
    /// Create a new builder.
    ///
    /// The recommended way to create new builders is to use [ConstantResourceSource::builder()].
    #[inline]
    pub fn new() -> Self {
        Self {
            raw: HashMap::new(),
        }
    }

    /// Define an individual resource's static data.
    #[inline]
    pub fn resource(mut self, id: impl Into<ResourcePath>, data: &'static [u8]) -> Self {
        let hash = STANDARD.encode(&Sha256::digest(data));
        self.raw.insert(id.into(), (data, hash));
        self
    }

    /// Build the [ConstantResourceSource].
    #[inline]
    pub fn build(self) -> ConstantResourceSource {
        ConstantResourceSource::new(self.raw)
    }
}