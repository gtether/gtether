use async_trait::async_trait;

use crate::resource::path::ResourcePath;
use crate::resource::source::ResourceWatcher;
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

#[async_trait]
impl ResourceSource for ResourceSourceList {
    async fn load(&self, id: &ResourcePath) -> ResourceSubDataResult {
        for (idx, source) in self.inner.iter().enumerate() {
            match source.load(id).await {
                Ok(data) => return Ok(data.wrap(idx)),
                Err(ResourceLoadError::NotFound(_)) => {},
                Err(e) => return Err(e),
            }
        }
        Err(ResourceLoadError::NotFound(id.clone()))
    }

    async fn sub_load(&self, id: &ResourcePath, sub_idx: &SourceIndex) -> ResourceSubDataResult {
        if let Some(source) = self.inner.get(sub_idx.idx) {
            match sub_idx.sub_idx() {
                Some(sub_idx) => source.sub_load(id, sub_idx).await,
                None => source.load(id).await,
            }.map(|data| data.wrap(sub_idx.idx))
        } else {
            Err(ResourceLoadError::NotFound(id.clone()))
        }
    }

    fn watch(&self, id: ResourcePath, watcher: Box<dyn ResourceWatcher>, sub_idx: Option<SourceIndex>) {
        let sub_idx = sub_idx.unwrap_or(SourceIndex::new(self.inner.len()));
        for (idx, source) in self.inner.iter().enumerate() {
            if idx <= sub_idx.idx() {
                source.watch(
                    id.clone(),
                    watcher.clone_with_sub_index(idx.into()),
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

    use crate::resource::manager::tests::*;
    use crate::resource::manager::{LoadPriority, ResourceManager};
    use smol::future;
    use std::sync::Arc;
    use std::time::Duration;

    pub fn create_list_resource_manager<const N: usize>() -> (Arc<ResourceManager>, [Arc<ResourceDataMap>; N]) {
        let sub_sources = core::array::from_fn::<_, N, _>(|_| TestResourceSource::default());
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

        let fut = manager.get_or_load("key", TestResourceLoader::default(), LoadPriority::Immediate);
        let value = future::block_on(timeout(fut.wait(), Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());
        manager.test_ctx().sync_load.assert_count(1);
    }

    #[test]
    fn test_list_resource_manager_update() {
        let (manager, data_maps) = create_list_resource_manager::<1>();
        data_maps[0].insert("key", b"value");

        data_maps[0].assert_watch("key", false);
        let fut = manager.get_or_load("key", TestResourceLoader::default(), LoadPriority::Immediate);
        let value = future::block_on(timeout(fut.wait(), Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        data_maps[0].assert_watch("key", true);
        manager.test_ctx().sync_update.assert_count(0);

        {
            let _lock = future::block_on(manager.test_ctx().sync_update.block(Some(Duration::from_secs(1))));
            data_maps[0].insert("key", b"new_value");
            assert_eq!(*value.read(), "value".to_owned());
        }

        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        assert_eq!(*value.read(), "new_value".to_owned());
        data_maps[0].assert_watch("key", true);
    }

    #[test]
    fn test_list_resource_manager_priorities() {
        let (manager, data_maps) = create_list_resource_manager::<3>();
        data_maps[1].insert("key", b"value_1");
        data_maps[2].insert("key", b"value_2");

        // Load should retrieve "value_1", as it's earlier in the priority chain
        let fut = manager.get_or_load("key", TestResourceLoader::default(), LoadPriority::Immediate);
        let value = future::block_on(timeout(fut.wait(), Duration::from_secs(1)))
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value_1".to_owned());
        manager.test_ctx().sync_update.assert_count(0);
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", true);
        data_maps[2].assert_watch("key", false);

        // Updating lower priority shouldn't change the value
        data_maps[2].insert("key", b"new_value_2");
        // This shouldn't even trigger a watch, so there is no mechanism to wait on, unfortunately
        assert_eq!(*value.read(), "value_1".to_owned());
        manager.test_ctx().sync_update.assert_count(0);
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", true);
        data_maps[2].assert_watch("key", false);

        // Updating same priority *should* change the value
        data_maps[1].insert("key", b"new_value_1");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(1), Duration::from_secs(1)));
        assert_eq!(*value.read(), "new_value_1");
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", true);
        data_maps[2].assert_watch("key", false);

        // Updating higher priority *should* change the value - and also what watchers are active
        data_maps[0].insert("key", b"new_value_0");
        future::block_on(timeout(manager.test_ctx().sync_update.wait_count(2), Duration::from_secs(1)));
        assert_eq!(*value.read(), "new_value_0");
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", false);
        data_maps[2].assert_watch("key", false);

        // Updating previously watched source shouldn't change the value
        data_maps[1].insert("key", b"new_new_value_1");
        // This shouldn't even trigger a watch, so there is no mechanism to wait on, unfortunately
        assert_eq!(*value.read(), "new_value_0");
        data_maps[0].assert_watch("key", true);
        data_maps[1].assert_watch("key", false);
        data_maps[2].assert_watch("key", false);
    }

}