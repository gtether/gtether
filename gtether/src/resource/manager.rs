use parking_lot::RwLock;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Weak};

use crate::resource::path::ResourcePath;
use crate::resource::{Resource, ResourceLoadError, ResourceLoader, ResourceMut};

pub trait ResourceSource {
    fn load(&self, id: &ResourcePath) -> Result<Box<dyn Read>, ResourceLoadError>;
    fn watch(&self, id: ResourcePath, sink: ResourceSink);
    fn unwatch(&self, id: &ResourcePath);
}

impl<S: ResourceSource + 'static> From<S> for Box<dyn ResourceSource> {
    #[inline]
    fn from(value: S) -> Self {
        Box::new(value)
    }
}

pub struct ResourceSink {
    manager: Weak<ResourceManager>,
    idx: usize,
}

impl ResourceSink {
    #[inline]
    pub fn update(&self, id: &ResourcePath, data: Box<dyn Read>) -> Result<(), ResourceLoadError> {
        if let Some(manager) = self.manager.upgrade() {
            manager.update(id, data, self.idx)
        } else {
            Ok(())
        }
    }
}

struct CacheEntry {
    // TODO: Move back to Weak<>
    value: Weak<dyn Any + Send + Sync>,
    update: Box<dyn Fn(Box<dyn Read>) -> Result<(), ResourceLoadError>>,
}

#[derive(Default)]
struct ResourceCache {
    inner: HashMap<ResourcePath, CacheEntry>,
}

// TODO: Add method to clear unused entries
impl ResourceCache {
    fn insert<T, L>(&mut self, id: ResourcePath, resource: &Arc<Resource<T>>, loader: L)
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T> + 'static,
    {
        let update_id = id.clone();
        let weak = Arc::downgrade(resource);
        let entry = CacheEntry {
            value: weak.clone(),
            update: Box::new(move |data| {
                // Use a weak reference here to allow the resource to be released if needed
                if let Some(strong) = weak.upgrade() {
                    let resource_mut = ResourceMut::from_resource(
                        update_id.clone(),
                        strong,
                    );
                    loader.update(resource_mut, data)
                } else {
                    Ok(())
                }
            }),
        };
        self.inner.insert(id, entry);
    }

    fn get<T>(&mut self, id: &ResourcePath)
        -> Option<Result<Arc<Resource<T>>, ResourceLoadError>>
    where
        T: Send + Sync + 'static,
    {
        if let Some(entry) = self.inner.get(id) {
            if let Some(strong) = entry.value.upgrade() {
                match strong.downcast::<Resource<T>>() {
                    Ok(resource) => Some(Ok(resource)),
                    Err(any) => Some(Err(ResourceLoadError::TypeMismatch {
                        requested: TypeId::of::<ResourceMut<T>>(),
                        actual: (&*any).type_id(),
                    })),
                }
            } else {
                // Expired, so remove the entry
                self.inner.remove(id);
                None
            }
        } else {
            None
        }
    }

    fn update(&self, id: &ResourcePath, data: Box<dyn Read>)
        -> Result<(), ResourceLoadError>
    {
        // TODO: Reject if update comes from source with lower priority
        if let Some(entry) = self.inner.get(id) {
            (entry.update)(data)
        } else {
            Ok(())
        }
    }
}

pub struct ResourceManager {
    sources: Vec<Box<dyn ResourceSource>>,
    cache: RwLock<ResourceCache>,
    weak: Weak<Self>,
}

impl ResourceManager {
    fn new(sources: Vec<Box<dyn ResourceSource>>) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            sources,
            cache: RwLock::new(ResourceCache::default()),
            weak: weak.clone(),
        })
    }

    #[inline]
    pub fn builder() -> ResourceManagerBuilder {
        ResourceManagerBuilder::default()
    }

    fn watch_n(&self, id: &ResourcePath, n: usize) {
        for (idx, source) in self.sources.iter().enumerate() {
            if idx <= n {
                source.watch(id.clone(), ResourceSink {
                    manager: self.weak.clone(),
                    idx,
                });
            } else {
                source.unwatch(id);
            }
        }
    }

    // TODO: Replace source indices with more opaque sortable key that sources can generate
    fn update(&self, id: &ResourcePath, data: Box<dyn Read>, idx: usize)
        -> Result<(), ResourceLoadError>
    {
        self.cache.read().update(id, data)?;
        self.watch_n(id, idx);
        Ok(())
    }

    fn get_data(&self, id: &ResourcePath) -> Result<Option<Box<dyn Read>>, ResourceLoadError> {
        let result = self.sources.iter().enumerate().find_map(|(idx, source)| {
            match source.load(id) {
                Ok(data) => Some((idx, Ok(Some(data)))),
                Err(ResourceLoadError::NotFound(_)) => None,
                Err(e) => Some((idx, Err(e))),
            }
        });

        if let Some((n, result)) = result {
            self.watch_n(id, n);
            Ok(result?)
        } else {
            Ok(None)
        }
    }

    pub fn get<T, L>(&self, id: impl Into<ResourcePath>, loader: L)
        -> Result<Arc<Resource<T>>, ResourceLoadError>
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T> + 'static,
    {
        let id = id.into();
        let mut cache = self.cache.write();
        if let Some(cache_result) = cache.get(&id) {
            cache_result
        } else {
            if let Some(data) = self.get_data(&id)? {
                let resource = Arc::new(Resource::new(loader.load(data)?));
                cache.insert(id, &resource, loader);
                Ok(resource)
            } else {
                Err(ResourceLoadError::NotFound(id))
            }
        }
    }
}

#[derive(Default)]
pub struct ResourceManagerBuilder {
    sources: Vec<Box<dyn ResourceSource>>,
}

impl ResourceManagerBuilder {
    #[inline]
    pub fn source(mut self, source: impl Into<Box<dyn ResourceSource>>) -> Self {
        self.sources.push(source.into());
        self
    }

    #[inline]
    pub fn build(self) -> Arc<ResourceManager> {
        ResourceManager::new(self.sources)
    }
}