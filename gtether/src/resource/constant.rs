use std::collections::HashMap;
use std::io::Read;

use crate::resource::manager::{ResourceSink, ResourceSource};
use crate::resource::path::ResourcePath;
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

impl ResourceSource for ConstantResourceSource {
    fn load(&self, id: &ResourcePath) -> Result<Box<dyn Read>, ResourceLoadError> {
        if let Some(data) = self.raw.get(id) {
            Ok(Box::new(*data))
        } else {
            Err(ResourceLoadError::NotFound(id.clone()))
        }
    }

    fn watch(&self, _id: ResourcePath, _sink: ResourceSink) {
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