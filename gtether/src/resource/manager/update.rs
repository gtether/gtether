use ahash::{HashMap, HashSet};
use chrono::{DateTime, Utc};
use futures_util::task::AtomicWaker;
use parking_lot::{Mutex, RwLock};
use smol::future::FutureExt;
use smol::{channel as sc, future, Executor, Task};
use std::any::{Any, TypeId};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake};
use std::thread::JoinHandle;
use tracing::{debug, trace, warn};

use crate::resource::id::ResourceId;
use crate::resource::manager::dependency::DependencyGraph;
use crate::resource::manager::load::{Cache, CacheEntryMetadata, UpdateParams};
use crate::resource::manager::source::Sources;
use crate::resource::manager::ResourceFutureInner;
use crate::resource::source::{BulkResourceUpdate, ResourceUpdate, ResourceUpdater, SourceIndex};
use crate::resource::{ResourceLoadError, ResourceLoadResult};
use crate::util::waker::MultiWaker;

pub struct UpdateOutput<T: ?Sized + Send + Sync + 'static> {
    result: Mutex<Option<ResourceLoadResult<T>>>,
    wakers: Arc<MultiWaker>,
}

impl<T: ?Sized + Send + Sync + 'static> Default for UpdateOutput<T> {
    #[inline]
    fn default() -> Self {
        Self {
            result: Mutex::new(None),
            wakers: Arc::new(MultiWaker::default()),
        }
    }
}

impl<T: ?Sized + Send + Sync + 'static> UpdateOutput<T> {
    #[inline]
    pub fn get(&self) -> Option<ResourceLoadResult<T>> {
        self.result.lock().clone()
    }

    #[inline]
    pub fn set(&self, result: ResourceLoadResult<T>) {
        *self.result.lock() = Some(result);
        self.wakers.wake_by_ref();
    }

    fn future(self: &Arc<Self>) -> ResourceUpdateFuture<T> {
        let waker = Arc::new(AtomicWaker::new());
        ResourceUpdateFuture {
            output: self.clone(),
            waker,
        }
    }

    #[inline]
    pub fn has_futures(&self) -> bool {
        !self.wakers.is_empty()
    }
}

#[derive(Clone)]
pub struct UpdateOutputUntyped {
    output: Arc<dyn Any + Send + Sync>, // Arc<UpdateOutput<T>>,
    output_type: TypeId,
}

impl<T: ?Sized + Send + Sync + 'static> From<Arc<UpdateOutput<T>>> for UpdateOutputUntyped {
    #[inline]
    fn from(output: Arc<UpdateOutput<T>>) -> Self {
        Self {
            output,
            output_type: TypeId::of::<T>(),
        }
    }
}

impl UpdateOutputUntyped {
    #[inline]
    pub fn into_typed<T>(self) -> Result<Arc<UpdateOutput<T>>, ResourceLoadError>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.output.downcast::<UpdateOutput<T>>()
            .map_err(|_| ResourceLoadError::TypeMismatch {
                requested: TypeId::of::<T>(),
                actual: self.output_type,
            })
    }
}

pub struct ResourceUpdateFuture<T: ?Sized + Send + Sync + 'static> {
    output: Arc<UpdateOutput<T>>,
    waker: Arc<AtomicWaker>,
}

impl<T: ?Sized + Send + Sync + 'static> Future for ResourceUpdateFuture<T> {
    type Output = ResourceLoadResult<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.waker.register(cx.waker());

        match self.output.get() {
            Some(result) => Poll::Ready(result),
            None => Poll::Pending,
        }
    }
}

struct UpdateTask {
    hash: String,
    source_idx: SourceIndex,
    timestamp: DateTime<Utc>,
    task: Task<()>,
    output: UpdateOutputUntyped,
}

impl UpdateTask {
    #[inline]
    fn is_stale(&self) -> bool {
        // TODO: Possibly still race condition where update is actually complete (and lets go of e.g.
        //  locks) before task is finished? (maybe only relevant for test validation)
        self.task.is_finished()
    }

    fn valid_for_update(
        &self,
        hash: &str,
        source_idx: &SourceIndex,
        timestamp: DateTime<Utc>,
    ) -> bool {
        if &self.hash == hash || source_idx > &self.source_idx {
            return false
        }
        timestamp >= self.timestamp
    }
}

pub struct Updates {
    inner: smol::lock::RwLock<HashMap<ResourceId, UpdateTask>>,
}

impl Updates {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: smol::lock::RwLock::new(Default::default()),
        })
    }

    pub fn get_future<T>(&self, id: &ResourceId) -> Option<ResourceFutureInner<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let inner = self.inner.read_blocking();
        let task = inner.get(id)?;
        if !task.is_stale() {
            match task.output.clone().into_typed() {
                Ok(output) => Some(ResourceFutureInner::Updating(output.future())),
                Err(error) => Some(ResourceFutureInner::Cached(Err(error))),
            }
        } else {
            // Update entry is stale; it has already modified the cache so there is no need to wait
            // on it
            None
        }
    }
}

enum Msg {
    Update{
        src_idx: SourceIndex,
        bulk_update: BulkResourceUpdate,
    },
    Close,
}

enum State {
    Init {
        receiver: sc::Receiver<Msg>,
    },
    Started {
        join_handle: JoinHandle<()>,
    },
    Stopped,
}

pub struct UpdateManager {
    sender: sc::Sender<Msg>,
    updates: Arc<Updates>,
    state: State,
}

impl UpdateManager {
    pub fn new() -> Self {
        let (sender, receiver) = sc::unbounded();
        Self {
            sender,
            updates: Updates::new(),
            state: State::Init { receiver },
        }
    }

    pub fn start(
        &mut self,
        sources: Arc<Sources>,
        cache: Arc<Cache>,
        dependencies: Arc<RwLock<DependencyGraph>>,
        #[cfg(test)]
        sync_update: Arc<super::tests::SyncContext>,
    ) {
        match &self.state {
            State::Init { receiver } => {
                let receiver = receiver.clone();
                let updates = self.updates.clone();
                let join_handle = std::thread::Builder::new()
                    .name("resource-update-manager".to_string())
                    .spawn(move || UpdateManagerThread::new(
                        receiver,
                        updates,
                        sources,
                        cache,
                        dependencies,
                        #[cfg(test)]
                        sync_update,
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
                let _ = self.sender.send_blocking(Msg::Close);
                join_handle.join().unwrap();
            },
            _ => {},
        }
    }

    #[inline]
    pub fn updates(&self) -> &Arc<Updates> {
        &self.updates
    }

    #[inline]
    pub fn create_watcher(&self, source_idx: SourceIndex) -> Box<dyn ResourceUpdater> {
        Box::new(UpdateManagerResourceWatcher {
            sender: self.sender.clone(),
            idx: source_idx,
        })
    }
}

impl Drop for UpdateManager {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug, Clone)]
struct UpdateManagerResourceWatcher {
    sender: sc::Sender<Msg>,
    idx: SourceIndex,
}

impl ResourceUpdater for UpdateManagerResourceWatcher {
    #[inline]
    fn notify_update(&self, bulk_update: BulkResourceUpdate) {
        // Silently ignore if the channel is closed
        let _ = self.sender.send_blocking(Msg::Update {
            src_idx: self.idx.clone(),
            bulk_update,
        });
    }

    #[inline]
    fn clone_with_sub_index(&self, sub_idx: SourceIndex) -> Box<dyn ResourceUpdater> {
        Box::new(Self {
            sender: self.sender.clone(),
            idx: self.idx.clone().with_sub_idx(Some(sub_idx)),
        })
    }
}

struct UpdateManagerThread {
    receiver: sc::Receiver<Msg>,
    exec: Executor<'static>,
    updates: Arc<Updates>,
    sources: Arc<Sources>,
    cache: Arc<Cache>,
    dependencies: Arc<RwLock<DependencyGraph>>,
    #[cfg(test)]
    sync_update: Arc<super::tests::SyncContext>,
}

impl UpdateManagerThread {
    fn new(
        receiver: sc::Receiver<Msg>,
        updates: Arc<Updates>,
        sources: Arc<Sources>,
        cache: Arc<Cache>,
        dependencies: Arc<RwLock<DependencyGraph>>,
        #[cfg(test)]
        sync_update: Arc<super::tests::SyncContext>,
    ) -> Self {
        Self {
            receiver,
            exec: Executor::<'static>::new(),
            updates,
            sources,
            cache,
            dependencies,
            #[cfg(test)]
            sync_update
        }
    }

    fn run(self) {
        future::block_on(self.loop_receive().or(self.loop_exec()));
    }

    fn create_update_task(
        &self,
        metadata: &CacheEntryMetadata,
        prev_task: Option<UpdateTask>,
    ) -> (Task<()>, UpdateOutputUntyped) {
        let (prev_output, prev_task) = match prev_task {
            Some(prev_task) => (Some(prev_task.output), Some(prev_task.task)),
            None => (None, None),
        };
        let (fut, output) = metadata.create_update_future(
            &self.cache,
            UpdateParams {
                prev_output,
                #[cfg(test)]
                sync_update: self.sync_update.clone(),
            }
        );
        let task = self.exec.spawn(async move {
            if let Some(prev_task) = prev_task {
                prev_task.cancel().await;
            }

            fut.await;
        });
        (task, output)
    }

    async fn loop_receive(&self) {
        'exit: loop {
            if let Ok(msg) = self.receiver.recv().await {
                match msg {
                    Msg::Update{ src_idx: source_idx, bulk_update } => {
                        let mut queued_updates = self.updates.inner.write().await;

                        let mut dependent_update_ids = HashSet::default();
                        let mut queued_update_ids = HashSet::default();
                        'updates: for (id, update) in bulk_update.updates {
                            trace!(%id, %source_idx, ?update, "Processing update");

                            {
                                let dependency_graph = self.dependencies.read();
                                if let Some(node) = dependency_graph.get(&id) {
                                    for parent in node.dependents().recursive() {
                                        dependent_update_ids.insert(parent.id().clone());
                                    }
                                }
                            }

                            let metadata = match self.cache.metadata(&id) {
                                Some(metadata) => metadata,
                                // No entry in the cache, this update will be ignored
                                None => {
                                    trace!(%id, "Clearing expired cache entry");
                                    self.cache.remove(&id);
                                    #[cfg(test)]
                                    self.sync_update.mark_attempt();
                                    continue 'updates
                                },
                            };

                            let (hash, source_idx) = match update {
                                ResourceUpdate::Added(hash) | ResourceUpdate::Modified(hash) => {
                                    if !metadata.valid_for_update(&hash, &source_idx) {
                                        #[cfg(test)]
                                        self.sync_update.mark_attempt();
                                        continue 'updates
                                    }

                                    (hash.clone(), source_idx.clone())
                                },
                                ResourceUpdate::Removed => {
                                    let new_source = match self.sources.find_hash(&id) {
                                        Some(new_source) => new_source,
                                        // Data was removed, but there isn't a backup. Leave existing
                                        // entry alone for now
                                        None => {
                                            warn!(%id, "All valid data sources have been removed for active resource");
                                            #[cfg(test)]
                                            self.sync_update.mark_attempt();
                                            continue 'updates
                                        },
                                    };

                                    (new_source.hash, new_source.idx)
                                }
                            };

                            let prev_task = if let Some(queued_update) = queued_updates.get(&id) {
                                if queued_update.is_stale() {
                                    // House-cleaning
                                    queued_updates.remove(&id);
                                    None
                                } else {
                                    if !queued_update.valid_for_update(&hash, &source_idx, bulk_update.timestamp) {
                                        // There's an existing update that has priority
                                        debug!(
                                            %id,
                                            %source_idx,
                                            existing_source_idx = %queued_update.source_idx,
                                            ?hash,
                                            existing_hash = ?queued_update.hash,
                                            "Higher priority update already exists",
                                        );
                                        #[cfg(test)]
                                        self.sync_update.mark_attempt();
                                        continue 'updates
                                    }

                                    let queued_update = queued_updates.remove(&id).unwrap();
                                    Some(queued_update)
                                }
                            } else {
                                None
                            };

                            queued_update_ids.insert(id.clone());

                            trace!(%id, %source_idx, ?hash, "Creating queued update");
                            let (task, output) = self.create_update_task(
                                &metadata,
                                prev_task,
                            );

                            queued_updates.insert(id, UpdateTask {
                                hash: hash.clone(),
                                source_idx: source_idx.clone(),
                                timestamp: bulk_update.timestamp.clone(),
                                task,
                                output,
                            });
                        }

                        'updates: for id in dependent_update_ids.difference(&queued_update_ids) {
                            trace!(%id, %source_idx, "Processing triggered dependent update");
                            let metadata = match self.cache.metadata(id) {
                                Some(metadata) => metadata,
                                // No entry in the cache, this update will be ignored
                                None => {
                                    trace!(%id, "Clearing expired cache entry");
                                    self.cache.remove(id);
                                    #[cfg(test)]
                                    self.sync_update.mark_attempt();
                                    continue 'updates
                                },
                            };

                            let hash = match metadata.hash() {
                                Some(hash) => hash.to_string(),
                                None => {
                                    warn!(%id, "Dependent update triggered before load could determine hash; cannot update. Resource may be out-of-sync");
                                    #[cfg(test)]
                                    self.sync_update.mark_attempt();
                                    continue 'updates
                                }
                            };
                            let source_idx = metadata.source_idx().clone();

                            let prev_task = if let Some(queued_update) = queued_updates.get(id) {
                                if queued_update.is_stale() {
                                    // House-cleaning
                                    queued_updates.remove(id);
                                    None
                                } else {
                                    debug!(
                                        %id, %source_idx, ?hash,
                                        "Re-queueing existing dependent update",
                                    );
                                    let queued_update = queued_updates.remove(id).unwrap();
                                    Some(queued_update)
                                }
                            } else {
                                None
                            };

                            trace!(%id, %source_idx, ?hash, "Creating queued dependent update");
                            let (task, output) = self.create_update_task(
                                &metadata,
                                prev_task,
                            );

                            queued_updates.insert(id.clone(), UpdateTask {
                                hash,
                                source_idx: metadata.source_idx().clone(),
                                timestamp: Utc::now(),
                                task,
                                output,
                            });
                        }

                    }
                    Msg::Close => {
                        // Start exiting
                        break 'exit
                    }
                }
            } else {
                // Sender is closed, so this thread is abandoned. Start exiting
                break 'exit
            }
        }
    }

    async fn loop_exec(&self) {
        loop {
            for _ in 0..200 {
                self.exec.tick().await;
            }

            // Yield occasionally
            future::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use macro_rules_attribute::apply;
    use rstest::rstest;
    use smol_macros::test as smol_test;
    use std::sync::Arc;
    use std::time::Duration;
    use test_log::test as test_log;

    use crate::resource::id::ResourceId;
    use crate::resource::manager::dependency::DependencyGraph;
    use crate::resource::manager::tests::prelude::*;
    use crate::resource::manager::ResourceManager;
    use crate::resource::ResourceLoader;

    #[rstest]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_manager_can_drop_while_updating(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
    ) {
        data_maps[0].lock().insert("key", b"value", "h_value");

        let _value = manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("Resource should load for 'key'");

        let sync_update = manager.test_ctx().sync_update.clone();
        let _lock = sync_update.block().await;
        data_maps[0].lock().insert("key", b"new_value", "h_new_value");

        timeout(
            assert_manager_drops(manager),
            Duration::from_secs(1),
        ).await;
    }

    #[rstest]
    #[case::value_to_value(ExpectedDataEntry::ok("key"), ExpectedDataEntry::ok_new("key"))]
    #[case::value_to_error(ExpectedDataEntry::ok("key"), ExpectedDataEntry::read_error_new("key"))]
    #[case::error_to_value(ExpectedDataEntry::read_error("key"), ExpectedDataEntry::ok_new("key"))]
    #[case::error_to_error(ExpectedDataEntry::read_error("key"), ExpectedDataEntry::read_error_new("key"))]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_update(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
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
    async fn test_update_source_order(
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

    #[rstest]
    #[case::same("h_value", 0, "value")]
    #[case::different("h_new_value", 1, "new_value")]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_update_hashing(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
        #[case] new_hash: &str,
        #[case] expected_update_count: usize,
        #[case] expected_value: &str,
    ) {
        data_maps[0].lock().insert("key", b"value", "h_value");

        let value = manager.get_with_loader(
            "key",
            TestResLoader::new("key"),
        ).await.expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());

        data_maps[0].lock().insert("key", b"new_value", new_hash);
        timeout(
            manager.test_ctx().sync_update.wait_attempts(1),
            Duration::from_millis(500),
        ).await;
        manager.test_ctx().sync_update.assert_count(expected_update_count);
        assert_eq!(*value.read(), expected_value.to_owned());
    }

    #[rstest]
    #[case::single_value(
        DependencyGraph::builder().add("a", ["b"]).build(),
        [TestDataEntry::new("b", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", TestResChainLoader::default()),
            expected: ExpectedLoadResult::Ok(vec!["a:b:value"]),
        }],
        BulkUpdate::new(0, [Update::Insert(TestDataEntry::new("b", b"new_value", "h_new_value"))]),
        1, 3, // includes 'new_value'
        [vec!["a:b:new_value"]],
        DependencyGraph::builder().add("a", ["b"]).build(),
    )]
    #[case::single_swap(
        DependencyGraph::builder().add("a", ["b"]).build(),
        [TestDataEntry::new("b", b"value", "h_value"), TestDataEntry::new("c", b"other_value", "h_other_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", TestResChainLoader::default()),
            expected: ExpectedLoadResult::Ok(vec!["a:b:value"]),
        }],
        BulkUpdate::new(0, [Update::Insert(TestDataEntry::new("a", b"c", "h_new_a_dep"))]),
        1, 3, // includes 'new_value'
        [vec!["a:c:other_value"]],
        DependencyGraph::builder().add("a", ["c"]).build(),
    )]
    #[case::single_gap(
        DependencyGraph::builder().add("a", ["b"]).add("b", ["c"]).build(),
        [TestDataEntry::new("c", b"value", "h_value")],
        [ExpectedLoadInfo {
            load_info: TestLoadInfo::new("a", TestResChainLoader::default()),
            expected: ExpectedLoadResult::Ok(vec!["a:b:c:value"]),
        }],
        BulkUpdate::new(0, [Update::Insert(TestDataEntry::new("c", b"new_value", "h_new_value"))]),
        1, 4, // includes 'new_value'
        [vec!["a:b:c:new_value"]],
        DependencyGraph::builder().add("a", ["b"]).add("b", ["c"]).build(),
    )]
    #[case::tree(
        DependencyGraph::builder()
            .add("a1", ["b1", "b2"]).add("a2", ["b1", "b2"])
            .add("b1", ["c1", "c2"]).add("b2", ["c2", "c3"])
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
        2, 12, // includes 'value#'
        [
            vec!["a1:b1:c1:value1", "a1:b1:c2:new_value2"],
            vec![
                "a2:b1:c1:value1", "a2:b1:c2:new_value2",
                "a2:b2:c1:value1", "a2:b2:c2:new_value2", "a2:b2:c3:new_value3",
            ],
        ],
        DependencyGraph::builder()
            .add("a1", ["b1"]).add("a2", ["b1", "b2"])
            .add("b1", ["c1", "c2"]).add("b2", ["c1", "c2", "c3"])
            .build(),
    )]
    #[case::diamond(
        DependencyGraph::builder()
            .add("a", ["b1", "b2", "b3"])
            .add("b1", ["c"]).add("b2", ["c"]).add("b3", ["c"])
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
        1, 6, // includes 'new_value'
        [vec![
            "a:b1:c:new_value",
            "a:b2:c:new_value",
            "a:b3:c:new_value",
        ]],
        DependencyGraph::builder()
            .add("a", ["b1", "b2", "b3"])
            .add("b1", ["c"]).add("b2", ["c"]).add("b3", ["c"])
            .build(),
    )]
    #[test_attr(test_log(apply(smol_test)))]
    async fn test_update_dependencies<L>(
        #[from(test_resource_manager)] (manager, data_maps): (Arc<ResourceManager>, [Arc<ResourceDataMap>; 1]),
        #[case] dependency_graph: DependencyGraph,
        #[case] data_entries: impl IntoIterator<Item=TestDataEntry>,
        #[case] initial_entries: impl IntoIterator<Item=ExpectedLoadInfo<Vec<String>, L, Vec<&'static str>>>,
        #[case] update: BulkUpdate,
        #[case] wait_update_count: usize,
        #[case] expected_update_load_count: usize,
        #[case] expected_values: impl IntoIterator<Item=Vec<&'static str>>,
        #[case] expected_graph: DependencyGraph,
    )
    where
        L: ResourceLoader<Vec<String>>,
    {
        setup_dependency_data(&data_maps[0], &dependency_graph);
        {
            let mut data_map = data_maps[0].lock();
            for entry in data_entries.into_iter() {
                data_map.insert(entry.id, entry.data, entry.hash);
            }
        }

        let mut values = Vec::new();
        for entry in initial_entries.into_iter() {
            let result = manager.get_with_loader(
                entry.load_info.key,
                entry.load_info.loader,
            ).await;
            let value = entry.expected.assert_matches(result)
                .expect("Error expectations not supported in this test");
            values.push(value);
        }

        // Reset load counts so we can assert on just the ones resulting from updates
        manager.test_ctx().sync_load.clear();
        update.apply(&data_maps);

        timeout(
            manager.test_ctx().sync_update.wait_count(wait_update_count),
            Duration::from_millis(500),
        ).await;
        manager.test_ctx().sync_load.assert_count(expected_update_load_count);

        for (idx, expected_value) in expected_values.into_iter().enumerate() {
            assert_eq!(&expected_value, &*values[idx].read());
        }

        assert_eq!(&*manager.dependencies.read(), &expected_graph);
    }
}