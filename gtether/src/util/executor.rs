//! Async executors.
//!
//! The executors in this module are based off of the reference implementations that can be found in
//! the [`async-executor` crate](https://docs.rs/async-executor/).

use async_task::{Builder as TaskBuilder, Runnable, Task};
use educe::Educe;
use parking_lot::Mutex;
use slab::Slab;
use smol::future;
use smol::future::FutureExt;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::marker::PhantomData;
use std::sync::atomic::AtomicUsize;
use std::sync::{atomic, Arc};
use std::task::{Poll, Waker};
use std::thread::JoinHandle;

use crate::util::priority::{HasDynamicPriority, HasStaticPriority};

trait Metadata: Ord {
    fn increment_attempts(&self);
}

fn priority_eq<P: HasDynamicPriority>(a: &P, b: &P) -> bool {
    a.priority().eq(&b.priority())
}

fn priority_cmp<P: HasDynamicPriority>(a: &P, b: &P) -> Ordering {
    a.priority().cmp(&b.priority())
}

fn atomic_usize_eq(a: &AtomicUsize, b: &AtomicUsize) -> bool {
    a.load(atomic::Ordering::Relaxed).eq(&b.load(atomic::Ordering::Relaxed))
}

fn atomic_usize_cmp(a: &AtomicUsize, b: &AtomicUsize) -> Ordering {
    a.load(atomic::Ordering::Relaxed).cmp(&b.load(atomic::Ordering::Relaxed))
}

/// [Task] metadata with a static priority.
///
/// Dereferences into `P`.
#[derive(Educe)]
#[educe(Deref, PartialEq, Eq, PartialOrd, Ord)]
pub struct StaticMetadata<P: HasStaticPriority> {
    #[educe(Deref)]
    #[educe(Ord(rank = 0))]
    priority: P,
    #[educe(Eq(method(atomic_usize_eq)))]
    #[educe(Ord(rank = 1, method(atomic_usize_cmp)))]
    attempts: AtomicUsize,
}

impl<P: HasStaticPriority> Metadata for StaticMetadata<P> {
    fn increment_attempts(&self) {
        self.attempts.fetch_add(1, atomic::Ordering::Relaxed);
    }
}

impl<P: HasStaticPriority> StaticMetadata<P> {
    fn new(priority: P) -> Self {
        Self {
            priority,
            attempts: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub fn into_inner(self) -> P {
        self.priority
    }
}

/// [Task] metadata with a dynamic priority.
///
/// Dereferences into `P`.
#[derive(Educe)]
#[educe(Deref, PartialEq, Eq, PartialOrd, Ord)]
pub struct DynamicMetadata<P: HasDynamicPriority> {
    #[educe(Deref)]
    #[educe(Eq(method(priority_eq)))]
    #[educe(Ord(rank = 0, method(priority_cmp)))]
    priority: P,
    #[educe(Eq(method(atomic_usize_eq)))]
    #[educe(Ord(rank = 1, method(atomic_usize_cmp)))]
    attempts: AtomicUsize,
}

impl<P: HasDynamicPriority> Metadata for DynamicMetadata<P> {
    fn increment_attempts(&self) {
        self.attempts.fetch_add(1, atomic::Ordering::Relaxed);
    }
}

impl<P: HasDynamicPriority> DynamicMetadata<P> {
    fn new(priority: P) -> Self {
        Self {
            priority,
            attempts: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub fn into_inner(self) -> P {
        self.priority
    }
}

fn runnable_ord_eq<M: Metadata>(
    a: &Runnable<M>,
    b: &Runnable<M>,
) -> bool {
    a.metadata().eq(b.metadata())
}

fn runnable_ord_cmp<M: Metadata>(
    a: &Runnable<M>,
    b: &Runnable<M>,
) -> Ordering {
    a.metadata().cmp(b.metadata())
}

#[derive(Educe)]
#[educe(PartialEq, Eq, PartialOrd, Ord)]
struct RunnableOrd<M: Metadata>(
    #[educe(Eq(method(runnable_ord_eq)))]
    #[educe(Ord(method(runnable_ord_cmp)))]
    Runnable<M>
);

impl<M: Metadata> RunnableOrd<M> {
    fn into_inner(self) -> Runnable<M> {
        self.0
    }
}

trait ExecutorState: Send + Sync + 'static {
    type Metadata: Metadata;


    fn push(&mut self, runnable: Runnable<Self::Metadata>);

    fn pop(&mut self) -> Option<Runnable<Self::Metadata>>;

    // This will be used when the logic to re-schedule a task directly to its worker thread is
    // implemented.
    #[allow(unused)]
    fn swap_if_higher(&mut self, runnable: Runnable<Self::Metadata>) -> Runnable<Self::Metadata>;
}

/// Executor state for an [Executor] using static priorities.
///
/// This type isn't publicly constructable, and is only available for naming to use as a generic for
/// [Executor].
pub struct StaticPriority<P: HasStaticPriority> {
    queue: BinaryHeap<RunnableOrd<StaticMetadata<P>>>,
}

impl<P: HasStaticPriority + Send + Sync + 'static> ExecutorState for StaticPriority<P> {
    type Metadata = StaticMetadata<P>;

    fn push(&mut self, runnable: Runnable<Self::Metadata>) {
        self.queue.push(RunnableOrd(runnable));
    }

    fn pop(&mut self) -> Option<Runnable<Self::Metadata>> {
        self.queue.pop().map(RunnableOrd::into_inner)
    }

    fn swap_if_higher(&mut self, runnable: Runnable<Self::Metadata>) -> Runnable<Self::Metadata> {
        let runnable = RunnableOrd(runnable);
        if let Some(highest) = self.queue.peek() && highest > &runnable {
            let new_runnable = self.queue.pop().unwrap();
            self.queue.push(runnable);
            new_runnable.into_inner()
        } else {
            runnable.into_inner()
        }
    }
}

/// Executor state for an [Executor] using dynamic priorities.
///
/// This type isn't publicly constructable, and is only available for naming to use as a generic for
/// [Executor].
pub struct DynamicPriority<P: HasDynamicPriority> {
    queue: Vec<RunnableOrd<DynamicMetadata<P>>>,
}

fn find_highest_vec_idx<P: HasDynamicPriority>(
    runnables: &Vec<RunnableOrd<DynamicMetadata<P>>>,
) -> Option<(usize, &RunnableOrd<DynamicMetadata<P>>)> {
    let mut highest: Option<(usize, &RunnableOrd<DynamicMetadata<P>>)> = None;
    for (idx, runnable) in runnables.iter().enumerate() {
        match highest {
            Some((_, ref highest_priority)) => {
                if runnable > *highest_priority {
                    highest = Some((idx, runnable));
                }
            },
            None => {
                highest = Some((idx, runnable));
            }
        }
    }
    highest
}

impl<P: HasDynamicPriority + Send + Sync + 'static> ExecutorState for DynamicPriority<P> {
    type Metadata = DynamicMetadata<P>;

    fn push(&mut self, runnable: Runnable<Self::Metadata>) {
        self.queue.push(RunnableOrd(runnable));
    }

    fn pop(&mut self) -> Option<Runnable<Self::Metadata>> {
        find_highest_vec_idx(&self.queue)
            .map(|(idx, _)| idx)
            .map(|idx| self.queue.swap_remove(idx).into_inner())
    }

    fn swap_if_higher(&mut self, runnable: Runnable<Self::Metadata>) -> Runnable<Self::Metadata> {
        let runnable = RunnableOrd(runnable);
        if let Some((idx, highest)) = find_highest_vec_idx(&self.queue) && highest > &runnable {
            let new_runnable = self.queue.swap_remove(idx);
            self.queue.push(runnable);
            new_runnable.into_inner()
        } else {
            runnable.into_inner()
        }
    }
}

/// Multithreaded async executor.
///
/// Different priority styles can be used depending on how the executor is built.
///
/// # Static Priorities
///
/// ```
/// use gtether::util::executor::ExecutorBuilder;
///
/// let executor = ExecutorBuilder::default()
///     .static_priority::<i64>()
///     .build();
///
/// let task = executor.spawn(10, async { /* insert logic here */ });
/// ```
///
/// An executor built with static priorities cannot change a task's priority once it's submitted.
///
/// # Dynamic Priorities
///
/// ```
/// use gtether::util::executor::ExecutorBuilder;
/// use std::sync::atomic::{AtomicI64, Ordering};
/// use std::sync::Arc;
///
/// let executor = ExecutorBuilder::default()
///     .dynamic_priority::<Arc<AtomicI64>>()
///     .build();
///
/// let priority = Arc::new(AtomicI64::new(0));
/// let task = executor.spawn(priority.clone(), async { /* insert logic here */ });
/// priority.store(10, Ordering::Relaxed);
/// ```
///
/// An executor built with dynamic priorities can change submitted task priorities. If a task is
/// being actively executed, changing its priority will not immediately stop its execution, but the
/// next time the task is yielded its priority will be re-evaluated and another task may take its
/// place.
///
/// # Multiple Workers
///
/// ```
/// use gtether::util::executor::ExecutorBuilder;
///
/// let executor = ExecutorBuilder::default()
///     .static_priority::<i64>()
///     .worker_count(4)
///     .build();
///
/// let mut tasks = vec![];
/// executor.spawn_many(
///     (0..4).map(|idx| (idx, async { /* insert logic here */ })),
///     &mut tasks,
/// );
/// ```
///
/// By default, executors will be created with one threaded worker, but the worker count can be
/// configured when building the executor.
#[allow(private_bounds)]
pub struct Executor<'a, S: ExecutorState> {
    workers: Vec<WorkerHandle>,
    state: Arc<Mutex<S>>,
    active: Arc<Mutex<Slab<Waker>>>,
    _marker: PhantomData<&'a ()>,
}

#[allow(private_bounds)]
impl<'a, S: ExecutorState> Executor<'a, S> {
    /// Returns `true` if there are no unfinished tasks.
    pub fn is_empty(&self) -> bool {
        self.active.lock().is_empty()
    }

    fn spawn_inner<T: Send + 'a>(
        &self,
        metadata: S::Metadata,
        future: impl Future<Output = T> + Send + 'a,
        active: &mut Slab<Waker>,
    ) -> Task<T, S::Metadata> {
        let entry = active.vacant_entry();
        let idx = entry.key();
        let active_arc = self.active.clone();
        let future = call_on_drop::AsyncCallOnDrop::new(
            future,
            move || drop(active_arc.lock().try_remove(idx)),
        );

        // SAFETY:
        //
        // `future` is not `'static`, but we make sure that the `Runnable` does
        // not outlive `'a`. When the executor is dropped, workers are stopped
        // and the `active` field is drained, waking all of its `Waker`s. Then,
        // the queue inside the `Executor` is drained of all of its runnables.
        // This ensures that runnables are dropped and this precondition is
        // satisfied.
        //
        // `self.schedule()` is `Send`, `Sync` and `'static`, as checked below.
        // Therefore, we do not need to worry about what is done with the
        // `Waker`.
        let task_builder = TaskBuilder::new()
            .propagate_panic(true)
            .metadata(metadata);
        let (runnable, task) = unsafe { task_builder
            .spawn_unchecked(|_| future, self.schedule()) };
        entry.insert(runnable.waker());

        runnable.schedule();
        task
    }

    fn schedule(&self) -> impl Fn(Runnable<S::Metadata>) + Send + Sync + 'a {
        let workers = self.workers.iter()
            .map(WorkerHandle::waker)
            .collect::<Vec<_>>();
        let state = Arc::downgrade(&self.state);
        move |runnable| {
            // TODO: Enqueue directly back into the worker if task has woken itself
            runnable.metadata().increment_attempts();
            let state = state.upgrade()
                .expect("task_queue dropped before scheduled!");
            state.lock().push(runnable);
            for worker in &workers {
                worker.wake();
            }
        }
    }
}

impl<'a, P: HasStaticPriority + Send + Sync + 'static> Executor<'a, StaticPriority<P>> {
    /// Spawns a task onto the executor with a static priority.
    pub fn spawn<T: Send + 'a>(
        &self,
        priority: P,
        future: impl Future<Output = T> + Send + 'a,
    ) -> Task<T, StaticMetadata<P>> {
        let metadata = StaticMetadata::new(priority);
        let mut active = self.active.lock();
        self.spawn_inner(metadata, future, &mut *active)
    }

    /// Spawns many tasks onto the executor, each with their own static priority.
    ///
    /// This locks the executor's inner task lock once and spawns all the tasks in one go. With
    /// large amounts of tasks this can improve contention.
    ///
    /// For very large numbers of tasks the lock is occasionally dropped and re-acquired to prevent
    /// runner thread starvation. It is assumed that the iterator provided does not block; blocking
    /// iterators can lock up the internal mutex and therefore the entire executor.
    pub fn spawn_many<T: Send + 'a, F: Future<Output = T> + Send + 'a>(
        &self,
        futures: impl IntoIterator<Item = (P, F)>,
        handles: &mut impl Extend<Task<T, StaticMetadata<P>>>
    ) {
        let mut active = Some(self.active.lock());

        let tasks = futures.into_iter().enumerate()
            .map(move |(idx, (priority, fut))| {
                let metadata = StaticMetadata::new(priority);
                let active_ref = &mut **(active.as_mut().unwrap());
                let task = self.spawn_inner(metadata, fut, active_ref);

                if idx.wrapping_add(1) % 500 == 0 {
                    drop(active.take());
                    active = Some(self.active.lock());
                }

                task
            });

        handles.extend(tasks);
    }
}

impl<'a, P: HasDynamicPriority + Send + Sync + 'static> Executor<'a, DynamicPriority<P>> {
    /// Spawns a task onto the executor with a dynamic priority.
    pub fn spawn<T: Send + 'a>(
        &self,
        priority: P,
        future: impl Future<Output = T> + Send + 'a,
    ) -> Task<T, DynamicMetadata<P>> {
        let metadata = DynamicMetadata::new(priority);
        let mut active = self.active.lock();
        self.spawn_inner(metadata, future, &mut *active)
    }

    /// Spawns many tasks onto the executor, each with their own dynamic priority.
    ///
    /// This locks the executor's inner task lock once and spawns all the tasks in one go. With
    /// large amounts of tasks this can improve contention.
    ///
    /// For very large numbers of tasks the lock is occasionally dropped and re-acquired to prevent
    /// runner thread starvation. It is assumed that the iterator provided does not block; blocking
    /// iterators can lock up the internal mutex and therefore the entire executor.
    pub fn spawn_many<T: Send + 'a, F: Future<Output = T> + Send + 'a>(
        &self,
        futures: impl IntoIterator<Item = (P, F)>,
        handles: &mut impl Extend<Task<T, DynamicMetadata<P>>>
    ) {
        let mut active = Some(self.active.lock());

        let tasks = futures.into_iter().enumerate()
            .map(move |(idx, (priority, fut))| {
                let active_ref = &mut **(active.as_mut().unwrap());
                let metadata = DynamicMetadata::new(priority);
                let task = self.spawn_inner(metadata, fut, active_ref);

                if idx.wrapping_add(1) % 500 == 0 {
                    drop(active.take());
                    active = Some(self.active.lock());
                }

                task
            });

        handles.extend(tasks);
    }
}

impl<S: ExecutorState> Drop for Executor<'_, S> {
    fn drop(&mut self) {
        // Stop all workers first
        for worker in self.workers.drain(..) {
            drop(worker);
        }

        {
            // Wake all tasks so that they are in the queue to be cleared by the next step
            let mut active = self.active.lock();
            for w in active.drain() {
                w.wake();
            }
        }

        {
            // Forcibly drain the queue to clear any held cyclic Arcs
            let mut state = self.state.lock();
            while state.pop().is_some() {}
        }
    }
}

/// Builder pattern for [Executor].
///
/// See Executor documentation for usage examples.
pub struct ExecutorBuilder<S> {
    worker_count: Option<usize>,
    #[cfg(test)]
    worker_lock: Option<Arc<Mutex<()>>>,
    state: S,
}

impl Default for ExecutorBuilder<()> {
    #[inline]
    fn default() -> Self {
        Self {
            worker_count: None,
            #[cfg(test)]
            worker_lock: None,
            state: (),
        }
    }
}

impl<S> ExecutorBuilder<S> {
    /// Set the number of threaded workers this [Executor] will use.
    ///
    /// Defaults to `1`.
    pub fn worker_count(mut self, count: usize) -> Self {
        self.worker_count = Some(count);
        self
    }

    /// In test environments, set an optional lock that must be acquired before workers can perform
    /// work.
    ///
    /// This can be used to synchronize test cases for more deterministic behavior.
    #[cfg(test)]
    pub fn worker_lock(mut self, worker_lock: Arc<Mutex<()>>) -> Self {
        self.worker_lock = Some(worker_lock);
        self
    }
}

impl ExecutorBuilder<()> {
    /// Configure an [Executor] that uses static priorities.
    pub fn static_priority<P: HasStaticPriority>(self) -> ExecutorBuilder<StaticPriority<P>> {
        ExecutorBuilder {
            worker_count: self.worker_count,
            #[cfg(test)]
            worker_lock: self.worker_lock,
            state: StaticPriority {
                queue: Default::default(),
            },
        }
    }

    /// Configure an [Executor] that uses dynamic priorities.
    pub fn dynamic_priority<P: HasDynamicPriority>(self) -> ExecutorBuilder<DynamicPriority<P>> {
        ExecutorBuilder {
            worker_count: self.worker_count,
            #[cfg(test)]
            worker_lock: self.worker_lock,
            state: DynamicPriority {
                queue: Default::default(),
            },
        }
    }
}

#[allow(private_bounds)]
impl<S: ExecutorState> ExecutorBuilder<S> {
    /// Build the [Executor].
    ///
    /// The executor must first be configured to use a specific state before it can be built. This
    /// is done by using one of the following methods:
    ///  * [`static_priority()`](ExecutorBuilder::static_priority)
    ///  * [`dynamic_priority()`](ExecutorBuilder::dynamic_priority)
    pub fn build<'a>(self) -> Executor<'a, S> {
        let worker_count = self.worker_count.unwrap_or(1);
        let state = Arc::new(Mutex::new(self.state));

        let workers = (0..worker_count).into_iter()
            .map(|_| Worker::start(
                state.clone(),
                #[cfg(test)]
                self.worker_lock.clone(),
            ))
            .collect();

        let active = Arc::new(Mutex::new(Slab::new()));

        Executor {
            workers,
            state,
            active,
            _marker: PhantomData,
        }
    }
}

// This module is ripped from the async-executor reference implementation
mod call_on_drop {
    use pin_project_lite::pin_project;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct CallOnDrop<F: FnMut()>(F);

    impl<F: FnMut()> Drop for CallOnDrop<F> {
        fn drop(&mut self) {
            self.0();
        }
    }

    pin_project! {
        pub struct AsyncCallOnDrop<Fut, Cleanup: FnMut()> {
            #[pin]
            future: Fut,
            cleanup: CallOnDrop<Cleanup>,
        }
    }

    impl<Fut, Cleanup: FnMut()> AsyncCallOnDrop<Fut, Cleanup> {
        pub fn new(future: Fut, cleanup: Cleanup) -> Self {
            Self {
                future,
                cleanup: CallOnDrop(cleanup),
            }
        }
    }

    impl<Fut: Future, Cleanup: FnMut()> Future for AsyncCallOnDrop<Fut, Cleanup> {
        type Output = Fut::Output;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.project().future.poll(cx)
        }
    }
}

struct WorkerWaker(Arc<Mutex<Option<Waker>>>);

impl WorkerWaker {
    fn wake(&self) {
        if let Some(waker) = self.0.lock().take() {
            waker.wake();
        }
    }
}

struct WorkerHandle {
    waker: Arc<Mutex<Option<Waker>>>,
    handle: Option<(smol::channel::Sender<()>, JoinHandle<()>)>,
}

impl WorkerHandle {
    fn waker(&self) -> WorkerWaker {
        WorkerWaker(self.waker.clone())
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        if let Some((exit_send, join_handle)) = self.handle.take() {
            drop(exit_send);
            join_handle.join().unwrap();
        }
    }
}

struct Worker<S: ExecutorState> {
    waker: Arc<Mutex<Option<Waker>>>,
    exit_recv: smol::channel::Receiver<()>,
    state: Arc<Mutex<S>>,
}

impl<S: ExecutorState> Worker<S> {
    async fn run(
        &self,
        #[cfg(test)]
        worker_lock: Option<Arc<Mutex<()>>>
    ) {
        loop {
            for _ in 0..200 {
                #[cfg(test)]
                let _guard = worker_lock.as_ref().map(|lock| lock.lock());
                let runnable = self.runnable().await;
                runnable.run();
            }
            future::yield_now().await;
        }
    }

    async fn check_exit(&self) {
        let _ = self.exit_recv.recv().await;
    }

    fn start(
        state: Arc<Mutex<S>>,
        #[cfg(test)]
        worker_lock: Option<Arc<Mutex<()>>>,
    ) -> WorkerHandle {
        let waker = Arc::new(Mutex::new(None));
        let (exit_send, exit_recv) = smol::channel::bounded(1);

        let runner = Self {
            waker: waker.clone(),
            exit_recv,
            state,
        };

        let join_handle = std::thread::spawn(move || {
            future::block_on(runner.check_exit().or(runner.run(
                #[cfg(test)]
                worker_lock,
            )));
        });

        WorkerHandle {
            waker,
            handle: Some((exit_send, join_handle)),
        }
    }

    fn sleep(&self, new_waker: &Waker) -> bool {
        let mut waker = self.waker.lock();
        if waker.is_none() {
            let _ = waker.insert(new_waker.clone());
            true
        } else {
            false
        }
    }

    async fn runnable(&self) -> Runnable<S::Metadata> {
        future::poll_fn(|cx| {
            loop {
                match self.state.lock().pop() {
                    None => {
                        if !self.sleep(cx.waker()) {
                            // Only returning this after sleeping twice and not detecting a change
                            // in state avoids a race condition where a new task can become
                            // available after setting the waker but before yielding from this
                            // method.
                            return Poll::Pending
                        }
                    },
                    Some(r) => {
                        self.waker.lock().take();
                        return Poll::Ready(r)
                    },
                }
            }
        }).await
    }
}

fn _ensure_send_and_sync() {
    fn is_send<T: Send>(_: T) {}
    fn is_sync<T: Sync>(_: T) {}
    fn is_static<T: 'static>(_: T) {}

    {
        is_send::<Executor<'_, StaticPriority<usize>>>(ExecutorBuilder::default().static_priority().build());
        is_sync::<Executor<'_, StaticPriority<usize>>>(ExecutorBuilder::default().static_priority().build());

        let ex = ExecutorBuilder::default().static_priority::<usize>().build();
        is_send(ex.schedule());
        is_sync(ex.schedule());
        is_static(ex.schedule());
    }

    {
        is_send::<Executor<'_, DynamicPriority<usize>>>(ExecutorBuilder::default().dynamic_priority().build());
        is_sync::<Executor<'_, DynamicPriority<usize>>>(ExecutorBuilder::default().dynamic_priority().build());

        let ex = ExecutorBuilder::default().dynamic_priority::<usize>().build();
        is_send(ex.schedule());
        is_sync(ex.schedule());
        is_static(ex.schedule());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    #[test]
    fn test_executor_drops() {
        let executor = ExecutorBuilder::default()
            .worker_count(1)
            .static_priority::<usize>()
            .build();

        let active = Arc::downgrade(&executor.active);
        let state = Arc::downgrade(&executor.state);

        let task = executor.spawn(0, future::pending::<()>());

        drop(executor);

        assert!(active.upgrade().is_none());
        assert!(state.upgrade().is_none());

        let result = future::block_on(task.fallible());
        assert!(result.is_none());
    }

    #[test]
    fn test_executor_static_priority() {
        let worker_lock = Arc::new(Mutex::new(()));

        let executor = ExecutorBuilder::default()
            .worker_count(1)
            .worker_lock(worker_lock.clone())
            .static_priority::<usize>()
            .build();

        let output = Mutex::new(vec![]);

        let mut tasks = vec![];
        {
            let _guard = worker_lock.lock();
            executor.spawn_many(
                [
                    (1, async { output.lock().push(1); }.boxed()),
                    (0, async { output.lock().push(0); }.boxed()),
                    (2, async { output.lock().push(2); }.boxed()),
                ],
                &mut tasks,
            );
        }

        for task in tasks {
            future::block_on(task);
        }

        assert_eq!(*output.lock(), [2, 1, 0]);
    }

    #[test]
    fn test_executor_dynamic_priority() {
        let worker_lock = Arc::new(Mutex::new(()));

        let executor = ExecutorBuilder::default()
            .worker_count(1)
            .worker_lock(worker_lock.clone())
            .dynamic_priority::<usize>()
            .build();

        let output = Mutex::new(vec![]);

        let mut tasks = vec![];
        {
            let _guard = worker_lock.lock();
            executor.spawn_many(
                [
                    (1, async { output.lock().push(1); }.boxed()),
                    (0, async { output.lock().push(0); }.boxed()),
                    (2, async { output.lock().push(2); }.boxed()),
                ],
                &mut tasks,
            );
        }

        for task in tasks {
            future::block_on(task);
        }

        assert_eq!(*output.lock(), [2, 1, 0]);
    }
}