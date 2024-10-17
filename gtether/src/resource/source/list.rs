use crate::resource::manager::ResourceWatcher;
use crate::resource::path::ResourcePath;
use crate::resource::source::{ResourceSource, ResourceSubDataResult, SourceIndex};
use crate::resource::ResourceLoadError;

pub struct ResourceSourceList {
    inner: Vec<Box<dyn ResourceSource>>,
}

impl ResourceSourceList {
    pub fn new(sources: impl IntoIterator<Item=Box<dyn ResourceSource>>) -> Self {
        Self {
            inner: sources.into_iter().collect(),
        }
    }
}

impl ResourceSource for ResourceSourceList {
    fn load(&self, id: &ResourcePath) -> ResourceSubDataResult {
        self.inner.iter().enumerate().find_map(|(idx, source)| {
            match source.load(id) {
                Ok(data) => Some(Ok(data.wrap(idx))),
                Err(ResourceLoadError::NotFound(_)) => None,
                Err(e) => Some(Err(e)),
            }
        }).unwrap_or_else(|| Err(ResourceLoadError::NotFound(id.clone())))
    }

    fn sub_load(&self, id: &ResourcePath, sub_idx: &SourceIndex) -> ResourceSubDataResult {
        if let Some(source) = self.inner.get(sub_idx.idx) {
            match sub_idx.sub_idx() {
                Some(sub_idx) => source.sub_load(id, sub_idx),
                None => source.load(id),
            }.map(|data| data.wrap(sub_idx.idx))
        } else {
            Err(ResourceLoadError::NotFound(id.clone()))
        }
    }

    fn watch(&self, id: ResourcePath, watcher: ResourceWatcher, sub_idx: Option<SourceIndex>) {
        let sub_idx = sub_idx.unwrap_or(SourceIndex::new(self.inner.len()));
        for (idx, source) in self.inner.iter().enumerate() {
            if idx <= sub_idx.idx() {
                source.watch(
                    id.clone(),
                    watcher.clone_with_sub_index(idx),
                    sub_idx.sub_idx().map(|inner| inner.clone())
                );
            } else {
                source.unwatch(&id);
            }
        }
    }

    fn unwatch(&self, id: &ResourcePath) {
        for source in &self.inner {
            source.unwatch(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::resource::manager::tests::*;
    use crate::resource::manager::ResourceManager;

    pub fn create_list_resource_manager<const N: usize>() -> (Arc<ResourceManager>, [Arc<ResourceDataMap>; N]) {
        let sub_sources = core::array::from_fn::<_, N, _>(|_| TestResourceSource::new());
        let data_maps = core::array::from_fn(|i| sub_sources[i].data_map().clone());
        let source_list = ResourceSourceList::new(sub_sources.into_iter()
            .map(|source| Box::new(source) as Box<dyn ResourceSource>));
        let manager = ResourceManager::builder()
            .source(source_list)
            .build();
        (manager, data_maps)
    }

    #[test]
    fn test_list_resource_manager_found() {
        let (manager, data_maps) = create_list_resource_manager::<1>();
        data_maps[0].insert("key", b"value");

        let (loader, sync) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader);
        assert_matches!(result.get(), None);

        sync.notify_load().wait();
        let value = result.unwrap()
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());
    }

    #[test]
    fn test_list_resource_manager_update() {
        let (manager, data_maps) = create_list_resource_manager::<1>();
        data_maps[0].insert("key", b"value");

        let (loader, sync) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader);
        sync.notify_load().wait();
        let value = result.unwrap()
            .expect("Resource should load for 'key'");

        data_maps[0].insert("key", b"new_value");
        assert_eq!(*value.read(), "value");
        sync.notify_update().wait();
        assert_eq!(*value.read(), "new_value");
    }

    #[test]
    fn test_list_resource_manager_priorities() {
        let (manager, data_maps) = create_list_resource_manager::<3>();
        data_maps[1].insert("key", b"value_1");
        data_maps[2].insert("key", b"value_2");

        // Load should retrieve "value_1", as it's earlier in the priority chain
        let (loader, sync) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader);
        sync.notify_load().wait();
        let value = result.unwrap()
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value_1".to_owned());

        // Updating lower priority shouldn't change the value
        data_maps[2].insert("key", b"new_value_2");
        // TODO: Need some way to wait for "possible" update - may need to add test-only sync logic
        //  in the actual data structures
        assert_eq!(*value.read(), "value_1".to_owned());

        // Updating same priority *should* change the value
        data_maps[1].insert("key", b"new_value_1");
        sync.notify_update().wait();
        assert_eq!(*value.read(), "new_value_1");

        // Updating higher priority *should* change the value
        data_maps[0].insert("key", b"new_value_0");
        sync.notify_update().wait();
        assert_eq!(*value.read(), "new_value_0");

        // Updating previously watched source shouldn't change the value
        data_maps[1].insert("key", b"new_new_value_1");
        // TODO: sync
        assert_eq!(*value.read(), "new_value_0");
    }

}