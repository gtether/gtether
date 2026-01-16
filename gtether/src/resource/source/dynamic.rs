//! [ResourceSource] that dynamically wraps a changing set of sub-sources.
//!
//! The [ResourceSourceDynamic] found in this module allows dynamically changing the set of resource
//! sources at runtime. This is useful for when e.g. a user connects to a server that provides its
//! own resources, or otherwise interacts in a way that changes the set of resource sources.
//!
//! Mutating the sources of a [ResourceSourceDynamic] occurs through the cheaply cloneable [handle]
//! that can be [retrieved from a dynamic source](ResourceSourceDynamic::handle).

use async_trait::async_trait;
use educe::Educe;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::ops::Deref;
use std::sync::Arc;

use crate::resource::id::ResourceId;
use crate::resource::source::{ResourceDataResult, ResourceDataSource, ResourceSource};
use crate::resource::watcher::ResourceWatcher;
use crate::resource::ResourceLoadError;

struct ResourceSourceDynamicState<S: ResourceSource> {
    sources: RwLock<Vec<S>>,
    watcher: ResourceWatcher,
}

/// Read guard for [ResourceSourceDynamic].
///
/// Provides a read-only dereference to the `Vec<>` of children sources.
pub struct ResourceSourceDynamicReadGuard<'a, S: ResourceSource>(RwLockReadGuard<'a, Vec<S>>);

impl<'a, S: ResourceSource> Deref for ResourceSourceDynamicReadGuard<'a, S> {
    type Target = Vec<S>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

/// Write guard for [ResourceSourceDynamic].
///
/// In addition to providing a read-only dereference to the `Vec<>` of children sources, this guard
/// also provides specific `mut` methods for inserting, pushing, and removing children.
pub struct ResourceSourceDynamicWriteGuard<'a, S: ResourceSource> {
    guard: RwLockWriteGuard<'a, Vec<S>>,
    watcher: &'a ResourceWatcher,
}

impl<'a, S: ResourceSource> Deref for ResourceSourceDynamicWriteGuard<'a, S> {
    type Target = Vec<S>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl<'a, S: ResourceSource> ResourceSourceDynamicWriteGuard<'a, S> {
    /// Inserts a source at position `idx`, shifting all sources after it to the right.
    ///
    /// See also: [`Vec::insert()`].
    ///
    /// # Panics
    ///
    /// Panics if `idx` > `len`.
    #[inline]
    pub fn insert(&mut self, idx: usize, source: S) {
        self.watcher.insert_child(idx, source.watcher());
        self.guard.insert(idx, source);
    }

    /// Appends a source to the back of the collection.
    ///
    /// See also: [`Vec::push()`].
    #[inline]
    pub fn push(&mut self, source: S) {
        self.watcher.push_child(source.watcher());
        self.guard.push(source);
    }

    /// Removes the source at position `idx`, shifting all sources after it to the left.
    ///
    /// See also: [`Vec::remove()`].
    ///
    /// # Panics
    ///
    /// Panics if `idx` is out of bounds.
    #[inline]
    pub fn remove(&mut self, idx: usize) -> S {
        self.watcher.remove_child(idx);
        self.guard.remove(idx)
    }
}

impl<'a> ResourceSourceDynamicWriteGuard<'a, Box<dyn ResourceSource>> {
    /// Inserts a source dynamically.
    ///
    /// See [`Self::insert()`] for more.
    #[inline]
    pub fn insert_dyn(&mut self, idx: usize, source: impl ResourceSource) {
        self.insert(idx, Box::new(source))
    }

    /// Appends a source dynamically.
    ///
    /// See [`Self::push()`] for more.
    #[inline]
    pub fn push_dyn(&mut self, source: impl ResourceSource) {
        self.push(Box::new(source))
    }
}

/// Handle to a [ResourceSourceDynamic] that allows modification of the children resource sources.
///
/// Use the
/// [`read()`](ResourceSourceDynamicHandle::read)/[`write()`](ResourceSourceDynamicHandle::write)
/// methods to get actual access to the resource sources.
///
/// This handle is cheaply cloneable (it just clones the `Arc<>` it wraps).
#[derive(Educe)]
#[educe(Clone)]
pub struct ResourceSourceDynamicHandle<S: ResourceSource>(Arc<ResourceSourceDynamicState<S>>);

impl<S: ResourceSource> ResourceSourceDynamicHandle<S> {
    /// Get read access to the children resource sources.
    #[inline]
    pub fn read(&self) -> ResourceSourceDynamicReadGuard<'_, S> {
        ResourceSourceDynamicReadGuard(self.0.sources.read())
    }

    /// Get write access to the children resource sources.
    ///
    /// See [ResourceSourceDynamicWriteGuard] for specialized insert/push/remove methods.
    #[inline]
    pub fn write(&self) -> ResourceSourceDynamicWriteGuard<'_, S> {
        ResourceSourceDynamicWriteGuard {
            guard: self.0.sources.write(),
            watcher: &self.0.watcher,
        }
    }
}

/// [ResourceSource] that contains a dynamic sub-set of resource sources.
///
/// See [module-level](super::dynamic) documentation for more.
///
/// ```
/// use gtether::resource::source::dynamic::ResourceSourceDynamic;
///
/// let sources: ResourceSourceDynamic = Default::default();
/// ```
pub struct ResourceSourceDynamic<S: ResourceSource = Box<dyn ResourceSource>> {
    state: Arc<ResourceSourceDynamicState<S>>,
}

impl<S: ResourceSource> Default for ResourceSourceDynamic<S> {
    #[inline]
    fn default() -> Self {
        Self {
            state: Arc::new(ResourceSourceDynamicState {
                sources: RwLock::new(vec![]),
                watcher: ResourceWatcher::new(()),
            })
        }
    }
}

impl<S: ResourceSource> ResourceSourceDynamic<S> {
    /// Get a [handle](ResourceSourceDynamicHandle) that allows modifying the children resource
    /// sources.
    #[inline]
    pub fn handle(&self) -> ResourceSourceDynamicHandle<S> {
        ResourceSourceDynamicHandle(self.state.clone())
    }
}

#[async_trait]
impl<S: ResourceSource> ResourceSource for ResourceSourceDynamic<S> {
    fn hash(&self, id: &ResourceId) -> Option<ResourceDataSource> {
        for (idx, source) in self.state.sources.read().iter().enumerate() {
            match source.hash(id) {
                Some(s) => return Some(s.wrap(idx)),
                None => continue,
            }
        }
        None
    }

    async fn load(&self, id: &ResourceId) -> ResourceDataResult {
        for (idx, source) in self.state.sources.read().iter().enumerate() {
            match source.load(id).await {
                Ok(data) => return Ok(data.wrap(idx)),
                Err(ResourceLoadError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(ResourceLoadError::NotFound(id.clone()))
    }

    fn watcher(&self) -> &ResourceWatcher {
        &self.state.watcher
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use macro_rules_attribute::apply;
    use rstest::{fixture, rstest};
    use smol_macros::test as smol_test;
    use test_log::test as test_log;

    use crate::resource::manager::tests::{timeout, ExpectedDataEntry, ExpectedLoadResult, TestDataEntry, TestResLoader, TestResourceSource};
    use crate::resource::manager::ResourceManager;

    #[fixture]
    fn dynamic_resource_manager<S: ResourceSource>() -> (
        Arc<ResourceManager>,
        ResourceSourceDynamicHandle<S>,
    ) {
        let source = ResourceSourceDynamic::default();
        let handle = source.handle();

        let manager = ResourceManager::builder()
            .source(source)
            .build();

        (manager, handle)
    }

    async fn assert_manager_load(
        manager: &Arc<ResourceManager>,
        key: &str,
        expected_load_result: ExpectedLoadResult<String>,
        load_count: usize,
    ) {
        let fut = {
            let _lock = manager.test_ctx().sync_load.block().await;
            let fut = manager.get_with_loader(key, TestResLoader::new(key));
            fut.check().expect_err("Resource should not be loaded yet")
        };

        let result = fut.await;
        // Hold a reference to keep possibly loaded values from expiring
        let _maybe_value = expected_load_result.assert_matches(result);
        manager.test_ctx().sync_load.assert_count(load_count);

        let result = manager.get_with_loader(key, TestResLoader::new(key))
            .check()
            .expect("Result should be cached and pollable");
        expected_load_result.assert_matches(result);
        // No extra loads should have been made
        manager.test_ctx().sync_load.assert_count(load_count);
    }

    #[rstest]
    #[case::not_found("key", ExpectedLoadResult::NotFound(ResourceId::from("key")))]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_dynamic_load_no_sources(
        #[from(dynamic_resource_manager)] (manager, _handle): (
            Arc<ResourceManager>,
            ResourceSourceDynamicHandle<Box<dyn ResourceSource>>,
        ),
        #[case] key: &str,
        #[case] expected_load_result: ExpectedLoadResult<String>,
    ) {
        assert_manager_load(&manager, key, expected_load_result, 1).await;
    }

    #[rstest]
    #[case::not_found("key", None, ExpectedLoadResult::NotFound(ResourceId::from("key")))]
    #[case::ok("key", Some(TestDataEntry::new("key", b"value", "h_value")), ExpectedLoadResult::Ok("value".to_string()))]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_dynamic_load_insert_source_then_data(
        #[from(dynamic_resource_manager)] (manager, handle): (
            Arc<ResourceManager>,
            ResourceSourceDynamicHandle<Box<dyn ResourceSource>>,
        ),
        #[case] key: &str,
        #[case] start_data: Option<TestDataEntry>,
        #[case] expected_load_result: ExpectedLoadResult<String>,
    ) {
        let (source, data_map) = TestResourceSource::new();
        handle.write().push_dyn(source);

        if let Some(start_data) = start_data {
            data_map.lock().insert(start_data.id, start_data.data, start_data.hash);
        }

        assert_manager_load(&manager, key, expected_load_result, 1).await;
    }

    #[rstest]
    #[case::not_found("key", None, ExpectedLoadResult::NotFound(ResourceId::from("key")))]
    #[case::ok("key", Some(TestDataEntry::new("key", b"value", "h_value")), ExpectedLoadResult::Ok("value".to_string()))]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_dynamic_load_insert_data_then_source(
        #[from(dynamic_resource_manager)] (manager, handle): (
            Arc<ResourceManager>,
            ResourceSourceDynamicHandle<Box<dyn ResourceSource>>,
        ),
        #[case] key: &str,
        #[case] start_data: Option<TestDataEntry>,
        #[case] expected_load_result: ExpectedLoadResult<String>,
    ) {
        let (source, data_map) = TestResourceSource::new();

        if let Some(start_data) = start_data {
            data_map.lock().insert(start_data.id, start_data.data, start_data.hash);
        }

        handle.write().push_dyn(source);

        assert_manager_load(&manager, key, expected_load_result, 1).await;
    }

    #[rstest]
    #[case::lower_priority_value_to_value(
        false, ExpectedDataEntry::ok("key"), Some(ExpectedDataEntry::ok_new("key")),
    )]
    #[case::lower_priority_value_to_error(
        false, ExpectedDataEntry::ok("key"), Some(ExpectedDataEntry::read_error_new("key")),
    )]
    #[case::lower_priority_value_to_none(
        false, ExpectedDataEntry::ok("key"), None,
    )]
    #[case::lower_priority_error_to_value(
        false, ExpectedDataEntry::read_error("key"), Some(ExpectedDataEntry::ok_new("key")),
    )]
    #[case::lower_priority_error_to_error(
        false, ExpectedDataEntry::read_error("key"), Some(ExpectedDataEntry::read_error_new("key")),
    )]
    #[case::lower_priority_error_to_none(
        false, ExpectedDataEntry::read_error("key"), None,
    )]
    #[case::higher_priority_value_to_value(
        true, ExpectedDataEntry::ok("key"), Some(ExpectedDataEntry::ok_new("key")),
    )]
    #[case::higher_priority_value_to_error(
        true, ExpectedDataEntry::ok("key"), Some(ExpectedDataEntry::read_error_new("key")),
    )]
    #[case::higher_priority_value_to_none(
        true, ExpectedDataEntry::ok("key"), None,
    )]
    #[case::higher_priority_error_to_value(
        true, ExpectedDataEntry::read_error("key"), Some(ExpectedDataEntry::ok_new("key")),
    )]
    #[case::higher_priority_error_to_error(
        true, ExpectedDataEntry::read_error("key"), Some(ExpectedDataEntry::read_error_new("key")),
    )]
    #[case::higher_priority_error_to_none(
        true, ExpectedDataEntry::read_error("key"), None,
    )]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_dynamic_update_insert(
        #[from(dynamic_resource_manager)] (manager, handle): (
            Arc<ResourceManager>,
            ResourceSourceDynamicHandle<Box<dyn ResourceSource>>,
        ),
        #[case] insert_before: bool,
        #[case] start: ExpectedDataEntry<String>,
        #[case] end: Option<ExpectedDataEntry<String>>,
    ) {
        let (main_source, main_data_map) = TestResourceSource::new();
        main_data_map.lock().insert(start.entry.id.clone(), start.entry.data, start.entry.hash);
        handle.write().push_dyn(main_source);

        let (new_source, new_data_map) = TestResourceSource::new();
        let end = end.map(|end| {
            new_data_map.lock().insert(end.entry.id.clone(), end.entry.data, end.entry.hash);
            (end.entry.id, end.expected)
        });

        main_data_map.assert_watch(start.entry.id.clone(), false);
        let result = manager.get_with_loader(
            start.entry.id.clone(),
            TestResLoader::new(start.entry.id.clone()),
        ).await;
        let value = start.expected.assert_matches(result);
        main_data_map.assert_watch(start.entry.id.clone(), true);
        manager.test_ctx().sync_update.assert_count(0);

        {
            let _lock = manager.test_ctx().sync_update.block().await;
            if insert_before {
                handle.write().insert_dyn(0, new_source);
            } else {
                handle.write().push_dyn(new_source);
            }
            if let ExpectedLoadResult::Ok(expected_value) = &start.expected {
                let value = value.as_ref().unwrap();
                assert_eq!(&*value.read(), expected_value);
            }
        }

        if insert_before {
            timeout(
                manager.test_ctx().sync_update.wait_attempts(1),
                Duration::from_millis(500),
            ).await;

            if let Some((end_id, end_expected)) = end {
                match end_expected {
                    ExpectedLoadResult::Ok(expected_value) => {
                        match value {
                            Some(value) => {
                                manager.test_ctx().sync_update.assert_count(1);
                                assert_eq!(*value.read(), expected_value.to_owned());
                            },
                            None => {
                                manager.test_ctx().sync_update.assert_count(0);
                                let value = manager.get_with_loader(
                                    end_id.clone(),
                                    TestResLoader::new(end_id.clone()),
                                ).await.expect(&format!("Resource should load for '{:#?}'", &end_id));
                                assert_eq!(*value.read(), expected_value.to_owned());
                            },
                        }
                        new_data_map.assert_watch(end_id.clone(), true);
                        main_data_map.assert_watch(end_id.clone(), false);
                    },
                    ExpectedLoadResult::NotFound(_) => unimplemented!(),
                    ExpectedLoadResult::ReadErr => {
                        match start.expected {
                            ExpectedLoadResult::Ok(expected_value) => {
                                manager.test_ctx().sync_update.assert_count(1);
                                let value = value.as_ref().unwrap();
                                assert_eq!(*value.read(), expected_value);
                                new_data_map.assert_watch(end_id.clone(), true);
                                main_data_map.assert_watch(end_id.clone(), true);
                            },
                            ExpectedLoadResult::NotFound(_) => unimplemented!(),
                            ExpectedLoadResult::ReadErr => {
                                manager.test_ctx().sync_update.assert_count(0);
                                manager.get_with_loader(
                                    end_id.clone(),
                                    TestResLoader::new(end_id.clone()),
                                ).await.expect_err(&format!("Resource should fail to load for '{:#?}'", &end_id));
                                new_data_map.assert_watch(end_id.clone(), true);
                                main_data_map.assert_watch(end_id.clone(), false);
                            },
                        }
                    },
                }
            } else {
                manager.test_ctx().sync_update.assert_count(0);
                new_data_map.assert_watch(start.entry.id.clone(), true);
                main_data_map.assert_watch(start.entry.id.clone(), true);
            }
        } else {
            // Nothing to await on
            manager.test_ctx().sync_update.assert_count(0);
            new_data_map.assert_watch(start.entry.id.clone(), false);
            main_data_map.assert_watch(start.entry.id.clone(), true);
        }
    }

    #[rstest]
    #[case::source_0(0, "value_1", 1, 0, [None, Some(true), Some(false), Some(false)])]
    #[case::source_1(1, "value_2", 1, 1, [Some(true), None, Some(true), Some(false)])]
    #[case::source_2(2, "value_1", 0, 0, [Some(true), Some(true), None, Some(false)])]
    #[case::source_3(3, "value_1", 0, 0, [Some(true), Some(true), Some(false), None])]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_dynamic_update_remove(
        #[from(dynamic_resource_manager)] (manager, handle): (
            Arc<ResourceManager>,
            ResourceSourceDynamicHandle<Box<dyn ResourceSource>>,
        ),
        #[case] remove_idx: usize,
        #[case] expected_value: &str,
        #[case] wait_update_attempts: usize,
        #[case] expected_update_count: usize,
        #[case] expected_watches: [Option<bool>; 4],
    ) {
        let data_maps = core::array::from_fn::<_, 4, _>(|_| {
            let (source, data_map) = TestResourceSource::new();
            handle.write().push_dyn(source);
            data_map
        });

        data_maps[1].lock().insert("key", b"value_1", "h_value_1");
        data_maps[2].lock().insert("key", b"value_2", "h_value_2");

        let value = manager.get_with_loader("key", TestResLoader::new("key")).await
            .expect("resource should load");
        assert_eq!(*value.read(), "value_1".to_owned());
        manager.test_ctx().sync_update.assert_count(0);

        handle.write().remove(remove_idx);

        timeout(
            manager.test_ctx().sync_update.wait_attempts(wait_update_attempts),
            Duration::from_millis(500),
        ).await;
        manager.test_ctx().sync_update.assert_count(expected_update_count);

        assert_eq!(*value.read(), expected_value.to_owned());

        for (idx, expected_watch) in expected_watches.into_iter().enumerate() {
            if let Some(expected_watch) = expected_watch {
                data_maps[idx].assert_watch("key", expected_watch);
            }
        }
    }
}