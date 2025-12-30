use ahash::HashSet;
use async_trait::async_trait;

use crate::resource::id::ResourceId;
use crate::resource::source::{ResourceDataResult, ResourceSource, SourceIndex};
use crate::resource::source::{ResourceDataSource, ResourceUpdater};
use crate::resource::ResourceLoadError;

/// [ResourceSource][rs] that wraps multiple sub-sources.
///
/// This source simply contains a collection of sub-sources, to wrap many sub-sources under a single
/// top-layer source, which can be useful for structuring many nested layers of sources and adapters.
///
/// [rs]: ResourceSource
pub struct ResourceSourceList<S: ResourceSource> {
    sources: Vec<S>,
}

impl<S: ResourceSource> FromIterator<S> for ResourceSourceList<S> {
    fn from_iter<T: IntoIterator<Item=S>>(iter: T) -> Self {
        Self {
            sources: iter.into_iter().collect(),
        }
    }
}

#[async_trait]
impl<S: ResourceSource> ResourceSource for ResourceSourceList<S> {
    fn hash(&self, id: &ResourceId) -> Option<ResourceDataSource> {
        for (idx, source) in self.sources.iter().enumerate() {
            match source.hash(id) {
                Some(s) => return Some(s.wrap(idx)),
                None => {},
            }
        }
        None
    }

    async fn load(&self, id: &ResourceId) -> ResourceDataResult {
        for (idx, source) in self.sources.iter().enumerate() {
            match source.load(id).await {
                Ok(data) => return Ok(data.wrap(idx)),
                Err(ResourceLoadError::NotFound(_)) => {},
                Err(e) => return Err(e),
            }
        }
        Err(ResourceLoadError::NotFound(id.clone()))
    }

    async fn sub_load(&self, id: &ResourceId, sub_idx: &SourceIndex) -> ResourceDataResult {
        if let Some(source) = self.sources.get(sub_idx.idx) {
            match sub_idx.sub_idx() {
                Some(sub_idx) => source.sub_load(id, sub_idx).await,
                None => source.load(id).await,
            }.map(|data| data.wrap(sub_idx.idx))
        } else {
            Err(ResourceLoadError::NotFound(id.clone()))
        }
    }

    fn set_updater(&self, updater: Box<dyn ResourceUpdater>, watches: HashSet<ResourceId>) {
        for (idx, source) in self.sources.iter().enumerate() {
            source.set_updater(updater.clone_with_sub_index(idx.into()), watches.clone());
        }
    }

    fn watch(&self, id: ResourceId, sub_idx: Option<SourceIndex>) {
        let sub_idx = sub_idx.unwrap_or(SourceIndex::new(self.sources.len()));
        for (idx, source) in self.sources.iter().enumerate() {
            if idx <= sub_idx.idx() {
                source.watch(
                    id.clone(),
                    sub_idx.sub_idx().map(|inner| inner.clone())
                );
            } else {
                source.unwatch(&id);
            }
        }
    }

    fn unwatch(&self, id: &ResourceId) {
        for source in &self.sources {
            source.unwatch(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use macro_rules_attribute::apply;
    use rstest::{fixture, rstest};
    use smol_macros::test as smol_test;
    use std::sync::Arc;
    use std::time::Duration;
    use test_log::test as test_log;

    use crate::resource::manager::tests::*;
    use crate::resource::manager::ResourceManager;

    #[fixture]
    fn list_resource_manager<const N: usize>() -> (Arc<ResourceManager>, [Arc<ResourceDataMap>; N]) {
        let tuples = core::array::from_fn::<_, N, _>(|_| TestResourceSource::new());
        let data_maps = core::array::from_fn::<_, N, _>(|idx| tuples[idx].1.clone());

        let source = ResourceSourceList::from_iter(tuples.into_iter().map(|t| t.0));

        let manager = ResourceManager::builder()
            .source(source)
            .build();

        (manager, data_maps)
    }

    #[rstest]
    #[case::not_found("key", None, ExpectedLoadResult::NotFound(ResourceId::from("key")))]
    #[case::ok("key", Some(TestDataEntry::new("key", b"value", "h_value")), ExpectedLoadResult::Ok("value".to_string()))]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_list_load(
        #[from(list_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
        #[case] key: &str,
        #[case] start_data: Option<TestDataEntry>,
        #[case] expected_load_result: ExpectedLoadResult<String>,
    ) {
        if let Some(start_data) = start_data {
            data_maps[0].lock().insert(start_data.id, start_data.data, start_data.hash);
        }

        let fut = {
            let _lock = manager.test_ctx().sync_load.block().await;
            let fut = manager.get_with_loader(key, TestResLoader::new(key));
            fut.check().expect_err("Resource should not be loaded yet")
        };

        let result = fut.await;
        // Hold a reference to keep possibly loaded values from expiring
        let _maybe_value = expected_load_result.assert_matches(result);
        manager.test_ctx().sync_load.assert_count(1);

        let result = manager.get_with_loader(key, TestResLoader::new(key))
            .check()
            .expect("Result should be cached and pollable");
        expected_load_result.assert_matches(result);
        // No extra loads should have been made
        manager.test_ctx().sync_load.assert_count(1);
    }

    #[rstest]
    #[case::value_to_value(ExpectedDataEntry::ok("key"), ExpectedDataEntry::ok_new("key"))]
    #[case::value_to_error(ExpectedDataEntry::ok("key"), ExpectedDataEntry::read_error_new("key"))]
    #[case::error_to_value(ExpectedDataEntry::read_error("key"), ExpectedDataEntry::ok_new("key"))]
    #[case::error_to_error(ExpectedDataEntry::read_error("key"), ExpectedDataEntry::read_error_new("key"))]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_list_resource_manager_update(
        #[from(list_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
        #[case] start: ExpectedDataEntry<String>,
        #[case] end: ExpectedDataEntry<String>,
    ) {
        data_maps[0].lock().insert(start.entry.id.clone(), start.entry.data, start.entry.hash);

        data_maps[0].assert_watch(start.entry.id.clone(), false);
        let result = manager.get_with_loader(
            start.entry.id.clone(),
            TestResLoader::new(start.entry.id.clone()),
        ).await;
        let value = start.expected.assert_matches(result);
        data_maps[0].assert_watch(start.entry.id.clone(), true);
        manager.test_ctx().sync_update.assert_count(0);

        {
            let _lock = manager.test_ctx().sync_update.block().await;
            data_maps[0].lock().insert(end.entry.id.clone(), end.entry.data, end.entry.hash);
            if let ExpectedLoadResult::Ok(expected_value) = &start.expected {
                let value = value.as_ref().unwrap();
                assert_eq!(&*value.read(), expected_value);
            }
        }

        timeout(
            manager.test_ctx().sync_update.wait_attempts(1),
            Duration::from_millis(500),
        ).await;
        match end.expected {
            ExpectedLoadResult::Ok(expected_value) => {
                match value {
                    Some(value) => {
                        manager.test_ctx().sync_update.assert_count(1);
                        assert_eq!(*value.read(), expected_value.to_owned());
                    },
                    None => {
                        manager.test_ctx().sync_update.assert_count(0);
                        let value = manager.get_with_loader(
                            end.entry.id.clone(),
                            TestResLoader::new(end.entry.id.clone()),
                        ).await.expect(&format!("Resource should load for '{:#?}'", &end.entry.id));
                        assert_eq!(*value.read(), expected_value.to_owned());
                    },
                }
            },
            ExpectedLoadResult::NotFound(_) => unimplemented!(),
            ExpectedLoadResult::ReadErr => {
                match start.expected {
                    ExpectedLoadResult::Ok(expected_value) => {
                        manager.test_ctx().sync_update.assert_count(1);
                        let value = value.as_ref().unwrap();
                        assert_eq!(*value.read(), expected_value);
                    },
                    ExpectedLoadResult::NotFound(_) => unimplemented!(),
                    ExpectedLoadResult::ReadErr => {
                        manager.test_ctx().sync_update.assert_count(0);
                        manager.get_with_loader(
                            end.entry.id.clone(),
                            TestResLoader::new(end.entry.id.clone()),
                        ).await.expect_err(&format!("Resource should fail to load for '{:#?}'", &end.entry.id));
                    },
                }
            },
        }
        data_maps[0].assert_watch(end.entry.id.clone(), true);
    }

    #[rstest]
    #[case::insert_lower_priority(
        BulkUpdate::new(2, [Update::Insert(TestDataEntry::new("key", b"new_value_2", "h_new_value_2"))]),
        "value_1",
        0, 0, // No watchers should even be triggered, so unfortunately nothing to wait on
        [true, true, false],
    )]
    #[case::insert_same_priority(
        BulkUpdate::new(1, [Update::Insert(TestDataEntry::new("key", b"new_value_1", "h_new_value_1"))]),
        "new_value_1",
        1, 1,
        [true, true, false],
    )]
    #[case::insert_higher_priority(
        BulkUpdate::new(0, [Update::Insert(TestDataEntry::new("key", b"new_value_0", "h_new_value_0"))]),
        "new_value_0",
        1, 1,
        [true, false, false],
    )]
    #[case::remove_lower_priority(
        BulkUpdate::new(2, [Update::Remove(ResourceId::from("key"))]),
        "value_1",
        0, 0, // No watchers should even be triggered, so unfortunately nothing to wait on
        [true, true, false],
    )]
    #[case::remove_same_priority(
        BulkUpdate::new(1, [Update::Remove(ResourceId::from("key"))]),
        "value_2",
        1, 1,
        [true, true, true],
    )]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_source_order(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 3]),
        #[case] update: BulkUpdate,
        #[case] expected_value: &str,
        #[case] wait_update_attempts: usize,
        #[case] expected_update_count: usize,
        #[case] expected_watches: [bool; 3],
    ) {
        data_maps[1].lock().insert("key", b"value_1", "h_value_1");
        data_maps[2].lock().insert("key", b"value_2", "h_value_2");

        // Load should retrieve "value_1", as it's earlier in the priority chain
        let value = manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value_1".to_owned());
        manager.test_ctx().sync_update.assert_count(0);

        update.apply(&data_maps);
        timeout(
            manager.test_ctx().sync_update.wait_attempts(wait_update_attempts),
            Duration::from_millis(500),
        ).await;

        manager.test_ctx().sync_update.assert_count(expected_update_count);
        assert_eq!(*value.read(), expected_value.to_owned());

        for (idx, expected_watch) in expected_watches.into_iter().enumerate() {
            data_maps[idx].assert_watch("key", expected_watch);
        }
    }

}