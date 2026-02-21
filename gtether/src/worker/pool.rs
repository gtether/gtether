use ahash::HashMap;
use crossbeam::channel::TryRecvError;
use educe::Educe;
use parking_lot::{Mutex, RwLock, RwLockWriteGuard};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::num::{NonZero, NonZeroUsize};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Wake, Waker};
use std::thread::{JoinHandle, Thread, ThreadId};
use thread_local::ThreadLocal;

use crate::util::priority::HasStaticPriority;
use crate::worker::{FindWorkError, FindWorkResult, Work, WorkSource};

struct WorkSourceBucketItem {
    source: Box<dyn WorkSource>,
    expired: AtomicBool,
}

impl WorkSourceBucketItem {
    fn find_work(&self) -> FindWorkResult {
        let result = self.source.find_work();
        if let Err(FindWorkError::Disconnected) = &result {
            self.expired.store(true, Ordering::Release);
        }
        result
    }
}

#[derive(Educe)]
#[educe(Default)]
struct Bucket<T> {
    next_idx: ThreadLocal<AtomicUsize>,
    values: Vec<T>,
}

impl<T> Bucket<T> {
    fn iter_ring(&self) -> BucketIterRing<'_, T> {
        if self.values.is_empty() {
            BucketIterRing {
                end_idx: Some(0),
                bucket: self,
            }
        } else {
            BucketIterRing {
                end_idx: None,
                bucket: self,
            }
        }
    }
}

impl Bucket<WorkSourceBucketItem> {
    fn clear_expired(&mut self) {
        self.next_idx.clear();
        self.values.retain(|item| !item.expired.load(Ordering::Acquire));
    }
}

struct BucketIterRing<'a, T> {
    end_idx: Option<usize>,
    bucket: &'a Bucket<T>,
}

impl<'a, T> Iterator for BucketIterRing<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        let atomic_next_idx = self.bucket.next_idx.get_or_default();
        let mut next_idx = atomic_next_idx.load(Ordering::Acquire);
        if let Some(end_idx) = self.end_idx && next_idx == end_idx {
            None
        } else {
            if self.end_idx.is_none() {
                self.end_idx = Some(next_idx);
            }
            let value = &self.bucket.values[next_idx];
            next_idx += 1;
            if next_idx >= self.bucket.values.len() {
                next_idx = 0;
            }
            atomic_next_idx.store(next_idx, Ordering::Release);
            Some(value)
        }
    }
}

impl<T> FromIterator<T> for Bucket<T> {
    fn from_iter<II: IntoIterator<Item=T>>(iter: II) -> Self {
        Self {
            next_idx: Default::default(),
            values: iter.into_iter().collect(),
        }
    }
}

#[derive(Educe)]
#[educe(Default)]
struct State<P: HasStaticPriority + Send + Sync + 'static> {
    work_sources: RwLock<BTreeMap<P, Bucket<WorkSourceBucketItem>>>,
    has_expired_sources: AtomicBool,
    sleepers: Mutex<HashMap<ThreadId, Thread>>,
}

impl<P: HasStaticPriority + Send + Sync + 'static> State<P> {
    fn insert_source(self: &Arc<Self>, priority: P, source: Box<dyn WorkSource>) {
        // Lock `sleepers` first to keep the same lock order as `State::insert_sleeper()`, which
        // avoids deadlocks
        let mut sleepers = self.sleepers.lock();
        let mut work_sources = self.work_sources.write();

        work_sources.entry(priority)
            .or_default()
            .values.push(WorkSourceBucketItem {
                source,
                expired: AtomicBool::new(false),
            });

        // Make sure all workers wake up to check for possible new work with the source, and failing
        // that, register the waker with the new source. Because we're holding the lock, new
        // sleepers can't be registered until the sleepers are empty, which ensures that a new waker
        // will be generated.
        for (_, sleeper) in sleepers.drain() {
            sleeper.unpark();
        }
    }

    fn find_work(&self) -> Option<Box<dyn Work<Output=()>>> {
        let sources = if self.has_expired_sources.swap(false, Ordering::AcqRel) {
            let mut sources = self.work_sources.write();
            for bucket in sources.values_mut() {
                bucket.clear_expired();
            }
            RwLockWriteGuard::downgrade(sources)
        } else {
            self.work_sources.read()
        };

        let mut output = None;
        let mut expired = false;

        'outer: for bucket in sources.values().rev() {
            for source in bucket.iter_ring() {
                match source.find_work() {
                    Ok(work) => {
                        output = Some(work);
                        break 'outer
                    },
                    Err(FindWorkError::NoWork) => {},
                    Err(FindWorkError::Disconnected) => {
                        expired = true;
                    },
                }
            }
        }

        if expired {
            // Don't clear the expired sources right now, because we've already retrieved a work
            // item. Instead, let the current thread process said work item, and the next thread to
            // look for work will clear the expired sources before scanning for work.
            self.has_expired_sources.store(true, Ordering::Release);
        }

        output
    }

    fn insert_sleeper(self: &Arc<Self>, thread: Thread) -> bool {
        let mut sleepers = self.sleepers.lock();
        if sleepers.contains_key(&thread.id()) {
            true
        } else {
            let create_waker = sleepers.is_empty();
            sleepers.insert(thread.id(), thread);
            // We don't need to hold the lock anymore, and it can deadlock with external logic
            // triggered by .set_worker_waker() below
            drop(sleepers);
            if create_waker {
                let waker = Waker::from(self.clone());
                for bucket in self.work_sources.read().values() {
                    for source in &bucket.values {
                        source.source.set_worker_waker(&waker);
                    }
                }
            }
            false
        }
    }

    fn remove_sleeper(self: &Arc<Self>, thread: Thread) {
        let mut sleepers = self.sleepers.lock();
        sleepers.remove(&thread.id());
    }
}

impl<P: HasStaticPriority + Send + Sync + 'static> Wake for State<P> {
    #[inline]
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }

    fn wake_by_ref(self: &Arc<Self>) {
        for (_, sleeper) in self.sleepers.lock().drain() {
            sleeper.unpark();
        }
    }
}

/// Pool of workers used to execute tasks.
///
/// Work cannot be directly given to the worker pool, but instead is retrieved via
/// [work sources](WorkSource), which can be configured with various priorities.
///
/// # Examples
///
/// Create a new worker pool:
/// ```
/// use gtether::worker::WorkerPool;
///
/// let workers = WorkerPool::<usize>::builder()
///     // Use 4 worker threads.
///     .worker_count(4.try_into().unwrap())
///     // Use the format "my-worker-{idx}" for thread names
///     .prefix("my-worker")
///     .start();
/// ```
pub struct WorkerPool<P: HasStaticPriority + Send + Sync + 'static> {
    state: Arc<State<P>>,
    exit_send: Option<crossbeam::channel::Sender<()>>,
    workers: Vec<JoinHandle<()>>,
}

impl<P: HasStaticPriority + Send + Sync + 'static> WorkerPool<P> {
    /// Create a [WorkerPoolBuilder].
    #[inline]
    pub fn builder() -> WorkerPoolBuilder<P> {
        WorkerPoolBuilder::default()
    }

    /// Shortcut for creating a [WorkerPoolBuilder] with a single worker already configured.
    #[inline]
    pub fn single() -> WorkerPoolBuilder<P> {
        WorkerPoolBuilder::default()
            .worker_count(unsafe { NonZeroUsize::new_unchecked(1) })
    }

    /// Insert a work source with the given priority.
    ///
    /// ```
    /// use gtether::worker::{WorkQueue, WorkerPool};
    /// use std::sync::Arc;
    ///
    /// let workers = WorkerPool::<isize>::builder()
    ///     .worker_count(1.try_into().unwrap())
    ///     .start();
    ///
    /// let queue_a = Arc::new(WorkQueue::new());
    /// let queue_b = Arc::new(WorkQueue::new());
    /// let queue_c = Arc::new(WorkQueue::new());
    ///
    /// // Work will be executed with priority given to queue A, then queue B, and finally queue C.
    /// workers.insert_source(-1, queue_a.clone());
    /// workers.insert_source(0, queue_a.clone());
    /// workers.insert_source(1, queue_a.clone());
    /// ```
    pub fn insert_source(&self, priority: P, source: impl WorkSource) {
        self.state.insert_source(priority, Box::new(source));
    }
}

impl<P: HasStaticPriority + Send + Sync + 'static> Drop for WorkerPool<P> {
    fn drop(&mut self) {
        drop(self.exit_send.take());

        for (_, sleeper) in self.state.sleepers.lock().drain() {
            sleeper.unpark();
        }

        for worker in self.workers.drain(..) {
            worker.join().expect("worker thread should safely exit");
        }
    }
}

/// Builder pattern for [WorkerPool].
#[derive(Educe)]
#[educe(Default)]
pub struct WorkerPoolBuilder<P: HasStaticPriority + Send + Sync + 'static> {
    worker_count: Option<NonZero<usize>>,
    prefix: Option<String>,
    #[cfg(test)]
    worker_lock: Option<Arc<RwLock<()>>>,
    _phantom_data: PhantomData<P>,
}

impl<P: HasStaticPriority + Send + Sync + 'static> WorkerPoolBuilder<P> {
    /// Number of workers this pool should use.
    ///
    /// Default uses [`std::thread::available_parallelism()`], and falls back to `1` if that fails.
    pub fn worker_count(mut self, count: NonZero<usize>) -> Self {
        self.worker_count = Some(count);
        self
    }

    /// Prefix to name worker threads with.
    ///
    /// Threads will be given the name "<prefix>-<idx>".
    ///
    /// Default is "worker".
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// In test environments, set an optional lock that must be acquired before workers can perform
    /// work.
    ///
    /// This can be used to synchronize test cases for more deterministic behavior.
    #[cfg(test)]
    pub fn worker_lock(mut self, worker_lock: Arc<RwLock<()>>) -> Self {
        self.worker_lock = Some(worker_lock);
        self
    }

    /// Start the [WorkerPool].
    pub fn start(self) -> WorkerPool<P> {
        let worker_count = self.worker_count.unwrap_or_else(|| {
            std::thread::available_parallelism()
                // SAFETY: Number is const `1`, which is non-zero
                .unwrap_or(unsafe { NonZero::new_unchecked(1) })
        });
        let prefix = self.prefix.unwrap_or_else(|| "worker".to_string());

        let state = Arc::new(State::<P>::default());
        let (exit_send, exit_recv) = crossbeam::channel::bounded(0);

        let mut workers = vec![];
        for idx in 0..worker_count.into() {
            let worker = Worker {
                state: state.clone(),
                exit_recv: exit_recv.clone(),
                #[cfg(test)]
                worker_lock: self.worker_lock.clone(),
            };

            let join_handle = std::thread::Builder::new()
                .name(format!("{}-{}", &prefix, idx))
                .spawn(|| worker.run())
                .expect("thread should spawn");
            workers.push(join_handle);
        }

        WorkerPool {
            state,
            exit_send: Some(exit_send),
            workers,
        }
    }
}

struct Worker<P: HasStaticPriority + Send + Sync + 'static> {
    state: Arc<State<P>>,
    exit_recv: crossbeam::channel::Receiver<()>,
    #[cfg(test)]
    worker_lock: Option<Arc<RwLock<()>>>,
}

impl<P: HasStaticPriority + Send + Sync + 'static> Worker<P> {
    fn run(self) {
        loop {
            match self.exit_recv.try_recv() {
                Err(TryRecvError::Empty) => {},
                Ok(()) | Err(TryRecvError::Disconnected) => {
                    // Exit signal received, time to break out of the loop
                    break
                },
            }

            #[cfg(test)]
            let guard = self.worker_lock.as_ref().map(|lock| lock.read());

            match self.state.find_work() {
                Some(task) => {
                    self.state.remove_sleeper(std::thread::current());
                    task.execute();
                },
                None => {
                    if self.state.insert_sleeper(std::thread::current()) {
                        // Only park on the second time we try to sleep, to allow for more work to
                        // be found after registering the sleeper and before actually sleeping,
                        // which would otherwise be a race condition.
                        #[cfg(test)]
                        drop(guard);
                        std::thread::park();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rstest::{fixture, rstest};
    use smol::future;

    use crate::worker::WorkQueue;

    struct WorkerPoolContext<P: HasStaticPriority + Send + Sync + 'static> {
        pool: WorkerPool<P>,
        lock: Arc<RwLock<()>>,
    }

    #[fixture]
    fn workers<P: HasStaticPriority + Send + Sync + 'static>(
        #[default(1)] worker_count: usize,
    ) -> WorkerPoolContext<P> {
        let lock = Arc::new(RwLock::new(()));
        let pool = WorkerPool::<P>::builder()
            .worker_count(worker_count.try_into().unwrap())
            .worker_lock(lock.clone())
            .start();
        WorkerPoolContext {
            pool,
            lock,
        }
    }

    #[rstest]
    #[case::empty(0, vec![], 0, vec![])]
    #[case::single(1, vec![0], 1, vec![0])]
    #[case::many(3, vec![0, 1, 2], 2, vec![2, 0, 1])]
    fn test_bucket_iter_ring(
        #[case] bucket_item_count: usize,
        #[case] expected_values_a: Vec<usize>,
        #[case] offset_iter_count: usize,
        #[case] expected_values_b: Vec<usize>,
    ) {
        let bucket = Bucket::from_iter(0..bucket_item_count);

        let values_a = bucket.iter_ring().cloned().collect::<Vec<_>>();
        assert_eq!(values_a, expected_values_a);

        {
            let mut iter = bucket.iter_ring();
            for _ in 0..offset_iter_count {
                iter.next();
            }
        }

        let values_b = bucket.iter_ring().cloned().collect::<Vec<_>>();
        assert_eq!(values_b, expected_values_b);
    }

    #[rstest]
    fn test_worker_pool_drop(
        #[with(4)] workers: WorkerPoolContext<usize>,
    ) {
        let state = Arc::downgrade(&workers.pool.state);
        drop(workers.pool);
        assert_eq!(state.strong_count(), 0);
    }

    #[rstest]
    fn test_worker_pool_queue_drop(
        #[with(1)] workers: WorkerPoolContext<usize>,
    ) {
        let mut tasks = vec![];
        {
            let _worker_lock = workers.lock.write();

            let queue = Arc::new(WorkQueue::new());
            tasks.push(queue.execute(move || "a".to_string()));
            tasks.push(queue.execute(move || "b".to_string()));
            tasks.push(queue.execute(move || "c".to_string()));
            workers.pool.insert_source(0, Arc::downgrade(&queue));

            drop(queue);
        }

        for task in tasks {
            future::block_on(task)
                .expect_err("task should be cancelled");
        }
    }

    #[rstest]
    #[case::single(vec![(0, vec!["a", "b", "c"])], vec!["a", "b", "c"])]
    #[case::same(
        vec![
            (0, vec!["a1", "b1", "c1"]),
            (0, vec!["a2", "b2", "c2"]),
        ],
        // Output should be round-robin of the input
        vec!["a1", "a2", "b1", "b2", "c1", "c2"],
    )]
    #[case::different(
        vec![
            (-1, vec!["a2", "b2"]),
            (1, vec!["a1", "b1"]),
            (0, vec!["a3", "b3"]),
        ],
        vec!["a1", "b1", "a3", "b3", "a2", "b2"],
    )]
    #[case::mixed(
        vec![
            (0, vec!["a2", "b2"]),
            (1, vec!["a1", "b1"]),
            (0, vec!["a3", "b3"]),
        ],
        vec!["a1", "b1", "a2", "a3", "b2", "b3"],
    )]
    fn test_worker_pool_priorities(
        workers: WorkerPoolContext<isize>,
        #[case] input: Vec<(isize, Vec<&'static str>)>,
        #[case] expected_output: Vec<&'static str>,
    ) {
        let output = Arc::new(Mutex::new(vec![]));
        let mut tasks = vec![];

        {
            let _worker_lock = workers.lock.write();
            for (priority, values) in input {
                let queue = WorkQueue::new();
                for value in values {
                    let value = value.to_string();
                    let output = output.clone();
                    let task = queue.execute(move || output.lock().push(value));
                    tasks.push(task);
                }
                workers.pool.insert_source(priority, queue);
            }
        }

        for task in tasks {
            future::block_on(task)
                .expect("task should complete");
        }

        let output = output.lock();
        assert_eq!(*output, expected_output);
    }
}