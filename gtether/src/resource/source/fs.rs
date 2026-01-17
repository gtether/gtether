//! [ResourceSources](ResourceSource) that provide resources from a filesystem.
//!
//! The main source found in this module is [ResourceSourceFiles], which contains sub-sources that
//! correspond to individual files in a filesystem. These sub-sources must implement
//! [ResourceSourceFile], which allows [ResourceSourceFiles] to maintain file watchers for its
//! sub-sources, and prompt them to reload and/or invalidate when their underlying files change.
//!
//! Internally, [ResourceSourceFiles] wraps [ResourceSourceDynamic], so the API ergonomics are
//! similar to that source.

use async_trait::async_trait;
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::Debouncer;
use parking_lot::Mutex;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use educe::Educe;
use tracing::{info, warn};

use crate::resource::id::ResourceId;
use crate::resource::source::dynamic::{ResourceSourceDynamic, ResourceSourceDynamicHandle, ResourceSourceDynamicReadGuard, ResourceSourceDynamicWriteGuard};
use crate::resource::source::{ResourceDataResult, ResourceDataSource, ResourceSource, SourceIndex};
use crate::resource::watcher::ResourceWatcher;

/// Trait representing a [ResourceSource] that corresponds to a single file in a filesystem.
pub trait ResourceSourceFile: ResourceSource {
    /// Path to the file this source represents.
    fn path(&self) -> &Path;

    /// Trigger a reload of this source.
    ///
    /// The source should re-read its file and update itself accordingly.
    fn reload(&self);

    /// Invalidate this source.
    ///
    /// This usually means that the underlying file has disappeared. It is up to the source to
    /// decide what to do in this situation, but it will usually clear any caches it has based on
    /// the previous read of the file.
    fn invalidate(&self);
}

impl<S: ResourceSourceFile + ?Sized> ResourceSourceFile for Box<S> {
    #[inline]
    fn path(&self) -> &Path {
        (**self).path()
    }

    #[inline]
    fn reload(&self) {
        (**self).reload()
    }

    #[inline]
    fn invalidate(&self) {
        (**self).invalidate()
    }
}

type FileWatcher = Debouncer<notify::RecommendedWatcher, notify_debouncer_full::RecommendedCache>;

/// Read guard for [ResourceSourceFiles].
///
/// Provides a read-only dereference to the `Vec<>` of children sources.
pub struct ResourceSourceFilesReadGuard<'a, S: ResourceSourceFile>(ResourceSourceDynamicReadGuard<'a, S>);

impl<'a, S: ResourceSourceFile> Deref for ResourceSourceFilesReadGuard<'a, S> {
    type Target = Vec<S>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

/// Write guard for [ResourceSourceFiles].
///
/// In addition to providing a read-only dereference to the `Vec<>` of children sources, this guard
/// also provides specific `mut` methods for inserting, pushing, and removing children.
pub struct ResourceSourceFilesWriteGuard<'a, S: ResourceSourceFile> {
    guard: ResourceSourceDynamicWriteGuard<'a, S>,
    file_watcher: &'a Mutex<FileWatcher>,
}

impl<'a, S: ResourceSourceFile> Deref for ResourceSourceFilesWriteGuard<'a, S> {
    type Target = Vec<S>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl<'a, S: ResourceSourceFile> ResourceSourceFilesWriteGuard<'a, S> {
    /// Inserts a source at position `idx`, shifting all sources after it to the right.
    ///
    /// See also: [`Vec::insert()`].
    ///
    /// # Panics
    ///
    /// Panics if `idx` > `len`.
    pub fn insert(&mut self, idx: usize, file_source: S) {
        let path = file_source.path();
        if let Err(error) = self.file_watcher.lock().watch(path, RecursiveMode::NonRecursive) {
            warn!(?path, ?error, "Failed to watch file_source path; resource source may not update when file changes");
        }
        self.guard.insert(idx, file_source);
    }

    /// Appends a source to the back of the collection.
    ///
    /// See also: [`Vec::push()`].
    pub fn push(&mut self, file_source: S) {
        let path = file_source.path();
        if let Err(error) = self.file_watcher.lock().watch(path, RecursiveMode::NonRecursive) {
            warn!(?path, ?error, "Failed to watch file_source path; resource source may not update when file changes");
        }
        self.guard.push(file_source);
    }

    /// Removes the source at position `idx`, shifting all sources after it to the left.
    ///
    /// See also: [`Vec::remove()`].
    ///
    /// # Panics
    ///
    /// Panics if `idx` is out of bounds.
    pub fn remove(&mut self, idx: usize) -> S {
        let file_source = self.guard.remove(idx);
        let path = file_source.path();
        if let Err(error) = self.file_watcher.lock().unwatch(path) {
            warn!(?path, ?error, "Failed to UN-watch file_source path; expect spurious resource update events");
        }
        file_source
    }
}

impl<'a> ResourceSourceFilesWriteGuard<'a, Box<dyn ResourceSourceFile>> {
    /// Inserts a source dynamically.
    ///
    /// See [`Self::insert()`] for more.
    #[inline]
    pub fn insert_dyn(&mut self, idx: usize, file_source: impl ResourceSourceFile) {
        self.insert(idx, Box::new(file_source))
    }

    /// Appends a source dynamically.
    ///
    /// See [`Self::push()`] for more.
    #[inline]
    pub fn push_dyn(&mut self, file_source: impl ResourceSourceFile) {
        self.push(Box::new(file_source))
    }
}

/// Handle to a [ResourceSourceFiles] that allows modification of the children resource sources.
///
/// Use the
/// [`read()`](ResourceSourceFiles::read)/[`write()`](ResourceSourceFiles::write)
/// methods to get actual access to the resource sources.
///
/// This handle is cheaply cloneable (it just clones the `Arc<>` it wraps).
#[derive(Educe)]
#[educe(Clone)]
pub struct ResourceSourceFilesHandle<S: ResourceSourceFile> {
    inner: ResourceSourceDynamicHandle<S>,
    file_watcher: Arc<Mutex<FileWatcher>>,
}

impl<S: ResourceSourceFile> ResourceSourceFilesHandle<S> {
    /// Get read access to the children resource sources.
    pub fn read(&self) -> ResourceSourceFilesReadGuard<'_, S> {
        ResourceSourceFilesReadGuard(self.inner.read())
    }

    /// Get write access to the children resource sources.
    ///
    /// See [ResourceSourceFilesWriteGuard] for specialized insert/push/remove methods.
    pub fn write(&self) -> ResourceSourceFilesWriteGuard<'_, S> {
        ResourceSourceFilesWriteGuard {
            guard: self.inner.write(),
            file_watcher: &self.file_watcher,
        }
    }
}

/// [ResourceSource] that contains a dynamic sub-set of file-based resource sources.
///
/// See [module-level](super::fs) documentation for more.
///
/// ```
/// use gtether::resource::source::fs::ResourceSourceFiles;
///
/// let sources: ResourceSourceFiles = Default::default();
/// ```
pub struct ResourceSourceFiles<S: ResourceSourceFile = Box<dyn ResourceSourceFile>> {
    inner: ResourceSourceDynamic<S>,
    file_watcher: Arc<Mutex<FileWatcher>>,
}

impl<S: ResourceSourceFile> Default for ResourceSourceFiles<S> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<S: ResourceSourceFile> ResourceSourceFiles<S> {
    fn reload_file_path(handle: &ResourceSourceDynamicHandle<S>, path: &Path) {
        let sources = handle.read();
        for (idx, source) in sources.iter().enumerate() {
            if source.path() == path {
                info!(?idx, ?path, "Reloading file-based source");
                source.reload();
            }
        }
    }

    fn invalidate_file_path(handle: &ResourceSourceDynamicHandle<S>, path: &Path) {
        let sources = handle.read();
        for (idx, source) in sources.iter().enumerate() {
            if source.path() == path {
                warn!(?idx, ?path, "File-based source removed, resource data will be invalidated");
                source.invalidate();
            }
        }
    }

    fn new() -> Self {
        let inner = ResourceSourceDynamic::default();
        let handle = inner.handle();

        let file_watcher = notify_debouncer_full::new_debouncer(
            Duration::from_secs(1),
            None,
            move |result: notify_debouncer_full::DebounceEventResult| {
                let events = match result {
                    Ok(events) => events,
                    Err(errors) => {
                        warn!(?errors, "Errors in debounced events");
                        return
                    }
                };

                for mut event in events {
                    match event.kind {
                        EventKind::Create(_) |
                        EventKind::Modify(ModifyKind::Data(_)) => {
                            for path in &event.paths {
                                Self::reload_file_path(&handle, path);
                            }
                        },
                        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                            if event.paths.len() == 1 {
                                Self::reload_file_path(&handle, &event.paths.pop().unwrap());
                            } else {
                                warn!(
                                    expected = 1,
                                    actual = event.paths.len(),
                                    ?event,
                                    "File watch RenameTo event contained an unexpected number of paths; ignoring",
                                );
                            }
                        },
                        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                            if event.paths.len() == 1 {
                                Self::invalidate_file_path(&handle, &event.paths.pop().unwrap());
                            } else {
                                warn!(
                                    expected = 1,
                                    actual = event.paths.len(),
                                    ?event,
                                    "File watch RenameFrom event contained an unexpected number of paths; ignoring",
                                );
                            }
                        },
                        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                            if event.paths.len() == 2 {
                                let [from, to] = <[PathBuf; 2]>::try_from(event.event.paths).unwrap();
                                Self::invalidate_file_path(&handle, &from);
                                Self::reload_file_path(&handle, &to);
                            } else {
                                warn!(
                                    expected = 2,
                                    actual = event.paths.len(),
                                    ?event,
                                    "File watch RenameBoth event contained an unexpected number of paths; ignoring",
                                );
                            }
                        },
                        EventKind::Remove(_) => {
                            for path in &event.paths {
                                Self::invalidate_file_path(&handle, path);
                            }
                        },
                        _ => {}
                    }
                }
            },
        ).expect("Initial file_watcher config should be valid");
        let file_watcher = Arc::new(Mutex::new(file_watcher));

        Self {
            inner,
            file_watcher,
        }
    }

    /// Get a [handle](ResourceSourceFilesHandle) that allows modifying the children resource
    /// sources.
    #[inline]
    pub fn handle(&self) -> ResourceSourceFilesHandle<S> {
        ResourceSourceFilesHandle {
            inner: self.inner.handle(),
            file_watcher: self.file_watcher.clone(),
        }
    }
}

#[async_trait]
impl<S: ResourceSourceFile> ResourceSource for ResourceSourceFiles<S> {
    #[inline]
    fn hash(&self, id: &ResourceId) -> Option<ResourceDataSource> {
        self.inner.hash(id)
    }

    #[inline]
    async fn load(&self, id: &ResourceId) -> ResourceDataResult {
        self.inner.load(id).await
    }

    #[inline]
    async fn sub_load(&self, id: &ResourceId, sub_idx: &SourceIndex) -> ResourceDataResult {
        self.inner.sub_load(id, sub_idx).await
    }

    #[inline]
    fn watcher(&self) -> &ResourceWatcher {
        self.inner.watcher()
    }
}