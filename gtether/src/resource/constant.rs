use async_trait::async_trait;
use std::collections::HashMap;

use crate::resource::path::ResourcePath;
use crate::resource::source::ResourceWatcher;
use crate::resource::source::{ResourceSource, ResourceSubDataResult, SourceIndex};
use crate::resource::ResourceLoadError;

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

#[async_trait]
impl ResourceSource for ConstantResourceSource {
    async fn load(&self, id: &ResourcePath) -> ResourceSubDataResult {
        if let Some(data) = self.raw.get(id) {
            Ok(Box::new(*data).into())
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