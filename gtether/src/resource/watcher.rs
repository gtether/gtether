//! Resource update watcher logic.
//!
//! [ResourceWatchers](ResourceWatcher) are used to notify e.g. resource [managers](super::manager)
//! when changes occur to resources that a [source](super::source) provides. Notification occurs
//! through a channel described by pairs of [ResourceWatcherReceiver]/[ResourceWatcherSender].
//!
//! # Configuration
//!
//! After being created, ResourceWatchers need to be configured before they can send update
//! messages. When a watcher is configured, it will automatically configure its children with the
//! required modifications to the config.
//!
//! This configuration is usually handled by the [ResourceManager](super::manager::ResourceManager)
//! or equivalent, so if you are designing a resource source, it is not something you need to worry
//! about. If you are designing an equivalent to the ResourceManager, configuration consists of the
//! [ResourceWatcherConfig] struct being passed to top-level watchers via
//! [`ResourceWatcher::configure()`]

use std::borrow::Borrow;
use ahash::{HashMap, HashSet};
use educe::Educe;
use parking_lot::{RwLock, RwLockReadGuard};
use smol::channel as sc;
use smol::channel::TryRecvError;
use std::iter::FusedIterator;
use std::ops::Deref;
use std::sync::Arc;
use tracing::trace;

use crate::resource::id::ResourceId;
use crate::resource::source::{BulkResourceUpdate, ResourceUpdate, SourceIndex};

/// Update notification message sent by [ResourceWatchers](ResourceWatcher) when
/// [notifying](ResourceWatcher::notify_update).
#[derive(Debug)]
pub struct UpdateMsg {
    /// [SourceIndex] of the watcher (and by extension the [source](super::source::ResourceSource))
    /// that sent the update notification.
    pub src_idx: SourceIndex,

    /// Update data.
    pub bulk_update: BulkResourceUpdate,
}

/// Sender half of the notification channel, paired with [ResourceWatcherReceiver].
///
/// This is cheaply cloneable and all clones send messages to the original receiver.
#[derive(Clone)]
pub struct ResourceWatcherSender(sc::Sender<UpdateMsg>);

impl ResourceWatcherSender {
    /// Send an [UpdateMsg].
    ///
    /// This method sends immediately and silently ignores errors if the receiving side is closed.
    /// As such, messages should be considered sent on a "best-effort" basis.
    #[inline]
    pub fn send(&self, msg: UpdateMsg) {
        // Silently ignore if the channel is closed
        let _ = self.0.send_blocking(msg);
    }
}

/// Receiver half of the notification channel, paired with [ResourceWatcherSender].
pub struct ResourceWatcherReceiver(sc::Receiver<UpdateMsg>);

impl ResourceWatcherReceiver {
    /// Attempt to receive an [UpdateMsg].
    ///
    /// This method does not block, and if there are no pending messages, it will yield `None`.
    ///
    /// # Errors
    ///
    /// Errors if all senders are already closed.
    pub fn try_recv(&self) -> Result<Option<UpdateMsg>, ()> {
        match self.0.try_recv() {
            Ok(msg) => Ok(Some(msg)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Closed) => Err(()),
        }
    }

    /// Await the receival of an [UpdateMsg].
    ///
    /// This method will await until a message is sent if there are no pending messages.
    ///
    /// # Errors
    ///
    /// Errors if all senders are already closed.
    #[inline]
    pub async fn recv(&self) -> Result<UpdateMsg, ()> {
        self.0.recv().await.map_err(|_| ())
    }
}

/// Create a notification channel pair.
pub fn update_channel() -> (ResourceWatcherSender, ResourceWatcherReceiver) {
    let (send, recv) = sc::unbounded();
    (ResourceWatcherSender(send), ResourceWatcherReceiver(recv))
}

/// Configuration data used to configure a [ResourceWatcher].
///
/// See [module-level documentation](super::watcher#configuration) for more.
#[derive(Educe, Clone)]
#[educe(Debug)]
pub struct ResourceWatcherConfig {
    /// [SourceIndex] that the [ResourceWatcher] should be attached to.
    pub src_idx: SourceIndex,

    /// Sender half of the notification channel.
    #[educe(Debug(ignore))]
    pub sender: ResourceWatcherSender,
}

impl ResourceWatcherConfig {
    /// Clone this config while modifying the [SourceIndex] with a sub-index.
    ///
    /// This is commonly used to propagate configuration to child watchers.
    pub fn clone_with_sub_index(&self, sub_idx: impl Into<SourceIndex>) -> Self {
        Self {
            src_idx: self.src_idx.clone().with_sub_idx(Some(sub_idx)),
            sender: self.sender.clone(),
        }
    }
}

/// Trait that provides access to resource hashes.
///
/// This trait is used to determine when resources can be provided by a source, for the purpose of
/// generating watcher updates. [ResourceWatchers](ResourceWatcher) will use this trait when moving
/// or otherwise modifying child watchers to automatically generate a
/// [bulk update](BulkResourceUpdate).
///
/// This trait is automatically implemented for many common use-cases, such as `Arc<T>` and `RwLock`
/// wrapper implementations and a "noop" implementation for `()`.
pub trait ResourceHashProvider: Send + Sync + 'static {
    /// If the resource specified by `id` can be provided, yields its hash.
    fn hash(&self, id: &ResourceId) -> Option<String>;

    /// For a set of IDs, map all IDs that resources can provided for to their hashes.
    #[inline]
    fn hashes(&self, ids: HashSet<ResourceId>) -> HashMap<ResourceId, String> {
        ids.into_iter()
            .filter_map(|id| self.hash(&id).map(|hash| (id, hash)))
            .collect()
    }
}

impl<T: ResourceHashProvider + ?Sized> ResourceHashProvider for Arc<T> {
    #[inline]
    fn hash(&self, id: &ResourceId) -> Option<String> {
        self.deref().hash(id)
    }
}

impl<T, R> ResourceHashProvider for parking_lot::lock_api::RwLock<R, T>
where
    T: ResourceHashProvider + ?Sized,
    R: parking_lot::lock_api::RawRwLock + Send + Sync + 'static,
{
    #[inline]
    fn hash(&self, id: &ResourceId) -> Option<String> {
        self.read().hash(id)
    }

    #[inline]
    fn hashes(&self, ids: HashSet<ResourceId>) -> HashMap<ResourceId, String> {
        let lock = self.read();
        ids.into_iter()
            .filter_map(|id| lock.hash(&id).map(|hash| (id, hash)))
            .collect()
    }
}

impl<T> ResourceHashProvider for smol::lock::RwLock<T>
where
    T: ResourceHashProvider + ?Sized,
{
    #[inline]
    fn hash(&self, id: &ResourceId) -> Option<String> {
        self.read_blocking().hash(id)
    }

    #[inline]
    fn hashes(&self, ids: HashSet<ResourceId>) -> HashMap<ResourceId, String> {
        let lock = self.read_blocking();
        ids.into_iter()
            .filter_map(|id| lock.hash(&id).map(|hash| (id, hash)))
            .collect()
    }
}

impl ResourceHashProvider for () {
    #[inline]
    fn hash(&self, _id: &ResourceId) -> Option<String> {
        None
    }
}

impl ResourceHashProvider for std::collections::HashMap<ResourceId, String> {
    #[inline]
    fn hash(&self, id: &ResourceId) -> Option<String> {
        self.get(id).cloned()
    }
}

impl ResourceHashProvider for HashMap<ResourceId, String> {
    #[inline]
    fn hash(&self, id: &ResourceId) -> Option<String> {
        self.get(id).cloned()
    }
}

impl<F> ResourceHashProvider for F
where
    F: Fn(&ResourceId) -> Option<String> + Send + Sync + 'static,
{
    #[inline]
    fn hash(&self, id: &ResourceId) -> Option<String> {
        (*self)(id)
    }
}

#[derive(Educe)]
#[educe(Debug(bound()))]
struct State {
    config: Option<ResourceWatcherConfig>,
    children: Vec<ArcState>,
    #[educe(Debug(ignore))]
    provider: Box<dyn ResourceHashProvider>,
    partial_watches: HashSet<ResourceId>,
    watches: HashSet<ResourceId>,
}
type ArcState = Arc<RwLock<State>>;

impl State {
    fn new(provider: Box<dyn ResourceHashProvider>) -> Self {
        Self {
            config: None,
            children: vec![],
            provider,
            partial_watches: HashSet::default(),
            watches: HashSet::default(),
        }
    }

    fn with_children(provider: Box<dyn ResourceHashProvider>, children: Vec<ArcState>) -> Self {
        for child in &children {
            child.write().unconfigure();
        }
        Self {
            config: None,
            children,
            provider,
            partial_watches: HashSet::default(),
            watches: HashSet::default(),
        }
    }

    fn notify_update(&self, bulk_update: BulkResourceUpdate) {
        if !bulk_update.updates.is_empty() && let Some(config) = &self.config {
            let src_idx = config.src_idx.clone();
            trace!(%src_idx, ?bulk_update, "Notifying resource watcher update");
            config.sender.send(UpdateMsg {
                src_idx,
                bulk_update,
            });
        }
    }

    fn configure_children(&self, start_idx: usize, config: &ResourceWatcherConfig) {
        for (idx, child) in self.children[start_idx..].iter().enumerate() {
            child.write().configure(config.clone_with_sub_index(start_idx + idx), 0);
        }
    }

    fn configure(
        &mut self,
        config: ResourceWatcherConfig,
        start_idx: usize,
    ) {
        let watches = self.all_watches().collect::<HashSet<_>>();
        trace!(?start_idx, src_idx = %config.src_idx, ?watches, "Configuring resource watcher");
        let hashes = self.provider.hashes(watches);
        let update: BulkResourceUpdate = match &self.config {
            Some(config) => {
                hashes.into_iter()
                    .map(|(id, _)| (id, ResourceUpdate::MovedSourceIndex(config.src_idx.clone())))
                    .collect()
            },
            None => {
                hashes.into_iter()
                    .map(|(id, hash)| (id, ResourceUpdate::Added(hash.to_string())))
                    .collect()
            },
        };
        self.config = Some(config);
        self.notify_update(update);
        self.configure_children(start_idx, self.config.as_ref().unwrap());
    }

    fn reconfigure(&self, start_idx: usize) {
        if let Some(config) = &self.config {
            self.configure_children(start_idx, config);
        }
    }

    fn unconfigure(&mut self) {
        let watches = self.watches.drain()
            .chain(self.partial_watches.drain())
            .collect::<HashSet<_>>();
        let hashes = self.provider.hashes(watches);
        let update = hashes.into_iter()
            .map(|(id, _)| (id, ResourceUpdate::Removed))
            .collect();
        self.notify_update(update);
        for child in &self.children {
            child.write().unconfigure();
        }
        self.config = None;
    }

    fn watch(&mut self, id: ResourceId, sub_idx: Option<SourceIndex>) {
        let sub_idx = match sub_idx {
            Some(sub_idx) => {
                self.partial_watches.insert(id.clone());
                sub_idx
            },
            None => {
                self.watches.insert(id.clone());
                SourceIndex::max()
            },
        };

        for (idx, child) in self.children.iter().enumerate() {
            let mut child = child.write();
            if idx <= sub_idx.idx() {
                child.watch(id.clone(), sub_idx.sub_idx().cloned());
            } else {
                child.unwatch(&id);
            }
        }
    }

    fn unwatch(&mut self, id: &ResourceId) {
        for child in &self.children {
            child.write().unwatch(id);
        }
        self.partial_watches.remove(id);
        self.watches.remove(id);
    }

    fn is_watched(&self, id: &ResourceId) -> bool {
        self.watches.contains(id) || self.partial_watches.contains(id)
    }

    fn all_watches(&self) -> impl Iterator<Item=ResourceId> + Clone {
        self.watches.iter().cloned().chain(self.partial_watches.iter().cloned())
    }

    fn add_watches<II>(&mut self, watches: II)
    where
        II: IntoIterator<Item=ResourceId>,
        II::IntoIter: Clone,
    {
        let watches = watches.into_iter();
        self.watches.extend(watches.clone());
        for child in &self.children {
            child.write().add_watches(watches.clone());
        }
    }

    fn insert_child(&mut self, idx: usize, child: ArcState) {
        if idx < self.children.len() {
            child.write().add_watches(self.children[idx].read().all_watches());
            self.children.insert(idx, child);
            self.reconfigure(idx);
        } else {
            self.push_child(child);
        }
    }

    fn push_child(&mut self, child: ArcState) {
        child.write().add_watches(self.watches.iter().cloned());
        self.children.push(child);
        self.reconfigure(self.children.len() - 1);
    }

    fn remove_child(&mut self, idx: usize) -> ArcState {
        let child = self.children.remove(idx);
        child.write().unconfigure();
        if idx < self.children.len() {
            self.reconfigure(idx)
        }
        child
    }
}

/// Watcher for resource change updates.
///
/// Usually associated 1:1 with a [ResourceSource](super::source::ResourceSource).
#[derive(Debug)]
pub struct ResourceWatcher {
    state: ArcState,
}

impl ResourceWatcher {
    /// Create a new watcher using the specified [`provider`](ResourceHashProvider).
    ///
    /// ```
    /// use gtether::resource::watcher::ResourceWatcher;
    ///
    /// // "default" watcher for something that doesn't provide any resources
    /// let watcher = ResourceWatcher::new(());
    /// ```
    pub fn new(provider: impl ResourceHashProvider) -> Self {
        Self {
            state: Arc::new(RwLock::new(State::new(Box::new(provider)))),
        }
    }

    /// Create a new watcher, and pre-populate it with the specified `children`.
    ///
    /// ```
    /// use gtether::resource::watcher::ResourceWatcher;
    /// let children = [
    ///     ResourceWatcher::new(()),
    ///     ResourceWatcher::new(()),
    ///     ResourceWatcher::new(()),
    /// ];
    /// let watcher = ResourceWatcher::with_children((), &children);
    /// ```
    pub fn with_children<'a>(
        provider: impl ResourceHashProvider,
        children: impl IntoIterator<Item=&'a ResourceWatcher>,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(State::with_children(
                Box::new(provider),
                children.into_iter().map(|watcher| watcher.state.clone()).collect(),
            )))
        }
    }

    /// Notify that resource/s have changed.
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::source::ResourceUpdate;
    /// use gtether::resource::watcher::ResourceWatcher;
    ///
    /// let watcher = ResourceWatcher::new(());
    /// watcher.notify_update((
    ///     ResourceId::from("my-resource-id"),
    ///     ResourceUpdate::Added("my-resource-id-hash".to_string())
    /// ).into());
    /// ```
    #[inline]
    pub fn notify_update(&self, bulk_update: BulkResourceUpdate) {
        self.state.read().notify_update(bulk_update);
    }

    /// Configure this watcher.
    ///
    /// See [module-level documentation](super::watcher#configuration) for more.
    #[inline]
    pub fn configure(&self, config: ResourceWatcherConfig) {
        self.state.write().configure(config, 0);
    }

    /// Unconfigure this watcher, effectively putting it back in a default state.
    ///
    /// This will also remove all active watches, and generate an [update](BulkResourceUpdate) that
    /// notifies the removal of all said watches.
    ///
    /// ```
    /// use gtether::resource::watcher::ResourceWatcher;
    ///
    /// let watcher = ResourceWatcher::new(());
    /// watcher.unconfigure();
    /// ```
    #[inline]
    pub fn unconfigure(&self) {
        self.state.write().unconfigure();
    }

    /// Watch a particular [ResourceId].
    ///
    /// Optionally, restrict the resource watch to a particular sub-[SourceIndex] and below. For
    /// example, if the sub-index "2" is specified, child watchers 0-2 will be told to watch the
    /// [ResourceId], but any child watcher at index 3 or later will not.
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::watcher::ResourceWatcher;
    ///
    /// let watcher = ResourceWatcher::new(());
    /// watcher.watch(ResourceId::from("resource-a"), None);
    /// watcher.watch(ResourceId::from("resource-b"), Some(2.into()));
    ///
    /// assert!(watcher.is_watched(&ResourceId::from("resource-a")));
    /// assert!(watcher.is_watched(&ResourceId::from("resource-b")));
    /// ```
    #[inline]
    pub fn watch(&self, id: ResourceId, sub_idx: Option<SourceIndex>) {
        self.state.write().watch(id, sub_idx);
    }

    /// "Un"-watch a particular [ResourceId].
    ///
    /// The given ID will no longer be watched, and therefore no longer generate change updates.
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::watcher::ResourceWatcher;
    ///
    /// let watcher = ResourceWatcher::new(());
    /// watcher.watch(ResourceId::from("resource-a"), None);
    /// watcher.watch(ResourceId::from("resource-b"), Some(2.into()));
    ///
    /// watcher.unwatch(&ResourceId::from("resource-a"));
    /// watcher.unwatch(&ResourceId::from("resource-b"));
    ///
    /// assert!(!watcher.is_watched(&ResourceId::from("resource-a")));
    /// assert!(!watcher.is_watched(&ResourceId::from("resource-b")));
    /// ```
    #[inline]
    pub fn unwatch(&self, id: &ResourceId) {
        self.state.write().unwatch(id);
    }

    /// Retrieve whether a given [ResourceId] is currently being watched.
    ///
    /// Partial watches, where only some child watchers are watching an ID, will still cause this
    /// to yield `true`.
    #[inline]
    pub fn is_watched(&self, id: &ResourceId) -> bool {
        self.state.read().is_watched(id)
    }

    /// Insert a reference to a child watcher at position `idx`, shifting all children after it to
    /// the right.
    ///
    /// See also: [`Vec::insert()`].
    ///
    /// NOTE: While it is technically possible to insert a child watcher reference in multiple other
    /// watchers, doing so is undefined behavior.
    ///
    /// ```
    /// use gtether::resource::watcher::ResourceWatcher;
    ///
    /// let watcher = ResourceWatcher::new(());
    /// let child_0 = ResourceWatcher::new(());
    /// let child_1 = ResourceWatcher::new(());
    /// watcher.insert_child(0, &child_1);
    /// watcher.insert_child(0, &child_0);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `idx` > `len`.
    #[inline]
    pub fn insert_child(&self, idx: usize, child: &ResourceWatcher) {
        self.state.write().insert_child(idx, child.state.clone());
    }

    /// Append a reference to a child watcher.
    ///
    /// See also: [`Vec::push()`].
    ///
    /// NOTE: While it is technically possible to insert a child watcher reference in multiple other
    /// watchers, doing so is undefined behavior.
    ///
    /// ```
    /// use gtether::resource::watcher::ResourceWatcher;
    ///
    /// let watcher = ResourceWatcher::new(());
    /// let child_0 = ResourceWatcher::new(());
    /// let child_1 = ResourceWatcher::new(());
    /// watcher.push_child(&child_0);
    /// watcher.push_child(&child_1);
    /// ```
    #[inline]
    pub fn push_child(&self, child: &ResourceWatcher) {
        self.state.write().push_child(child.state.clone());
    }

    /// Remove a reference to a child watcher at position `idx`, shifting all children after it to
    /// the left.
    ///
    /// See also: [`Vec::remove()`].
    ///
    /// ```
    /// use gtether::resource::watcher::ResourceWatcher;
    ///
    /// let watcher = ResourceWatcher::new(());
    /// let child_0 = ResourceWatcher::new(());
    /// watcher.push_child(&child_0);
    /// watcher.remove_child(0);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `idx` is out of bounds.
    #[inline]
    pub fn remove_child(&self, idx: usize) {
        self.state.write().remove_child(idx);
    }
}

pub struct FilterWatched<'a, I> {
    iter: I,
    state: RwLockReadGuard<'a, State>,
}

impl<I> Iterator for FilterWatched<'_, I>
where
    I: Iterator,
    I::Item: Borrow<ResourceId>,
{
    type Item = I::Item;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.iter.find(|item| self.state.is_watched(item.borrow()))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let (_, upper) = self.iter.size_hint();
        (0, upper)
    }
}

impl<I> DoubleEndedIterator for FilterWatched<'_, I>
where
    I: DoubleEndedIterator,
    I::Item: Borrow<ResourceId>,
{
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter.rfind(|item| self.state.is_watched(item.borrow()))
    }
}

impl<I> FusedIterator for FilterWatched<'_, I>
where
    I: FusedIterator,
    I::Item: Borrow<ResourceId>,
{}

pub trait FilterWatchedExt {
    fn filter_watched(self, watcher: &ResourceWatcher) -> FilterWatched<'_, Self>
    where
        Self: Sized;
}

impl<I> FilterWatchedExt for I
where
    I: Iterator,
    I::Item: Borrow<ResourceId>,
{
    fn filter_watched(self, watcher: &ResourceWatcher) -> FilterWatched<'_, Self>
    where
        Self: Sized,
    {
        FilterWatched {
            iter: self,
            state: watcher.state.read(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ahash::HashMap;
    use rstest::{fixture, rstest};
    use test_log::test as test_log;

    #[derive(Debug)]
    enum WatchSpec {
        Watched(ResourceId),
        Unwatched(ResourceId),
    }

    impl WatchSpec {
        fn assert_matches(&self, watcher: &ResourceWatcher) {
            match self {
                Self::Watched(id) =>
                    assert!(watcher.is_watched(id), "{id} should be watched by {watcher:?}"),
                Self::Unwatched(id) =>
                    assert!(!watcher.is_watched(id), "{id} should NOT be watched by {watcher:?}"),
            }
        }
    }

    #[derive(Default, Debug)]
    struct WatcherSpec {
        source_idx: Option<SourceIndex>,
        watches: Vec<WatchSpec>,
    }

    impl WatcherSpec {
        fn source_idx(mut self, source_idx: SourceIndex) -> Self {
            self.source_idx = Some(source_idx.into());
            self
        }

        fn watch(mut self, watch: impl Into<ResourceId>) -> Self {
            self.watches.push(WatchSpec::Watched(watch.into()));
            self
        }

        fn unwatch(mut self, watch: impl Into<ResourceId>) -> Self {
            self.watches.push(WatchSpec::Unwatched(watch.into()));
            self
        }

        fn assert_matches(&self, watcher: &ResourceWatcher) {
            {
                let state = watcher.state.read();
                match &self.source_idx {
                    Some(expected_src_idx) => {
                        assert!(state.config.is_some(), "Watcher config should not be 'None': {watcher:?}");
                        assert_eq!(&state.config.as_ref().unwrap().src_idx, expected_src_idx);
                    },
                    None => {
                        assert!(!state.config.is_some(), "Watcher config should be 'None': {watcher:?}")
                    },
                }
            }

            for expected_watch in &self.watches {
                expected_watch.assert_matches(watcher);
            }
        }
    }

    trait WatcherSpecVecExt {
        fn assert_matches(&self, watchers: &Vec<ResourceWatcher>);
    }

    impl WatcherSpecVecExt for Vec<WatcherSpec> {
        fn assert_matches(&self, watchers: &Vec<ResourceWatcher>) {
            assert_eq!(watchers.len(), self.len());
            for (idx, watcher) in watchers.iter().enumerate() {
                self[idx].assert_matches(watcher);
            }
        }
    }

    struct TestHarness {
        watcher: ResourceWatcher,
        recv: Option<ResourceWatcherReceiver>,
        children: Vec<ResourceWatcher>,
    }

    impl Default for TestHarness {
        fn default() -> Self {
            Self {
                watcher: ResourceWatcher::new(()),
                recv: None,
                children: vec![],
            }
        }
    }

    impl TestHarness {
        fn configure(
            &mut self,
            configure_src_idx: Option<SourceIndex>,
        ) {
            if let Some(src_idx) = configure_src_idx {
                let (sender, recv) = update_channel();
                self.watcher.configure(ResourceWatcherConfig {
                    src_idx,
                    sender,
                });
                self.recv = Some(recv);
            }
        }

        fn create_children(&mut self, count: usize) {
            let base_idx = self.children.len();
            self.children.reserve(count);
            for idx in 0..count {
                let hashes = [(watch_id(), format!("hash-{}", base_idx + idx))]
                    .into_iter().collect::<HashMap<_, _>>();
                self.children.push(ResourceWatcher::new(hashes));
            }
        }

        fn add_all_children(&self) {
            for child in &self.children {
                self.watcher.push_child(child);
            }
        }

        fn drain_updates(&self) -> Vec<UpdateMsg> {
            if let Some(recv) = &self.recv {
                let mut updates = vec![];
                while let Some(update) = recv.try_recv().unwrap() {
                    updates.push(update);
                }
                updates
            } else {
                vec![]
            }
        }
    }

    #[fixture]
    fn harness(
        #[default(0)]
        child_count: usize,
    ) -> TestHarness {
        let mut harness = TestHarness::default();
        harness.create_children(child_count);
        harness
    }

    #[fixture]
    fn watch_id() -> ResourceId {
        ResourceId::from("resource-a")
    }

    #[rstest]
    #[case::after_unconfigured(
        1, false, None,
        WatcherSpec::default().watch("resource-a"),
        vec![WatcherSpec::default().watch("resource-a")],
        vec![],
    )]
    #[case::before_unconfigured(
        1, true, None,
        WatcherSpec::default().watch("resource-a"),
        vec![WatcherSpec::default().watch("resource-a")],
        vec![],
    )]
    #[case::one_after_configure(
        1, false, Some(SourceIndex::from(0)),
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![WatcherSpec::default().source_idx([0, 0].into()).watch("resource-a")],
        vec![
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-0".to_string()))]),
            ),
        ],
    )]
    #[case::one_before_configure(
        1, true, Some(SourceIndex::from(0)),
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![WatcherSpec::default().source_idx([0, 0].into()).watch("resource-a")],
        vec![
            (
                [0, 0].into(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-0".to_string()))]),
            ),
        ],
    )]
    #[case::many_after_configure(
        3, false, Some(SourceIndex::from(0)),
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![
            WatcherSpec::default().source_idx([0, 0].into()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 1].into()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 2].into()).watch("resource-a"),
        ],
        vec![
            (
                [0, 0].into(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-0".to_string()))]),
            ),
            (
                [0, 1].into(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-1".to_string()))]),
            ),
            (
                [0, 2].into(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-2".to_string()))]),
            ),
        ],
    )]
    #[case::many_before_configure(
        3, true, Some(SourceIndex::from(0)),
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![
            WatcherSpec::default().source_idx([0, 0].into()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 1].into()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 2].into()).watch("resource-a"),
        ],
        vec![
            (
                [0, 0].into(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-0".to_string()))]),
            ),
            (
                [0, 1].into(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-1".to_string()))]),
            ),
            (
                [0, 2].into(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-2".to_string()))]),
            ),
        ],
    )]
    #[test_attr(test_log)]
    fn test_push(
        mut harness: TestHarness,
        watch_id: ResourceId,
        #[case] child_count: usize,
        #[case] push_before: bool,
        #[case] configure_src_idx: Option<SourceIndex>,
        #[case] expected: WatcherSpec,
        #[case] expected_children: Vec<WatcherSpec>,
        #[case] expected_updates: Vec<(SourceIndex, HashMap<ResourceId, ResourceUpdate>)>,
    ) {
        harness.create_children(child_count);
        if push_before {
            for watcher in &harness.children {
                harness.watcher.push_child(watcher);
            }
            harness.watcher.watch(watch_id.clone(), None);
            harness.configure(configure_src_idx);
        } else {
            harness.watcher.watch(watch_id.clone(), None);
            harness.configure(configure_src_idx);
            for watcher in &harness.children {
                harness.watcher.push_child(watcher);
            }
        }
        expected.assert_matches(&harness.watcher);
        expected_children.assert_matches(&harness.children);

        for child in &harness.children {
            assert!(child.is_watched(&watch_id));
        }

        let updates = harness.drain_updates().into_iter()
            .map(|update| (update.src_idx, update.bulk_update.updates))
            .collect::<Vec<_>>();
        assert_eq!(updates, expected_updates);
    }

    #[rstest]
    #[case::after_unconfigured(
        1, false, None, vec![(0, 0)],
        WatcherSpec::default().watch("resource-a"),
        vec![WatcherSpec::default().watch("resource-a")],
        vec![],
    )]
    #[case::before_unconfigured(
        1, true, None, vec![(0, 0)],
        WatcherSpec::default().watch("resource-a"),
        vec![WatcherSpec::default().watch("resource-a")],
        vec![],
    )]
    #[case::one_after_configure(
        1, false, Some(SourceIndex::from(0)),
        vec![(0, 0)],
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![WatcherSpec::default().source_idx([0, 0].into_iter().collect()).watch("resource-a")],
        vec![
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-0".to_string()))]),
            ),
        ],
    )]
    #[case::one_before_configure(
        1, true, Some(SourceIndex::from(0)),
        vec![(0, 0)],
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![WatcherSpec::default().source_idx([0, 0].into_iter().collect()).watch("resource-a")],
        vec![
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-0".to_string()))]),
            ),
        ],
    )]
    #[case::many_after_configure(
        4, false, Some(SourceIndex::from(0)),
        vec![(0, 0), (1, 1), (2, 0), (3, 2)],
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![
            WatcherSpec::default().source_idx([0, 1].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 3].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 0].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 2].into_iter().collect()).watch("resource-a"),
        ],
        vec![
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-0".to_string()))]),
            ),
            (
                [0, 1].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-1".to_string()))]),
            ),
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-2".to_string()))]),
            ),
            (
                [0, 1].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::MovedSourceIndex([0, 0].into_iter().collect()))]),
            ),
            (
                [0, 2].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::MovedSourceIndex([0, 1].into_iter().collect()))]),
            ),
            (
                [0, 2].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-3".to_string()))]),
            ),
            (
                [0, 3].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::MovedSourceIndex([0, 2].into_iter().collect()))]),
            ),
        ],
    )]
    #[case::many_before_configure(
        4, true, Some(SourceIndex::from(0)),
        vec![(0, 0), (1, 1), (2, 0), (3, 2)],
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![
            WatcherSpec::default().source_idx([0, 1].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 3].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 0].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 2].into_iter().collect()).watch("resource-a"),
        ],
        vec![
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-2".to_string()))]),
            ),
            (
                [0, 1].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-0".to_string()))]),
            ),
            (
                [0, 2].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-3".to_string()))]),
            ),
        (
                [0, 3].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Added("hash-1".to_string()))]),
            ),
        ],
    )]
    fn test_insert(
        mut harness: TestHarness,
        watch_id: ResourceId,
        #[case] child_count: usize,
        #[case] insert_before: bool,
        #[case] configure_src_idx: Option<SourceIndex>,
        #[case] insert_indices: Vec<(usize, usize)>,
        #[case] expected: WatcherSpec,
        #[case] expected_children: Vec<WatcherSpec>,
        #[case] expected_updates: Vec<(SourceIndex, HashMap<ResourceId, ResourceUpdate>)>,
    ) {
        harness.create_children(child_count);
        if insert_before {
            for (child_idx, insert_idx) in insert_indices {
                harness.watcher.insert_child(insert_idx, &harness.children[child_idx]);
            }
            harness.watcher.watch(watch_id.clone(), None);
            harness.configure(configure_src_idx);
        } else {
            harness.watcher.watch(watch_id.clone(), None);
            harness.configure(configure_src_idx);
            for (child_idx, insert_idx) in insert_indices {
                harness.watcher.insert_child(insert_idx, &harness.children[child_idx]);
            }
        }
        expected.assert_matches(&harness.watcher);
        expected_children.assert_matches(&harness.children);

        for child in &harness.children {
            assert!(child.is_watched(&watch_id));
        }

        let updates = harness.drain_updates().into_iter()
            .map(|update| (update.src_idx, update.bulk_update.updates))
            .collect::<Vec<_>>();
        assert_eq!(updates, expected_updates);
    }

    #[rstest]
    #[case::unconfigured(
        None,
        vec![1],
        WatcherSpec::default().watch("resource-a"),
        vec![
            WatcherSpec::default().watch("resource-a"),
            WatcherSpec::default(),
            WatcherSpec::default().watch("resource-a"),
        ],
        vec![true, false, true],
        vec![],
    )]
    #[case::first(
        Some(SourceIndex::from(0)),
        vec![0],
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![
            WatcherSpec::default(),
            WatcherSpec::default().source_idx([0, 0].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 1].into_iter().collect()).watch("resource-a"),
        ],
        vec![false, true, true],
        vec![
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Removed)]),
            ),
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::MovedSourceIndex([0, 1].into_iter().collect()))]),
            ),
            (
                [0, 1].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::MovedSourceIndex([0, 2].into_iter().collect()))]),
            ),
        ],
    )]
    #[case::last(
        Some(SourceIndex::from(0)),
        vec![2],
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![
            WatcherSpec::default().source_idx([0, 0].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default().source_idx([0, 1].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default(),
        ],
        vec![true, true, false],
        vec![
            (
                [0, 2].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Removed)]),
            ),
        ],
    )]
    #[case::many(
        Some(SourceIndex::from(0)),
        vec![0, 1],
        WatcherSpec::default().source_idx(0.into()).watch("resource-a"),
        vec![
            WatcherSpec::default(),
            WatcherSpec::default().source_idx([0, 0].into_iter().collect()).watch("resource-a"),
            WatcherSpec::default(),
        ],
        vec![false, true, false],
        vec![
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Removed)]),
            ),
            (
                [0, 0].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::MovedSourceIndex([0, 1].into_iter().collect()))]),
            ),
            (
                [0, 1].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::MovedSourceIndex([0, 2].into_iter().collect()))]),
            ),
            (
                [0, 1].into_iter().collect(),
                HashMap::from_iter([(ResourceId::from("resource-a"), ResourceUpdate::Removed)]),
            ),
        ],
    )]
    fn test_remove(
        #[with(3)] mut harness: TestHarness,
        watch_id: ResourceId,
        #[case] configure_src_idx: Option<SourceIndex>,
        #[case] remove_indices: Vec<usize>,
        #[case] expected: WatcherSpec,
        #[case] expected_children: Vec<WatcherSpec>,
        #[case] expected_watches: Vec<bool>,
        #[case] expected_updates: Vec<(SourceIndex, HashMap<ResourceId, ResourceUpdate>)>,
    ) {
        harness.configure(configure_src_idx);
        harness.watcher.watch(watch_id.clone(), None);
        harness.add_all_children();
        // clear the updates for later assertion
        harness.drain_updates();
        for idx in remove_indices {
            harness.watcher.remove_child(idx);
        }
        expected.assert_matches(&harness.watcher);
        expected_children.assert_matches(&harness.children);

        for (idx, child) in harness.children.iter().enumerate() {
            assert_eq!(child.is_watched(&watch_id), expected_watches[idx]);
        }

        let updates = harness.drain_updates().into_iter()
            .map(|update| (update.src_idx, update.bulk_update.updates))
            .collect::<Vec<_>>();
        assert_eq!(updates, expected_updates);
    }

    #[derive(Clone, Copy)]
    enum HarnessOperation {
        Create {
            count: usize,
        },
        Push {
            child_idx: usize,
        },
        Insert {
            child_idx: usize,
            insert_idx: usize,
        },
        Remove {
            remove_idx: usize,
        }
    }

    impl HarnessOperation {
        fn apply(self, harness: &mut TestHarness) {
            match self {
                Self::Create { count } =>
                    harness.create_children(count),
                Self::Push { child_idx } =>
                    harness.watcher.push_child(&harness.children[child_idx]),
                Self::Insert { child_idx, insert_idx } =>
                    harness.watcher.insert_child(insert_idx, &harness.children[child_idx]),
                Self::Remove { remove_idx } =>
                    harness.watcher.remove_child(remove_idx),
            }
        }
    }

    #[rstest]
    #[case::unmodified(
        vec![],
        vec![
            WatcherSpec::default().source_idx([0, 0].into_iter().collect())
                .watch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 1].into_iter().collect())
                .unwatch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 2].into_iter().collect())
                .unwatch("resource-0")
                .unwatch("resource-1")
                .watch("resource-2"),
        ]
    )]
    #[case::push(
        vec![HarnessOperation::Create { count: 1 }, HarnessOperation::Push { child_idx: 3 }],
        vec![
            WatcherSpec::default().source_idx([0, 0].into_iter().collect())
                .watch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 1].into_iter().collect())
                .unwatch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 2].into_iter().collect())
                .unwatch("resource-0")
                .unwatch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 3].into_iter().collect())
                .unwatch("resource-0")
                .unwatch("resource-1")
                .unwatch("resource-2"),
        ]
    )]
    #[case::insert_last(
        vec![HarnessOperation::Create { count: 1 }, HarnessOperation::Insert { child_idx: 3, insert_idx: 3 }],
        vec![
            WatcherSpec::default().source_idx([0, 0].into_iter().collect())
                .watch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 1].into_iter().collect())
                .unwatch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 2].into_iter().collect())
                .unwatch("resource-0")
                .unwatch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 3].into_iter().collect())
                .unwatch("resource-0")
                .unwatch("resource-1")
                .unwatch("resource-2"),
        ]
    )]
    #[case::insert_middle(
        vec![
            HarnessOperation::Create { count: 2 },
            HarnessOperation::Insert { child_idx: 3, insert_idx: 1 },
            HarnessOperation::Insert { child_idx: 4, insert_idx: 3 },
        ],
        vec![
            WatcherSpec::default().source_idx([0, 0].into_iter().collect())
                .watch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 2].into_iter().collect())
                .unwatch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 4].into_iter().collect())
                .unwatch("resource-0")
                .unwatch("resource-1")
                .watch("resource-2"),
            // 4th child, but inserted at 2nd position
            WatcherSpec::default().source_idx([0, 1].into_iter().collect())
                .unwatch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            // 5th child, but inserted at (now) 4th position
            WatcherSpec::default().source_idx([0, 3].into_iter().collect())
                .unwatch("resource-0")
                .unwatch("resource-1")
                .watch("resource-2"),
        ]
    )]
    #[case::remove_last(
        vec![HarnessOperation::Remove { remove_idx: 2 }],
        vec![
            WatcherSpec::default().source_idx([0, 0].into_iter().collect())
                .watch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 1].into_iter().collect())
                .unwatch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default(),
        ],
    )]
    #[case::remove_first(
        vec![HarnessOperation::Remove { remove_idx: 0 }],
        vec![
            WatcherSpec::default(),
            WatcherSpec::default().source_idx([0, 0].into_iter().collect())
                .unwatch("resource-0")
                .watch("resource-1")
                .watch("resource-2"),
            WatcherSpec::default().source_idx([0, 1].into_iter().collect())
                .unwatch("resource-0")
                .unwatch("resource-1")
                .watch("resource-2"),
        ],
    )]
    fn test_partial_watch(
        #[with(3)] mut harness: TestHarness,
        #[case] operations: Vec<HarnessOperation>,
        #[case] expected_children: Vec<WatcherSpec>,
    ) {
        harness.add_all_children();
        harness.configure(Some(0.into()));
        harness.watcher.watch(ResourceId::from("resource-0"), Some(0.into()));
        harness.watcher.watch(ResourceId::from("resource-1"), Some(1.into()));
        harness.watcher.watch(ResourceId::from("resource-2"), Some(2.into()));

        for operation in operations {
            operation.apply(&mut harness);
        }

        expected_children.assert_matches(&harness.children);
    }
}