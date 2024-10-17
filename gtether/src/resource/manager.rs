use parking_lot::{Condvar, Mutex, RwLock};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::Read;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Weak};
use std::thread;
use std::thread::JoinHandle;
use tracing::{event, Level};

use crate::resource::path::ResourcePath;
use crate::resource::source::{ResourceDataResult, ResourceSource, SourceIndex};
use crate::resource::{Resource, ResourceLoadError, ResourceLoader, ResourceMut};

#[derive(Debug, Clone)]
pub struct ResourceWatcher {
    manager: Weak<ResourceManager>,
    idx: SourceIndex,
}

impl ResourceWatcher {
    #[inline]
    pub fn notify_update(&self, id: &ResourcePath) {
        if let Some(manager) = self.manager.upgrade() {
            manager.notify_update(id.clone(), self.idx.clone());
        }
    }

    #[inline]
    pub fn clone_with_sub_index(&self, sub_idx: impl Into<SourceIndex>) -> Self {
        let sub_idx = sub_idx.into();
        Self {
            manager: self.manager.clone(),
            idx: self.idx.clone().with_sub_idx(Some(sub_idx)),
        }
    }
}

pub type ResourceLoadResult<T> = Result<Arc<Resource<T>>, ResourceLoadError>;

pub enum ResourceFuture<T: Send + Sync + 'static> {
    Immediate(ResourceLoadResult<T>),
    Delayed(Arc<(Mutex<Option<ResourceLoadResult<T>>>, Condvar)>),
}

impl<T: Send + Sync + 'static> ResourceFuture<T> {
    pub fn get(&self) -> Option<ResourceLoadResult<T>> {
        match self {
            ResourceFuture::Immediate(inner) => Some(inner.clone()),
            ResourceFuture::Delayed(pair) => {
                let &(ref lock, ref _cvar) = &**pair;
                lock.lock().clone()
            },
        }
    }

    pub fn unwrap(self) -> ResourceLoadResult<T> {
        match self {
            ResourceFuture::Immediate(inner) => inner,
            ResourceFuture::Delayed(pair) => {
                let &(ref lock, ref cvar) = &*pair;
                let mut guard = lock.lock();
                if let Some(result) = guard.as_ref() {
                    result.clone()
                } else {
                    cvar.wait_while(&mut guard, |result| {
                        result.is_none()
                    });
                    guard.clone().unwrap()
                }
            },
        }
    }
}

// Manual implementation because derive(Clone) requires generics implement Clone
impl<T: Send + Sync + 'static> Clone for ResourceFuture<T> {
    fn clone(&self) -> Self {
        match self {
            ResourceFuture::Immediate(inner) =>
                ResourceFuture::Immediate(inner.clone()),
            ResourceFuture::Delayed(inner) =>
                ResourceFuture::Delayed(inner.clone()),
        }
    }
}

enum CacheEntry {
    Loading(Arc<dyn Any + Send + Sync>),
    CachedValue {
        value: Weak<dyn Any + Send + Sync>,
        source_idx: SourceIndex,
        update: Box<dyn (Fn(Box<dyn Read>) -> Result<(), ResourceLoadError>) + Send + Sync>,
    },
    CachedError(ResourceLoadError),
}

impl CacheEntry {
    fn for_get_result<T>(result: ResourceFuture<T>) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self::Loading(Arc::new(result))
    }

    fn for_resource<T, L>(
        id: &ResourcePath,
        resource: Arc<Resource<T>>,
        loader: L,
        source_idx: SourceIndex,
    ) -> Self
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T> + 'static,
    {
        let update_id = id.clone();
        let weak = Arc::downgrade(&resource);
        Self::CachedValue {
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
            source_idx,
        }
    }

    fn for_load_error(load_error: ResourceLoadError) -> Self {
        Self::CachedError(load_error)
    }

    fn for_load_result<T, L>(
        id: &ResourcePath,
        load_result: Result<Arc<Resource<T>>, ResourceLoadError>,
        loader: L,
        source_idx: SourceIndex,
    ) -> Self
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T> + 'static,
    {
        match load_result {
            Ok(resource) => {
                Self::for_resource(id, resource, loader, source_idx)
            },
            Err(error) => {
                Self::for_load_error(error)
            },
        }

    }

    fn downcast<T>(&self) -> Option<Arc<ResourceFuture<T>>>
    where
        T: Send + Sync + 'static,
    {
        match self {
            CacheEntry::Loading(any) => {
                match any.clone().downcast::<ResourceFuture<T>>() {
                    Ok(result) => Some(result.clone()),
                    Err(any) => Some(Arc::new(ResourceFuture::Immediate(
                        Err(ResourceLoadError::TypeMismatch {
                            requested: TypeId::of::<ResourceFuture<T>>(),
                            actual: (&*any).type_id(),
                        })
                    )))
                }
            },
            CacheEntry::CachedValue { value, .. } => {
                if let Some(strong) = value.upgrade() {
                    let result = match strong.downcast::<Resource<T>>() {
                        Ok(resource) => Ok(resource),
                        Err(any) => Err(ResourceLoadError::TypeMismatch {
                            requested: TypeId::of::<Resource<T>>(),
                            actual: (&*any).type_id(),
                        }),
                    };
                    Some(Arc::new(ResourceFuture::Immediate(result)))
                } else {
                    None
                }
            },
            CacheEntry::CachedError(error) => {
                Some(Arc::new(ResourceFuture::Immediate(Err(error.clone()))))
            },
        }
    }

    fn set_source_idx(&mut self, new_idx: SourceIndex) {
        match self {
            CacheEntry::CachedValue { source_idx, .. } => {
                *source_idx = new_idx;
            },
            _ => {},
        }
    }
}

struct ResourceCache {
    inner: RwLock<HashMap<ResourcePath, CacheEntry>>,
}

impl Default for ResourceCache {
    fn default() -> Self {
        Self {
            inner: RwLock::new(HashMap::default()),
        }
    }
}

impl ResourceCache {
    fn insert<T, L>(
        &self,
        id: ResourcePath,
        load_result: ResourceLoadResult<T>,
        loader: L,
        source_idx: SourceIndex,
    ) where
        T: Send + Sync + 'static,
        L: ResourceLoader<T> + 'static,
    {
        let entry = CacheEntry::for_load_result(&id, load_result, loader, source_idx);
        self.inner.write().insert(id, entry);
    }

    fn get<T>(&self, id: &ResourcePath) -> Option<ResourceFuture<T>>
    where
        T: Send + Sync + 'static,
    {
        let mut read = self.inner.upgradable_read();
        if let Some(entry) = read.get(id) {
            if let Some(result) = entry.downcast() {
                Some((*result).clone())
            } else {
                // Expired so remove the entry
                read.with_upgraded(|write| {
                    write.remove(id);
                });
                None
            }
        } else {
            None
        }
    }

    fn get_or_reserve<T>(&self, id: &ResourcePath) -> Result<ResourceFuture<T>,
        (ResourceFuture<T>, Arc<(Mutex<Option<ResourceLoadResult<T>>>, Condvar)>)>
    where
        T: Send + Sync + 'static,
    {
        let mut read = self.inner.upgradable_read();
        if let Some(entry) = read.get(id) {
            if let Some(result) = entry.downcast() {
                Ok((*result).clone())
            } else {
                // Expired, so remove the entry
                read.with_upgraded(|write| {
                    write.remove(id);
                    let output_pair = Arc::new((Mutex::new(None), Condvar::new()));
                    let result = ResourceFuture::Delayed(output_pair.clone());
                    write.insert(id.clone(), CacheEntry::for_get_result(result.clone()));
                    Err((result, output_pair))
                })
            }
        } else {
            read.with_upgraded(|write| {
                let output_pair = Arc::new((Mutex::new(None), Condvar::new()));
                let result = ResourceFuture::Delayed(output_pair.clone());
                write.insert(id.clone(), CacheEntry::for_get_result(result.clone()));
                Err((result, output_pair))
            })
        }
    }

    fn update(&self, id: &ResourcePath, source: &Box<dyn ResourceSource>, new_idx: &SourceIndex)
              -> Result<(), ResourceLoadError>
    {
        let mut read = self.inner.upgradable_read();
        if let Some(entry) = read.get(id) {
            match entry {
                CacheEntry::Loading(_) => {
                    event!(Level::WARN, %id, "Tried to update currently loading Resource");
                },
                CacheEntry::CachedValue { source_idx, update, .. } => {
                    if new_idx <= source_idx {
                        let data = match new_idx.sub_idx() {
                            Some(sub_idx) => source.sub_load(id, sub_idx),
                            None => source.load(id),
                        }?.seal(new_idx.idx());
                        update(data.data)?;
                        read.with_upgraded(|write| {
                            write.get_mut(id).unwrap().set_source_idx(data.source_idx);
                        });
                    } else {
                        event!(Level::WARN, %id, %new_idx, %source_idx, "Tried to update Resource from lower priority source");
                    }
                },
                CacheEntry::CachedError { .. } => {
                    read.with_upgraded(|write| {
                        write.remove(id);
                    });
                }
            }
        } else {
            event!(Level::WARN, %id, "Tried to update expired Resource");
        }
        Ok(())
    }
}

enum ManagerMessage {
    LoadTask(Box<dyn FnOnce() + Send>),
    UpdateTask {
        id: ResourcePath,
        source_idx: SourceIndex,
    },
    Stop,
}

pub struct ResourceManager {
    sources: Vec<Box<dyn ResourceSource>>,
    cache: ResourceCache,
    weak: Weak<Self>,
    join_handle: Option<JoinHandle<()>>,
    tx: Sender<ManagerMessage>,
}

impl ResourceManager {
    fn new(sources: Vec<Box<dyn ResourceSource>>) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<ResourceManager>| {
            let task_weak = weak.clone();
            let (tx, rx) = channel();
            let join_handle = Some(thread::spawn(move || {
                for msg in rx {
                    match msg {
                        ManagerMessage::LoadTask(task) => task(),
                        ManagerMessage::UpdateTask { id, source_idx } => {
                            let manager = task_weak.upgrade()
                                .expect("ResourceManager background thread weak self-ref should not be broken");
                            if let Err(err) = manager.update(&id, source_idx.clone()) {
                                event!(Level::WARN, %id, %source_idx, %err, "Failed to update resource from source");
                            }
                        },
                        ManagerMessage::Stop => break,
                    }
                }
            }));

            Self {
                sources,
                cache: ResourceCache::default(),
                weak: weak.clone(),
                join_handle,
                tx,
            }
        })
    }

    #[inline]
    pub fn builder() -> ResourceManagerBuilder {
        ResourceManagerBuilder::default()
    }

    fn watch_n(&self, id: &ResourcePath, source_idx: &SourceIndex) {
        for (idx, source) in self.sources.iter().enumerate() {
            if idx <= source_idx.idx() {
                source.watch(id.clone(), ResourceWatcher {
                    manager: self.weak.clone(),
                    idx: SourceIndex::new(idx),
                }, source_idx.sub_idx().map(|inner| inner.clone()));
            } else {
                source.unwatch(id);
            }
        }
    }

    fn update(&self, id: &ResourcePath, source_idx: SourceIndex) -> Result<(), ResourceLoadError> {
        if let Some(source) = self.sources.get(source_idx.idx()) {
            self.cache.update(id, &source, &source_idx)?;
            self.watch_n(id, &source_idx);
        } else {
            event!(Level::WARN, %id, %source_idx, "Tried to update from non-existent source");
        }
        Ok(())
    }

    fn notify_update(&self, id: ResourcePath, source_idx: SourceIndex) {
        self.tx.send(ManagerMessage::UpdateTask { id, source_idx })
            .expect("ResourceManager background thread should be running");
    }

    fn find_data(&self, id: &ResourcePath) -> Option<ResourceDataResult> {
        self.sources.iter().enumerate().find_map(|(idx, source)| {
            match source.load(id) {
                Ok(data) => Some(Ok(data.seal(idx))),
                Err(ResourceLoadError::NotFound(_)) => None,
                Err(e) => Some(Err(e)),
            }
        })
    }

    fn load<T, L>(&self, id: ResourcePath, loader: L) -> ResourceLoadResult<T>
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T> + 'static,
    {
        if let Some(result) = self.find_data(&id) {
            let data = result?;
            let load_result = loader.load(data.data)
                .map(|value| Arc::new(Resource::new(value)));
            self.cache.insert(id.clone(), load_result.clone(), loader, data.source_idx.clone());
            self.watch_n(&id, &data.source_idx);
            load_result
        } else {
            Err(ResourceLoadError::NotFound(id))
        }
    }

    pub fn get<T>(&self, id: impl Into<ResourcePath>) -> Option<ResourceFuture<T>>
    where
        T: Send + Sync + 'static,
    {
        let id = id.into();
        self.cache.get(&id)
    }

    pub fn get_or_load<T, L>(&self, id: impl Into<ResourcePath>, loader: L) -> ResourceFuture<T>
    where
        T: Send + Sync + 'static,
        L: ResourceLoader<T> + 'static,
    {
        let id = id.into();
        match self.cache.get_or_reserve(&id) {
            Ok(cache_result) => cache_result,
            Err((output_result, output_pair)) => {
                let task_self = self.weak.upgrade()
                    .expect("ResourceManager cyclic reference should not be broken");

                let load_task = Box::new(move || {
                    let &(ref lock, ref cvar) = &*output_pair;
                    let result = task_self.load(id, loader);
                    let mut output = lock.lock();
                    *output = Some(result);
                    cvar.notify_all();
                });
                self.tx.send(ManagerMessage::LoadTask(load_task))
                    .expect("ResourceManager background thread should be running");

                output_result
            }
        }
    }
}

impl Drop for ResourceManager {
    fn drop(&mut self) {
        match self.tx.send(ManagerMessage::Stop) {
            Ok(()) => (),
            Err(error) =>
                event!(Level::WARN, %error, "ResourceManager message channel errored when dropping"),
        }

        if let Some(join_handle) = self.join_handle.take() {
            match join_handle.join() {
                Ok(()) => (),
                Err(error) =>
                    event!(Level::WARN, ?error, "ResourceManager background thread errored"),
            }
        } else {
            event!(Level::WARN, "ResourceManager join handle already taken");
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

#[cfg(test)]
pub(in crate::resource) mod tests {
    use super::*;

    use crate::resource::source::ResourceSubDataResult;

    pub struct SyncReqContext(ump::WaitReply<(), ()>);

    impl SyncReqContext {
        pub fn wait(self) {
            self.0.wait().unwrap();
        }
    }

    impl Into<SyncReqContext> for ump::WaitReply<(), ()> {
        fn into(self) -> SyncReqContext {
            SyncReqContext(self)
        }
    }

    pub struct SyncReplyContext(Option<ump::ReplyContext<(), ()>>);

    impl Drop for SyncReplyContext {
        fn drop(&mut self) {
            let ctx = self.0.take().unwrap();
            ctx.reply(()).unwrap();
        }
    }

    impl Into<SyncReplyContext> for ump::ReplyContext<(), ()> {
        fn into(self) -> SyncReplyContext {
            SyncReplyContext(Some(self))
        }
    }

    pub struct LoaderSync {
        load_server: ump::Server<(), (), ()>,
        load_client: ump::Client<(), (), ()>,
        update_server: ump::Server<(), (), ()>,
        update_client: ump::Client<(), (), ()>,
    }

    impl LoaderSync {
        pub fn new() -> Self {
            let (load_server, load_client) = ump::channel();
            let (update_server, update_client) = ump::channel();
            Self {
                load_server,
                load_client,
                update_server,
                update_client,
            }
        }

        pub fn wait_load(&self) -> SyncReplyContext {
            let (_, ctx) = self.load_server.wait().unwrap();
            ctx.into()
        }

        pub fn notify_load(&self) -> SyncReqContext {
            // TODO: Add a timeout
            self.load_client.req_async(()).unwrap().into()
        }

        pub fn wait_update(&self) -> SyncReplyContext {
            let (_, ctx) = self.update_server.wait().unwrap();
            ctx.into()
        }

        pub fn notify_update(&self) -> SyncReqContext {
            // TODO: Add a timeout
            self.update_client.req_async(()).unwrap().into()
        }
    }

    pub struct TestResourceLoader {
        sync: Arc<LoaderSync>,
    }

    impl TestResourceLoader {
        pub fn new() -> (Self, Arc<LoaderSync>) {
            let loader = Self {
                sync: Arc::new(LoaderSync::new()),
            };
            let sync = loader.sync.clone();
            (loader, sync)
        }
    }

    impl ResourceLoader<String> for TestResourceLoader {
        fn load(&self, mut data: Box<dyn Read>) -> Result<String, ResourceLoadError> {
            let mut output = String::new();
            let _sync_ctx = self.sync.wait_load();
            data.read_to_string(&mut output)
                .map_err(|err| ResourceLoadError::from_error(err))
                .map(|_| output)
        }

        fn update(&self, resource: ResourceMut<String>, mut data: Box<dyn Read>) -> Result<(), ResourceLoadError> {
            let mut output = String::new();
            let _sync_ctx = self.sync.wait_update();
            data.read_to_string(&mut output)
                .map_err(|err| ResourceLoadError::from_error(err))?;
            *resource.write() = output;
            Ok(())
        }
    }

    pub struct ResourceDataMap {
        raw: RwLock<HashMap<ResourcePath, &'static [u8]>>,
        watch_list: RwLock<HashMap<ResourcePath, ResourceWatcher>>,
    }

    impl ResourceDataMap {
        pub fn new() -> Self {
            Self {
                raw: RwLock::new(HashMap::new()),
                watch_list: RwLock::new(HashMap::new()),
            }
        }

        fn get(&self, id: &ResourcePath) -> Option<&'static [u8]> {
            self.raw.read().get(id).map(|r| *r)
        }

        pub fn insert(&self, id: impl Into<ResourcePath>, data: &'static [u8]) {
            let id = id.into();
            self.raw.write().insert(id.clone(), data);
            if let Some(watcher) = self.watch_list.read().get(&id) {
                watcher.notify_update(&id);
            }
        }

        fn watch(&self, id: ResourcePath, watcher: ResourceWatcher) {
            self.watch_list.write().insert(id, watcher);
        }

        fn unwatch(&self, id: &ResourcePath) {
            self.watch_list.write().remove(id);
        }
    }
    
    pub struct TestResourceSource {
        inner: Arc<ResourceDataMap>,
    }

    impl TestResourceSource {
        pub fn new() -> Self {
            Self {
                inner: Arc::new(ResourceDataMap::new()),
            }
        }

        pub fn data_map(&self) -> &Arc<ResourceDataMap> { &self.inner }
    }
    
    impl ResourceSource for TestResourceSource {
        fn load(&self, id: &ResourcePath) -> ResourceSubDataResult {
            if let Some(data) = self.inner.get(id) {
                Ok(Box::new(data).into())
            } else {
                Err(ResourceLoadError::NotFound(id.clone()))
            }
        }

        fn watch(&self, id: ResourcePath, watcher: ResourceWatcher, _sub_idx: Option<SourceIndex>) {
            self.inner.watch(id, watcher);
        }

        fn unwatch(&self, id: &ResourcePath) {
            self.inner.unwatch(id);
        }
    }

    pub fn create_resource_manager() -> (Arc<ResourceManager>, Arc<ResourceDataMap>) {
        let source = TestResourceSource::new();
        let data_map = source.data_map().clone();
        let manager = ResourceManager::builder()
            .source(source)
            .build();
        (manager, data_map)
    }

    pub fn create_multi_resource_manager<const N: usize>() -> (Arc<ResourceManager>, [Arc<ResourceDataMap>; N]) {
        let sources = core::array::from_fn::<_, N, _>(|_| TestResourceSource::new());
        let data_maps = core::array::from_fn(|i| sources[i].data_map().clone());
        let mut builder = ResourceManager::builder();
        for source in sources {
            builder = builder.source(source);
        }
        let manager = builder.build();
        (manager, data_maps)
    }

    #[test]
    fn test_resource_manager_not_found() {
        let (manager, _) = create_resource_manager();

        let (loader, _) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader);
        let err = result.unwrap()
            .expect_err("No resource data should be present for 'key'");
        assert_matches!(err, ResourceLoadError::NotFound(id) => {
            assert_eq!(id, ResourcePath::new("key"));
        });
    }

    #[test]
    fn test_resource_manager_load_error() {
        let (manager, data_map) = create_resource_manager();
        data_map.insert("invalid", b"\xC0");

        let (loader, sync) = TestResourceLoader::new();
        let result = manager.get_or_load("invalid", loader);
        assert_matches!(result.get(), None);

        sync.notify_load().wait();
        let error = result.unwrap()
            .expect_err("Resource should fail to load for 'invalid'");
        assert_matches!(error, ResourceLoadError::ReadError(_));

        // Second retrieval should immediately return with the same load error
        let (loader, _) = TestResourceLoader::new();
        let result = manager.get_or_load("invalid", loader).get();
        assert_matches!(result, Some(load_result) => {
            let error = load_result
                .expect_err("Second get should still return load error for 'invalid'");
            assert_matches!(error, ResourceLoadError::ReadError(_));
        });
    }

    #[test]
    fn test_resource_manager_found() {
        let (manager, data_map) = create_resource_manager();
        data_map.insert("key", b"value");

        let (loader, sync) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader);
        assert_matches!(result.get(), None);

        sync.notify_load().wait();
        let value = result.unwrap()
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "value".to_owned());

        // Second retrieval should immediately return with the same value
        let (loader, _) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader).get();
        assert_matches!(result, Some(load_result) => {
            let value = load_result
                .expect("Second get should still return value for 'key'");
            assert_eq!(*value.read(), "value".to_owned());
        });
    }

    #[test]
    fn test_resource_manager_update_value_to_value() {
        let (manager, data_map) = create_resource_manager();
        data_map.insert("key", b"value");

        let (loader, sync) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader);
        sync.notify_load().wait();
        let value = result.unwrap()
            .expect("Resource should load for 'key'");

        data_map.insert("key", b"new_value");
        assert_eq!(*value.read(), "value".to_owned());
        sync.notify_update().wait();
        assert_eq!(*value.read(), "new_value".to_owned());
    }

    #[test]
    fn test_resource_manager_update_value_to_error() {
        let (manager, data_map) = create_resource_manager();
        data_map.insert("key", b"value");

        let (loader, sync) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader);
        sync.notify_load().wait();
        let value = result.unwrap()
            .expect("Resource should load for 'key'");

        data_map.insert("key", b"\xC0");
        assert_eq!(*value.read(), "value".to_owned());
        sync.notify_update().wait();
        // Causing an error in an update shouldn't change the value
        assert_eq!(*value.read(), "value".to_owned());
    }

    #[test]
    fn test_resource_manager_update_error_to_value() {
        let (manager, data_map) = create_resource_manager();
        data_map.insert("key", b"\xC0");

        let (loader, sync) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader);
        sync.notify_load().wait();
        let error = result.unwrap()
            .expect_err("Resource should fail to load for 'key'");
        assert_matches!(error, ResourceLoadError::ReadError(_));

        data_map.insert("key", b"new_value");
        // Wait for entry to be cleared from the manager
        // TODO: timeout?
        while manager.get::<String>("key").is_some() {}

        let (loader, sync) = TestResourceLoader::new();
        let result = manager.get_or_load("key", loader);
        sync.notify_load().wait();
        let value = result.unwrap()
            .expect("Resource should load for 'key'");
        assert_eq!(*value.read(), "new_value".to_owned());
    }

    #[test]
    fn test_resource_manager_priorities() {
        let (manager, data_maps) = create_multi_resource_manager::<3>();
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