//! Worker pool and related traits and primitives.
//!
//! [WorkerPools](WorkerPool) are used to schedule and execute units of "work" with varying
//! priorities. Each unit of [Work] is a specific task that will be executed from start to end. Note
//! that these tasks are not async and therefore can't be context switched away from in the middle
//! of execution. If you wish to execute async tasks, see the [Executor](crate::util::executor)
//! abstraction which builds on top of worker pools.
//!
//! A basic usage of [WorkerPool] looks like the following:
//! ```
//! use gtether::worker::{WorkQueue, WorkerPool};
//! # use smol::future;
//! use std::sync::Arc;
//!
//! # future::block_on(async move {
//! // 1. create the worker pool
//! let workers = WorkerPool::builder().start();
//!
//! // 2. configure the worker pool with a work source
//! let queue = Arc::new(WorkQueue::new());
//! workers.insert_source((), queue.clone());
//!
//! // 3. execute a task
//! let task = queue.execute(|| 1 + 1);
//! let output = task.await.unwrap();
//! assert_eq!(output, 2);
//! # });
//! ```
//!
//! # Configuration
//!
//! By default, [worker pools](WorkerPool) use an amount of workers equal to
//! [`std::thread::available_parallelism()`]. If a different worker count is desired, it can be
//! configured with [`WorkerPoolBuilder::worker_count()`]:
//!
//! ```no_run
//! use gtether::worker::WorkerPool;
//!
//! // Use a singular worker
//! let workers = WorkerPool::<isize>::builder()
//!     .worker_count(1.try_into().unwrap())
//!     .start();
//!
//! // Use 4 workers
//! let workers = WorkerPool::<isize>::builder()
//!     .worker_count(4.try_into().unwrap())
//!     .start();
//! ```
//!
//! Worker threads are named "worker-<idx>" by default, where "<idx>" starts at `0`. To use a
//! different naming scheme, use [`WorkerPoolBuilder::prefix()`]:
//!
//! ```no_run
//! use gtether::worker::WorkerPool;
//!
//! // Name workers "custom-work-thread-<idx>"
//! let workers = WorkerPool::<isize>::builder()
//!     .prefix("custom-work-thread")
//!     .start();
//! ```
//!
//! # Submitting Work
//!
//! Work cannot be submitted directly to [worker pools](WorkerPool). Instead, worker pools poll for
//! work from [work sources](WorkSource), which can be inserted into worker pools with a given
//! priority.
//!
//! For general use cases, a basic [work queue](WorkQueue) that implements [WorkSource] is provided.
//!
//! ## Execution Order
//!
//! Work sources are polled in order of the priority they are inserted with, with higher priorities
//! being polled first. If multiple sources share the same priority, they are grouped in a bucket
//! that is polled in round-robin order. Note that the last source that was polled in a bucket is
//! kept track of per-thread, so that the next poll of that bucket continues where the last left
//! off. This ensures that every source in the same bucket is given a fair chance to poll for work.

use std::ops::Deref;
use std::sync::{Arc, Weak};
use std::task::Waker;

mod pool;
mod task;

pub use pool::{WorkerPool, WorkerPoolBuilder};
pub use task::{WorkQueue, WorkTask, WorkTaskError, WorkTaskResult};

/// A single unit of work to be submitted to a [WorkerPool].
///
/// Note that work items must have an output type of `()` to be executed by a [WorkerPool]. If your
/// work item has some other type of output, you can convert it via [`WorkTask::spawn()`] - most
/// [WorkSource] implementations will do this for you.
pub trait Work: Send + 'static {
    /// The output type of this work item.
    type Output: Send;

    /// Execute this work item and yield its output.
    fn execute(self: Box<Self>) -> Self::Output;
}

impl<F, T: Send> Work for F
where
    F: (FnOnce() -> T) + Send + 'static,
{
    type Output = T;

    #[inline]
    fn execute(self: Box<Self>) -> Self::Output {
        (*self)()
    }
}

/// Errors that can occur when looking for [Work] items from a [WorkSource].
#[derive(thiserror::Error, Debug)]
pub enum FindWorkError {
    /// The work source does not currently have any available work.
    #[error("no work available")]
    NoWork,

    /// The work source has been permanently disconnected.
    ///
    /// If a work source yields this error, it will be subsequently removed from the [WorkerPool].
    #[error("work source is disconnected")]
    Disconnected,
}

pub type FindWorkResult = Result<Box<dyn Work<Output=()>>, FindWorkError>;

/// A source of [Work] items that can be inserted into a [WorkerPool].
///
/// A basic queue implementation is provided via [WorkQueue], so this trait only needs to be
/// implemented for more advanced and/or bespoke implementations.
///
/// WorkSource implementations are provided for [Arc] and [Weak] containers that wrap other
/// WorkSource implementations.
///
/// # Implementation
///
/// Work sources are expected to provide [Work] items from a shared reference, and to store worker
/// wakers when workers sleep from lack of work. In the event that new work becomes available from
/// a source, that source is expected to wake the last waker that was provided to it.
///
/// Example implementation:
/// ```
/// use gtether::worker::{FindWorkError, FindWorkResult, Work, WorkSource};
/// use std::collections::VecDeque;
/// use std::sync::Mutex;
/// use std::task::Waker;
///
/// struct MyWorkSource {
///     queue: Mutex<VecDeque<Box<dyn Work<Output=()>>>>,
///     waker: Mutex<Option<Waker>>,
/// }
///
/// impl MyWorkSource {
///     fn push_work(&self, work: Box<dyn Work<Output=()>>) {
///         let mut queue = self.queue.lock().unwrap();
///         let mut waker = self.waker.lock().unwrap();
///         queue.push_back(work);
///         // There's new work, so wake the waker if it has been provided
///         waker.take().map(|w| w.wake());
///     }
/// }
///
/// impl WorkSource for MyWorkSource {
///     fn find_work(&self) -> FindWorkResult {
///         let mut queue = self.queue.lock().unwrap();
///         queue.pop_front().ok_or(FindWorkError::NoWork)
///     }
///
///     fn set_worker_waker(&self, waker: &Waker) {
///         *self.waker.lock().unwrap() = Some(waker.clone());
///     }
/// }
/// ```
pub trait WorkSource: Send + Sync + 'static {
    /// Attempt to find [Work] from this source.
    fn find_work(&self) -> FindWorkResult;

    /// Set the waker to be used to wake sleeping workers when new [Work] is available.
    ///
    /// The work source is expected to store the latest waker, and [`wake()`](Waker::wake) it when
    /// new work becomes available from this source.
    fn set_worker_waker(&self, waker: &Waker);
}

impl<S: WorkSource> WorkSource for Arc<S> {
    #[inline]
    fn find_work(&self) -> FindWorkResult {
        self.deref().find_work()
    }

    #[inline]
    fn set_worker_waker(&self, waker: &Waker) {
        self.deref().set_worker_waker(waker)
    }
}

impl<S: WorkSource> WorkSource for Weak<S> {
    #[inline]
    fn find_work(&self) -> FindWorkResult {
        match self.upgrade() {
            Some(source) => source.find_work(),
            None => Err(FindWorkError::Disconnected),
        }
    }

    #[inline]
    fn set_worker_waker(&self, waker: &Waker) {
        match self.upgrade() {
            Some(source) => source.set_worker_waker(waker),
            None => {},
        }
    }
}