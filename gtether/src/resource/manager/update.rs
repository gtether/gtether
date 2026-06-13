use crate::resource::manager::dependency::DependencyGraph;
use crate::resource::manager::load::{Cache, CacheGroupUpdateResult};
use crate::resource::manager::source::Sources;
use crate::resource::watcher::{ResourceWatcherReceiver, ResourceWatcherSender, UpdateMsg};

use ahash::HashSet;
use parking_lot::RwLock;
use smol::future::FutureExt;
use smol::{channel as sc, future};
use std::sync::Arc;
use std::thread::JoinHandle;
use tracing::trace;

#[cfg(test)]
pub struct UpdateManagerSyncContext {
    pub load: Arc<super::tests::SyncContext<crate::resource::BoxedResKey>>,
    pub update_block: Arc<super::tests::SyncContext<()>>,
    pub update: Arc<super::tests::SyncContext<crate::resource::BoxedResKey>>,
}

pub struct UpdateLock {
    start_load: smol::lock::RwLock<()>,
    #[cfg(test)]
    pub sync: Arc<UpdateManagerSyncContext>,
}

impl UpdateLock {
    fn new(
        #[cfg(test)]
        sync: Arc<UpdateManagerSyncContext>,
    ) -> Self {
        Self {
            start_load: smol::lock::RwLock::new(()),
            #[cfg(test)]
            sync,
        }
    }

    pub async fn start_load(&self) -> smol::lock::RwLockReadGuard<'_, ()> {
        self.start_load.read().await
    }
}

enum State {
    Init {
        receiver: ResourceWatcherReceiver,
        stop_recv: sc::Receiver<()>,
    },
    Started {
        join_handle: JoinHandle<()>,
    },
    Stopped,
}

pub struct UpdateManager {
    sender: ResourceWatcherSender,
    stop_send: sc::Sender<()>,
    state: State,
}

impl UpdateManager {
    pub fn new() -> Self {
        let (sender, receiver) = crate::resource::watcher::update_channel();
        let (stop_send, stop_recv) = sc::unbounded();
        Self {
            sender,
            stop_send,
            state: State::Init { receiver, stop_recv },
        }
    }

    pub fn start(
        &mut self,
        sources: Arc<Sources>,
        cache: Arc<Cache>,
        dependencies: Arc<RwLock<DependencyGraph>>,
        #[cfg(test)]
        sync: Arc<UpdateManagerSyncContext>,
    ) {
        let mut state = State::Stopped;
        std::mem::swap(&mut state, &mut self.state);
        match state {
            State::Init { receiver, stop_recv } => {
                let join_handle = std::thread::Builder::new()
                    .name("resource-update-manager".to_string())
                    .spawn(move || UpdateManagerThread::new(
                        receiver,
                        stop_recv,
                        sources,
                        cache,
                        dependencies,
                        #[cfg(test)]
                        sync,
                    ).run())
                    .unwrap();
                self.state = State::Started { join_handle };
            },
            _ => panic!("Can't start already started UpdateManager"),
        }
    }

    pub fn stop(&mut self) {
        let mut state = State::Stopped;
        std::mem::swap(&mut state, &mut self.state);
        match state {
            State::Started { join_handle } => {
                // If it's already closed, that's fine
                let _ = self.stop_send.send_blocking(());
                join_handle.join().unwrap();
            },
            _ => {},
        }
    }

    #[inline]
    pub fn watcher_sender(&self) -> &ResourceWatcherSender {
        &self.sender
    }
}

impl Drop for UpdateManager {
    fn drop(&mut self) {
        self.stop();
    }
}

struct UpdateManagerThread {
    receiver: ResourceWatcherReceiver,
    stop_recv: sc::Receiver<()>,
    sources: Arc<Sources>,
    cache: Arc<Cache>,
    dependencies: Arc<RwLock<DependencyGraph>>,
    #[cfg(test)]
    sync: Arc<UpdateManagerSyncContext>,
}

impl UpdateManagerThread {
    fn new(
        receiver: ResourceWatcherReceiver,
        stop_recv: sc::Receiver<()>,
        sources: Arc<Sources>,
        cache: Arc<Cache>,
        dependencies: Arc<RwLock<DependencyGraph>>,
        #[cfg(test)]
        sync: Arc<UpdateManagerSyncContext>,
    ) -> Self {
        Self {
            receiver,
            stop_recv,
            sources,
            cache,
            dependencies,
            #[cfg(test)]
            sync,
        }
    }

    fn run(self) {
        future::block_on(
            self.loop_stop()
                .or(self.loop_receive())
        );
    }

    async fn loop_stop(&self) {
        // If we get any response, either from an error from the sender being dropped or an actual
        // message, then it's time to stop/exit
        let _ = self.stop_recv.recv().await;
    }

    async fn loop_receive(&self) {
        'exit: loop {
            if let Ok(msg) = self.receiver.recv().await {
                #[cfg(test)]
                let _sync_block_lock = self.sync.update_block.run(()).await;

                let UpdateMsg {
                    src_idx: source_idx,
                    bulk_update,
                } = msg;

                let update_lock = Arc::new(UpdateLock::new(
                    #[cfg(test)]
                    self.sync.clone(),
                ));
                let start_lock = update_lock.start_load.write().await;

                let mut dependent_update_keys = HashSet::default();
                let mut queued_update_keys = HashSet::default();
                'updates: for (id, update) in bulk_update.updates {
                    trace!(%id, %source_idx, ?update, "Processing update");

                    let (keys, deps) = {
                        let dependency_graph = self.dependencies.read();
                        let keys = dependency_graph.keys_for_id(id)
                            .map(|k| k.clone())
                            .collect::<HashSet<_>>();
                        let deps = dependency_graph.nodes_for_id(id)
                            .map(|node| {
                                node.dependents().recursive()
                                    .map(|parent| parent.key().to_box())
                            })
                            .flatten()
                            .collect::<HashSet<_>>();
                        (keys, deps)
                    };

                    let cache_entries = self.cache.entries_with_group(id).await;
                    let group_lock = cache_entries.group(id).unwrap();
                    let mut group = group_lock.write().await;

                    let result = group.start_update(
                        id,
                        update,
                        &source_idx,
                        &self.sources,
                        &update_lock,
                        &self.cache,
                    );
                    match result {
                        CacheGroupUpdateResult::Skipped | CacheGroupUpdateResult::Moved => {
                            queued_update_keys.extend(keys);
                            continue 'updates
                        },
                        CacheGroupUpdateResult::Updated(keys) => {
                            trace!(%id, %source_idx, ?keys, ?deps, "Queueing updates");
                            queued_update_keys.extend(keys);
                            dependent_update_keys.extend(deps);
                        }
                    }
                }

                'updates: for key in dependent_update_keys.difference(&queued_update_keys) {
                    trace!(?key, %source_idx, "Processing triggered dependent update");
                    let id = key.id();

                    let cache_entries = self.cache.entries().read().await;
                    let group_lock = match cache_entries.group(id) {
                        Some(group) => group,
                        // No entry in the cache, this update will be ignored
                        None => {
                            continue 'updates
                        }
                    };
                    let mut group = group_lock.write().await;

                    let result = group.start_dependent_update(
                        key,
                        &update_lock,
                        &self.cache,
                    );
                    match result {
                        CacheGroupUpdateResult::Skipped | CacheGroupUpdateResult::Moved => {
                            continue 'updates
                        },
                        CacheGroupUpdateResult::Updated(keys) => {
                            trace!(?keys, %source_idx, "Queueing dependant updates");
                        }
                    }
                }

                // Start all updates
                drop(start_lock);
            } else {
                // Sender is closed, so this thread is abandoned. Start exiting
                break 'exit
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::ops::RangeBounds;
    use ahash::HashSet;
    use macro_rules_attribute::apply;
    use rstest::rstest;
    use smol_macros::test as smol_test;
    use std::time::Duration;
    use test_log::test as test_log;

    use crate::resource::id::ResourceId;
    use crate::resource::manager::dependency::DependencyGraph;
    use crate::resource::manager::tests::prelude::*;
    use crate::resource::{res_key, BoxedResKey, IdentityLoader, ResourceLoader};

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_manager_can_drop_while_updating(
        test_resource_ctx: TestResourceContext<1>,
    ) {
        test_resource_ctx.data_maps[0].lock().insert("key", b"value", "h_value");

        let _value = test_resource_ctx.manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("Resource should load for 'key'");

        let sync_update_block = test_resource_ctx.manager.test_ctx().sync_update_block.clone();
        let _lock = sync_update_block.block().await;
        test_resource_ctx.data_maps[0].lock().insert("key", b"new_value", "h_new_value");

        timeout(
            assert_manager_drops(test_resource_ctx.manager),
            Duration::from_secs(5),
        ).await;
    }

    #[rstest]
    #[case::value_to_value(
        TestDataEntry::new("key", b"value", "h_value"),
        ExpectedLoadInfo {
            load_info: TestLoadInfo::new("key", TestResLoader::new("key")),
            expected: ExpectedLoadResult::Ok(String::from("value")),
        },
        TestDataEntry::new("key", b"new_value", "h_new_value"),
        ExpectedLoadResult::Ok(String::from("new_value")),
        1, 2, true,
    )]
    #[case::value_to_error(
        TestDataEntry::new("key", b"value", "h_value"),
        ExpectedLoadInfo {
            load_info: TestLoadInfo::new("key", TestResLoader::new("key")),
            expected: ExpectedLoadResult::Ok(String::from("value")),
        },
        TestDataEntry::new("key", b"new_\xC0", "h_new_invalid"),
        ExpectedLoadResult::ReadErr,
        1, 2, true,
    )]
    #[case::error_to_value(
        TestDataEntry::new("key", b"\xC0", "h_invalid"),
        ExpectedLoadInfo {
            load_info: TestLoadInfo::new("key", TestResLoader::new("key")),
            expected: ExpectedLoadResult::ReadErr,
        },
        TestDataEntry::new("key", b"new_value", "h_new_value"),
        ExpectedLoadResult::Ok(String::from("new_value")),
        0, 2, true,
    )]
    #[case::error_to_error(
        TestDataEntry::new("key", b"\xC0", "h_invalid"),
        ExpectedLoadInfo {
            load_info: TestLoadInfo::new("key", TestResLoader::new("key")),
            expected: ExpectedLoadResult::<String>::ReadErr,
        },
        TestDataEntry::new("key", b"new_\xC0", "h_new_invalid"),
        ExpectedLoadResult::ReadErr,
        0, 2, true,
    )]
    #[case::virtual_value_to_value(
        TestDataEntry::new("key", b"value", "h_value"),
        ExpectedLoadInfo {
            load_info: TestLoadInfo::new("key", TestVirtualLoader::new(String::from("value"))),
            expected: ExpectedLoadResult::Ok(String::from("value")),
        },
        TestDataEntry::new("key", b"new_value", "h_new_value"),
        ExpectedLoadResult::Ok(String::from("value")),
        0, 0, false,
    )]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_update<L, A>(
        test_resource_ctx: TestResourceContext<1>,
        #[case] start_data: TestDataEntry,
        #[case] start_expected: ExpectedLoadInfo<L, A>,
        #[case] end_data: TestDataEntry,
        #[case] end_expected: ExpectedLoadResult<A>,
        #[case] expected_update_count: usize,
        #[case] expected_data_load_count: usize,
        #[case] expected_should_watch: bool,
    )
    where
        L: ResourceLoader + Clone,
        L::Output: Debug + Sized,
        A: Debug + PartialEq<L::Output>,
        <L as ResourceLoader>::Output: PartialEq<A>,
    {
        let key = res_key(start_expected.load_info.id, &start_expected.load_info.loader);
        {
            let mut data_map = test_resource_ctx.data_maps[0].lock();
            data_map.insert(start_data.id, start_data.data, start_data.hash);
            data_map.set_trigger_update_override(start_data.id, true);
        }

        test_resource_ctx.data_maps[0].assert_watch(start_expected.load_info.id, false);
        let result = test_resource_ctx.manager.get_with_loader(
            start_expected.load_info.id,
            start_expected.load_info.loader.clone(),
        ).await;
        let value = start_expected.expected.assert_matches(result);
        test_resource_ctx.data_maps[0].assert_watch(start_expected.load_info.id, expected_should_watch);
        test_resource_ctx.manager.test_ctx().sync_update_block.assert_count(&(), 0);

        {
            let _lock = test_resource_ctx.manager.test_ctx().sync_update_block.block().await;
            test_resource_ctx.data_maps[0].lock().insert(end_data.id, end_data.data, end_data.hash);
            if let ExpectedLoadResult::Ok(expected_value) = &start_expected.expected {
                let value = value.as_ref().unwrap();
                assert_eq!(&*value.read(), expected_value);
            }
        }

        timeout(async {
            test_resource_ctx.manager.test_ctx().sync_update_block.wait_count(&(), 1).await;
            test_resource_ctx.manager.test_ctx().sync_update.wait_attempts(&key, 1).await;
        }, Duration::from_millis(500)).await;
        test_resource_ctx.manager.test_ctx().sync_update.assert_count(&key, expected_update_count);
        match end_expected {
            ExpectedLoadResult::Ok(expected_value) => {
                match value {
                    Some(value) => {
                        assert_eq!(*value.read(), expected_value);
                    },
                    None => {
                        let value = test_resource_ctx.manager.get_with_loader(
                            start_expected.load_info.id,
                            start_expected.load_info.loader.clone(),
                        ).await.expect(&format!("Resource should load for '{}'", start_expected.load_info.id));
                        assert_eq!(*value.read(), expected_value);
                    },
                }
            },
            ExpectedLoadResult::NotFound(_) => unimplemented!(),
            ExpectedLoadResult::ReadErr => {
                match start_expected.expected {
                    ExpectedLoadResult::Ok(expected_value) => {
                        let value = value.as_ref().unwrap();
                        assert_eq!(*value.read(), expected_value);
                    },
                    ExpectedLoadResult::NotFound(_) => unimplemented!(),
                    ExpectedLoadResult::ReadErr => {
                        test_resource_ctx.manager.get_with_loader(
                            start_expected.load_info.id,
                            start_expected.load_info.loader.clone(),
                        ).await.expect_err(&format!("Resource should fail to load for '{}'", start_expected.load_info.id));
                    },
                }
            },
        }
        test_resource_ctx.data_maps[0].assert_load_count(start_expected.load_info.id, expected_data_load_count);
        test_resource_ctx.data_maps[0].assert_watch(start_expected.load_info.id, expected_should_watch);
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
    async fn test_update_source_order(
        test_resource_ctx: TestResourceContext<3>,
        #[case] update: BulkUpdate,
        #[case] expected_value: &str,
        #[case] wait_update_attempts: usize,
        #[case] expected_update_count: usize,
        #[case] expected_watches: [bool; 3],
    ) {
        let key = res_key("key", &TestResLoader::new("key"));
        test_resource_ctx.data_maps[1].lock().insert("key", b"value_1", "h_value_1");
        test_resource_ctx.data_maps[2].lock().insert("key", b"value_2", "h_value_2");

        // Load should retrieve "value_1", as it's earlier in the priority chain
        let value = test_resource_ctx.manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value_1".to_owned());
        test_resource_ctx.manager.test_ctx().sync_update_block.assert_count(&(), 0);
        test_resource_ctx.manager.test_ctx().sync_update.assert_count(&key, 0);

        update.apply(&test_resource_ctx.data_maps);
        timeout(async {
            test_resource_ctx.manager.test_ctx().sync_update_block.wait_count(&(), wait_update_attempts).await;
            test_resource_ctx.manager.test_ctx().sync_update.wait_attempts(&key, wait_update_attempts).await;
        }, Duration::from_millis(500)).await;

        test_resource_ctx.manager.test_ctx().sync_update.assert_count(&key, expected_update_count);
        assert_eq!(*value.read(), expected_value.to_owned());

        for (idx, expected_watch) in expected_watches.into_iter().enumerate() {
            test_resource_ctx.data_maps[idx].assert_watch("key", expected_watch);
        }
    }

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_update_multiple_types(
        test_resource_ctx: TestResourceContext<1>,
    ) {
        test_resource_ctx.data_maps[0].lock().insert("key", b"value", "h_value");

        let string_loader_key = res_key("key", &TestResLoader::new("key"));
        let string_value = test_resource_ctx.manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("String resource should load for 'key'");
        assert_eq!(*string_value.read(), "value".to_owned());

        let id_loader_key = res_key("key", &IdentityLoader);
        let id_value = test_resource_ctx.manager.get::<ResourceId>("key")
            .await.expect("ResourceId resource should load for 'key'");
        assert_eq!(*id_value.read(), ResourceId::from("key"));

        test_resource_ctx.manager.test_ctx().sync_update_block.assert_count(&(), 0);
        test_resource_ctx.manager.test_ctx().sync_update.assert_count(&string_loader_key, 0);
        test_resource_ctx.manager.test_ctx().sync_update.assert_count(&id_loader_key, 0);

        test_resource_ctx.data_maps[0].lock().insert("key", b"new_value", "h_new_value");
        timeout(async {
            test_resource_ctx.manager.test_ctx().sync_update_block.wait_count(&(), 1).await;
            test_resource_ctx.manager.test_ctx().sync_update.wait_attempts(&string_loader_key, 1).await;
            test_resource_ctx.manager.test_ctx().sync_update.wait_attempts(&id_loader_key, 1).await;
        }, Duration::from_millis(500)).await;
        test_resource_ctx.manager.test_ctx().sync_update.assert_count(&string_loader_key, 1);
        test_resource_ctx.manager.test_ctx().sync_update.assert_count(&id_loader_key, 1);

        assert_eq!(*string_value.read(), "new_value".to_owned());
        assert_eq!(*id_value.read(), ResourceId::from("key"));
    }

    #[rstest]
    #[case::same("h_value", 0, "value")]
    #[case::different("h_new_value", 1, "new_value")]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_update_hashing(
        test_resource_ctx: TestResourceContext<1>,
        #[case] new_hash: &str,
        #[case] expected_update_count: usize,
        #[case] expected_value: &str,
    ) {
        let key = res_key("key", &TestResLoader::new("key"));
        test_resource_ctx.data_maps[0].lock().insert("key", b"value", "h_value");

        let value = test_resource_ctx.manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());

        test_resource_ctx.data_maps[0].lock().insert("key", b"new_value", new_hash);
        timeout(async {
            test_resource_ctx.manager.test_ctx().sync_update_block.wait_count(&(), 1).await;
            test_resource_ctx.manager.test_ctx().sync_update.wait_count(&key, expected_update_count).await;
        }, Duration::from_millis(500)).await;
        test_resource_ctx.manager.test_ctx().sync_update.assert_count(&key, expected_update_count);
        assert_eq!(*value.read(), expected_value.to_owned());
    }

    #[rstest]
    #[case::single_value(
        DependencyGraph::builder()
            .add(res_key("a", &TestResChainLoader), [res_key("b", &TestResChainLoader)])
            .build(),
        [TestDataEntry::new("b", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", TestResChainLoader::default()),
            expected: ExpectedLoadResult::Ok(vec!["a:b:value"]),
        }],
        BulkUpdate::new(0, [Update::Insert(TestDataEntry::new("b", b"new_value", "h_new_value"))]),
        [vec!["a:b:new_value"]],
        [
            (res_key("a", &TestResChainLoader), 1..=1),
            (res_key("b", &TestResChainLoader), 1..=1),
        ],
        [("a", 2), ("b", 2)],
        DependencyGraph::builder()
            .add(res_key("a", &TestResChainLoader), [res_key("b", &TestResChainLoader)])
            .build(),
    )]
    #[case::single_swap(
        DependencyGraph::builder()
            .add(res_key("a", &TestResChainLoader), [res_key("b", &TestResChainLoader)])
            .build(),
        [TestDataEntry::new("b", b"value", "h_value"), TestDataEntry::new("c", b"other_value", "h_other_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", TestResChainLoader::default()),
            expected: ExpectedLoadResult::Ok(vec!["a:b:value"]),
        }],
        BulkUpdate::new(0, [Update::Insert(TestDataEntry::new("a", b"c", "h_new_a_dep"))]),
        [vec!["a:c:other_value"]],
        [
            (res_key("a", &TestResChainLoader), 1..=1),
            (res_key("b", &TestResChainLoader), 0..=0),
            (res_key("c", &TestResChainLoader), 1..=1),
        ],
        [("a", 2), ("b", 1), ("c", 1)],
        DependencyGraph::builder()
            .add(res_key("a", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .build(),
    )]
    #[case::single_gap(
        DependencyGraph::builder()
            .add(res_key("a", &TestResChainLoader), [res_key("b", &TestResChainLoader)])
            .add(res_key("b", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .build(),
        [TestDataEntry::new("c", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", TestResChainLoader::default()),
            expected: ExpectedLoadResult::Ok(vec!["a:b:c:value"]),
        }],
        BulkUpdate::new(0, [Update::Insert(TestDataEntry::new("c", b"new_value", "h_new_value"))]),
        [vec!["a:b:c:new_value"]],
        [
            (res_key("a", &TestResChainLoader), 1..=1),
            (res_key("b", &TestResChainLoader), 1..=1),
            (res_key("c", &TestResChainLoader), 1..=1),
        ],
        [("a", 2), ("b", 2), ("c", 2)],
        DependencyGraph::builder()
            .add(res_key("a", &TestResChainLoader), [res_key("b", &TestResChainLoader)])
            .add(res_key("b", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .build(),
    )]
    #[case::single_virtual(
        DependencyGraph::builder().build(),
        [TestDataEntry::new("b", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", ref_loader!(VRef("b"):TestResLoader::new("b"))),
            expected: ExpectedLoadResult::Ok(ExpectedResourceRef::new(vec!["value"])),
        }],
        BulkUpdate::new(0, [Update::Insert(TestDataEntry::new("b", b"new_value", "h_new_value"))]),
        [ExpectedResourceRef::new(vec!["new_value"])],
        [
            (res_key("a", &ref_loader!(VRef("b"):TestResLoader::new("b"))), 1..=1),
            (res_key("b", &TestResLoader::new("b")), 1..=1),
        ],
        [("a", 0), ("b", 2)],
        DependencyGraph::builder()
            .add(res_key("a", &ref_loader!(VRef("b"):TestResLoader::new("b"))), [
                res_key("b", &TestResLoader::new("b"))
            ])
            .build(),
    )]
    #[case::tree(
        DependencyGraph::builder()
            .add(res_key("a1", &TestResChainLoader), [
                res_key("b1", &TestResChainLoader),
                res_key("b2", &TestResChainLoader),
            ])
            .add(res_key("a2", &TestResChainLoader), [
                res_key("b1", &TestResChainLoader),
                res_key("b2", &TestResChainLoader),
            ])
            .add(res_key("b1", &TestResChainLoader), [
                res_key("c1", &TestResChainLoader),
                res_key("c2", &TestResChainLoader),
            ])
            .add(res_key("b2", &TestResChainLoader), [
                res_key("c2", &TestResChainLoader),
                res_key("c3", &TestResChainLoader),
            ])
            .build(),
        [
            TestDataEntry::new("c1", b"value1", "h_c1"),
            TestDataEntry::new("c2", b"value2", "h_c2"),
            TestDataEntry::new("c3", b"value3", "h_c3"),
        ],
        [
            ExpectedLoadInfo {
                load_info: TestLoadInfo::new("a1", TestResChainLoader::default()),
                expected: ExpectedLoadResult::Ok(vec![
                    "a1:b1:c1:value1", "a1:b1:c2:value2",
                    "a1:b2:c2:value2", "a1:b2:c3:value3",
                ]),
            },
            ExpectedLoadInfo {
                load_info: TestLoadInfo::new("a2", TestResChainLoader::default()),
                expected: ExpectedLoadResult::Ok(vec![
                    "a2:b1:c1:value1", "a2:b1:c2:value2",
                    "a2:b2:c2:value2", "a2:b2:c3:value3",
                ]),
            },
        ],
        BulkUpdate::new(0, [
            Update::Insert(TestDataEntry::new("a1", b"b1", "h_new_a1")),
            Update::Insert(TestDataEntry::new("b2", b"c1:c2:c3", "h_new_b2")),
            Update::Insert(TestDataEntry::new("c2", b"new_value2", "h_new_c2")),
            Update::Insert(TestDataEntry::new("c3", b"new_value3", "h_new_c3")),
        ]),
        [
            vec!["a1:b1:c1:value1", "a1:b1:c2:new_value2"],
            vec![
                "a2:b1:c1:value1", "a2:b1:c2:new_value2",
                "a2:b2:c1:value1", "a2:b2:c2:new_value2", "a2:b2:c3:new_value3",
            ],
        ],
        [
            (res_key("a1", &TestResChainLoader), 1..=1),
            (res_key("a2", &TestResChainLoader), 1..=1),
            (res_key("b1", &TestResChainLoader), 1..=2),
            (res_key("b2", &TestResChainLoader), 1..=1),
            (res_key("c1", &TestResChainLoader), 1..=2),
            (res_key("c2", &TestResChainLoader), 1..=2),
            (res_key("c3", &TestResChainLoader), 1..=1),
        ],
        // just need some to keep typing happy, but we honestly don't care about the rest as the
        // above check handles it
        [("a1", 2), ("a2", 2)],
        DependencyGraph::builder()
            .add(res_key("a1", &TestResChainLoader), [res_key("b1", &TestResChainLoader)])
            .add(res_key("a2", &TestResChainLoader), [
                res_key("b1", &TestResChainLoader),
                res_key("b2", &TestResChainLoader),
            ])
            .add(res_key("b1", &TestResChainLoader), [
                res_key("c1", &TestResChainLoader),
                res_key("c2", &TestResChainLoader),
            ])
            .add(res_key("b2", &TestResChainLoader), [
                res_key("c1", &TestResChainLoader),
                res_key("c2", &TestResChainLoader),
                res_key("c3", &TestResChainLoader),
            ])
            .build(),
    )]
    #[case::diamond(
        DependencyGraph::builder()
            .add(res_key("a", &TestResChainLoader), [
                res_key("b1", &TestResChainLoader),
                res_key("b2", &TestResChainLoader),
                res_key("b3", &TestResChainLoader),
            ])
            .add(res_key("b1", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .add(res_key("b2", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .add(res_key("b3", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .build(),
        [TestDataEntry::new("c", b"value", "h_c")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", TestResChainLoader::default()),
            expected: ExpectedLoadResult::Ok(vec![
                "a:b1:c:value",
                "a:b2:c:value",
                "a:b3:c:value",
            ]),
        }],
        BulkUpdate::new(0, [Update::Insert(TestDataEntry::new("c", b"new_value", "h_new_c"))]),
        [vec![
            "a:b1:c:new_value",
            "a:b2:c:new_value",
            "a:b3:c:new_value",
        ]],
        [
            (res_key("a", &TestResChainLoader), 1..=1),
            (res_key("b1", &TestResChainLoader), 1..=1),
            (res_key("b2", &TestResChainLoader), 1..=1),
            (res_key("b3", &TestResChainLoader), 1..=1),
            // Even though b1..b3 loads this, it _should_ be cached during the overall "a" load
            (res_key("c", &TestResChainLoader), 1..=1),
        ],
        [("a", 2), ("b1", 2), ("b2", 2), ("b3", 2), ("c", 2)],
        DependencyGraph::builder()
            .add(res_key("a", &TestResChainLoader), [
                res_key("b1", &TestResChainLoader),
                res_key("b2", &TestResChainLoader),
                res_key("b3", &TestResChainLoader),
            ])
            .add(res_key("b1", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .add(res_key("b2", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .add(res_key("b3", &TestResChainLoader), [res_key("c", &TestResChainLoader)])
            .build(),
    )]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_update_dependencies<L, A>(
        test_resource_ctx: TestResourceContext<1>,
        #[case] dependency_graph: DependencyGraph,
        #[case] data_entries: impl IntoIterator<Item=TestDataEntry>,
        #[case] initial_entries: impl IntoIterator<Item=ExpectedLoadInfo<L, A>>,
        #[case] update: BulkUpdate,
        #[case] expected_values: impl IntoIterator<Item=A>,
        #[case] expected_load_counts: impl IntoIterator<Item=(BoxedResKey, impl RangeBounds<usize> + Debug)>,
        #[case] expected_data_load_counts: impl IntoIterator<Item=(impl Into<ResourceId>, usize)>,
        #[case] expected_graph: DependencyGraph,
    )
    where
        L: ResourceLoader,
        L::Output: Debug + Sized,
        A: Debug + PartialEq<L::Output>,
    {
        setup_dependency_data(&test_resource_ctx.data_maps[0], &dependency_graph);
        {
            let mut data_map = test_resource_ctx.data_maps[0].lock();
            for entry in data_entries.into_iter() {
                data_map.insert(entry.id, entry.data, entry.hash);
            }
        }

        let mut values = Vec::new();
        let mut wait_keys = HashSet::default();
        for entry in initial_entries.into_iter() {
            wait_keys.insert(entry.load_info.key());
            let result = test_resource_ctx.manager.get_with_loader(
                entry.load_info.id,
                entry.load_info.loader,
            ).await;
            let value = entry.expected.assert_matches(result)
                .expect("Error expectations not supported in this test");
            values.push(value);
        }

        // Reset counts so we can assert on just the ones resulting from updates
        test_resource_ctx.manager.test_ctx().clear();
        update.apply(&test_resource_ctx.data_maps);

        timeout(async {
            test_resource_ctx.manager.test_ctx().sync_update_block.wait_count(&(), 1).await;
            for key in &wait_keys {
                test_resource_ctx.manager.test_ctx().sync_update.wait_count(key, 1).await;
            }
        }, Duration::from_millis(500)).await;

        for (idx, expected_value) in expected_values.into_iter().enumerate() {
            assert_eq!(&expected_value, &*values[idx].read());
        }

        assert_eq!(&*test_resource_ctx.manager.dependencies.read(), &expected_graph);
        for (key, expected_range) in expected_load_counts.into_iter() {
            test_resource_ctx.manager.test_ctx().sync_load.assert_count_range(&key, expected_range);
        }
        for (id, expected_count) in expected_data_load_counts.into_iter() {
            test_resource_ctx.data_maps[0].assert_load_count(id, expected_count);
        }
    }
}