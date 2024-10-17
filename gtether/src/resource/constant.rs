use std::collections::HashMap;

use crate::resource::manager::ResourceWatcher;
use crate::resource::path::ResourcePath;
use crate::resource::ResourceLoadError;
use crate::resource::source::{ResourceSource, ResourceSubDataResult, SourceIndex};

pub struct ConstantResourceSource {
    raw: HashMap<ResourcePath, &'static [u8]>,
}

impl ConstantResourceSource {
    #[inline]
    pub fn new(raw: HashMap<ResourcePath, &'static [u8]>) -> Self {
        Self { raw }
    }

    #[inline]
    pub fn builder() -> ConstantResourceSourceBuilder {
        ConstantResourceSourceBuilder::new()
    }
}

impl ResourceSource for ConstantResourceSource {
    fn load(&self, id: &ResourcePath) -> ResourceSubDataResult {
        if let Some(data) = self.raw.get(id) {
            Ok(Box::new(*data).into())
        } else {
            Err(ResourceLoadError::NotFound(id.clone()))
        }
    }

    fn watch(&self, _id: ResourcePath, _watcher: ResourceWatcher, _sub_idx: Option<SourceIndex>) {
        // Noop because constant data can't change
    }

    fn unwatch(&self, _id: &ResourcePath) {
        // Noop because constant data can't change
    }
}

pub struct ConstantResourceSourceBuilder {
    raw: HashMap<ResourcePath, &'static [u8]>,
}

impl ConstantResourceSourceBuilder {
    #[inline]
    pub fn new() -> Self {
        Self {
            raw: HashMap::new(),
        }
    }

    #[inline]
    pub fn resource(mut self, id: impl Into<ResourcePath>, data: &'static [u8]) -> Self {
        self.raw.insert(id.into(), data);
        self
    }

    #[inline]
    pub fn build(self) -> ConstantResourceSource {
        ConstantResourceSource::new(self.raw)
    }
}