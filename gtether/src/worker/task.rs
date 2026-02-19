use educe::Educe;
use futures_util::task::AtomicWaker;
use ouroboros::self_referencing;
use smol::channel::{TryRecvError, TrySendError};
use std::fmt::{Debug, Formatter};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::worker::{FindWorkError, FindWorkResult, Work, WorkSource};

/// Errors that can occur when a [WorkTask] resolves.
#[derive(thiserror::Error, Debug)]
pub enum WorkTaskError {
    /// The task was cancelled.
    ///
    /// Tasks cannot be manually cancelled, but if the task's [Work] item is dropped before it is
    /// executed, then the task will be marked as cancelled. This can occur if the task's
    /// [WorkSource] is dropped or otherwise halted before the task is executed.
    #[error("work task was cancelled")]
    Cancelled,
}

pub type WorkTaskResult<T> = Result<T, WorkTaskError>;

struct WorkRunnable<W: Work + ?Sized> {
    work: Box<W>,
    sender: smol::channel::Sender<W::Output>,
}

impl<W: Work + ?Sized> Work for WorkRunnable<W> {
    type Output = ();

    fn execute(self: Box<Self>) -> Self::Output {
        let value = self.work.execute();
        match self.sender.try_send(value) {
            Ok(()) | Err(TrySendError::Closed(_)) => {},
            Err(TrySendError::Full(_)) => {
                panic!("work task channel should never be full")
            }
        }
    }
}

/// Task representing a queued [Work] item.
///
/// This is a handle representing a [Work] item that has yet to be executed by the
/// [WorkerPool](super::WorkerPool). Not every [Work] item has an attached WorkTask, but if the
/// [Work] item has an output type other than `()`, a WorkTask is required to retrieve that output.
///
/// ```
/// # use smol::future;
/// use gtether::worker::WorkTask;
///
/// # future::block_on(async move {
/// let (work, task) = WorkTask::spawn(Box::new(|| true));
///
/// work.execute();
/// let output = task.await
///     .expect("task should not be cancelled");
/// assert_eq!(output, true);
/// # })
/// ```
#[self_referencing]
pub struct WorkTask<T: 'static> {
    receiver: smol::channel::Receiver<T>,
    #[borrows(receiver)]
    #[not_covariant]
    recv: Option<Pin<Box<smol::channel::Recv<'this, T>>>>,
}

impl<T: 'static> Debug for WorkTask<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let state_str = self.with_recv(|recv| match recv {
            Some(_) => "polling",
            None => "incomplete",
        });
        f.debug_struct("WorkTask")
            .field("state", &state_str)
            .finish()
    }
}

impl<T: 'static> WorkTask<T> {
    /// Create a new WorkTask from an existing [Work] item.
    ///
    /// This does not queue or otherwise execute the [Work] item, but only creates the task handle.
    /// This API is usually used inside [WorkSources](WorkSource), and should not be needed by an
    /// end user to execute a [Work] item.
    pub fn spawn<W>(work: Box<W>) -> (Box<dyn Work<Output=()>>, Self)
    where
        W: Work<Output=T> + ?Sized,
    {
        let (sender, receiver) = smol::channel::bounded(1);

        let runnable = Box::new(WorkRunnable {
            work,
            sender,
        });

        let task = WorkTaskBuilder {
            receiver,
            recv_builder: |_| None,
        }.build();

        (runnable, task)
    }

    /// Attempt to synchronously poll the task for completion.
    ///
    /// If the task has completed, this will yield the task's output. If the task has not yet
    /// completed, the task will be returned for later re-polling.
    ///
    /// This is a synchronous alternative to awaiting the task.
    ///
    /// ```
    /// use gtether::worker::WorkTask;
    ///
    /// let (work, task) = WorkTask::spawn(Box::new(|| true));
    ///
    /// let task = task.try_poll()
    ///     .expect_err("task should not be completed");
    ///
    /// work.execute();
    /// let output = task.try_poll()
    ///     .expect("task should be completed")
    ///     .expect("task should not be cancelled");
    /// assert_eq!(output, true);
    /// ```
    pub fn try_poll(self) -> Result<WorkTaskResult<T>, Self> {
        if self.with_recv(|recv| recv.is_none()) {
            match self.borrow_receiver().try_recv() {
                Ok(v) => Ok(Ok(v)),
                Err(TryRecvError::Empty) => Err(self),
                Err(TryRecvError::Closed) => Ok(Err(WorkTaskError::Cancelled)),
            }
        } else {
            panic!("WorkTask must not be try_polled() after it has been polled()")
        }
    }
}

impl<T> Future for WorkTask<T> {
    type Output = WorkTaskResult<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.with_mut(|fields| {
            let fut = fields.recv.get_or_insert_with(|| {
                Box::pin(fields.receiver.recv())
            });
            fut.as_mut().poll(cx)
                .map(|result| result.map_err(|_| WorkTaskError::Cancelled))
        })
    }
}

/// Basic [Work] queue for use with [WorkerPool](super::WorkerPool).
///
/// This work queue implements [WorkSource], and so can be inserted into a
/// [WorkerPool](super::WorkerPool) using [`insert_source()`](super::WorkerPool::insert_source).
///
/// # Examples
///
/// Using a WorkQueue with a [WorkerPool](super::WorkerPool).
/// ```
/// use gtether::worker::{WorkQueue, WorkerPool};
/// use std::sync::Arc;
///
/// let workers = WorkerPool::<usize>::builder()
///     .worker_count(1.try_into().unwrap())
///     .start();
///
/// let queue = Arc::new(WorkQueue::new());
///
/// // Insert a weak reference to cancel remaining tasks when the queue is dropped
/// workers.insert_source(0, Arc::downgrade(&queue));
///
/// // Or insert a strong reference to ensure all tasks can be completed.
/// workers.insert_source(0, queue.clone());
/// ```
#[derive(Educe)]
#[educe(Default)]
pub struct WorkQueue {
    queue: crossbeam::queue::SegQueue<Box<dyn Work<Output=()>>>,
    waker: AtomicWaker,
}

impl WorkQueue {
    /// Create a new work queue.
    ///
    /// ```
    /// use gtether::worker::WorkQueue;
    ///
    /// let q = WorkQueue::new();
    /// ```
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if the work queue is empty.
    ///
    /// ```
    /// use gtether::worker::WorkQueue;
    ///
    /// let q = WorkQueue::new();
    /// assert!(q.is_empty());
    ///
    /// let task = q.execute(|| {});
    /// assert!(!q.is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns the number of elements in the queue.
    ///
    /// ```
    /// use gtether::worker::WorkQueue;
    ///
    /// let q = WorkQueue::new();
    /// assert_eq!(q.len(), 0);
    ///
    /// let task_a = q.execute(|| {});
    /// assert_eq!(q.len(), 1);
    ///
    /// let task_b = q.execute(|| {});
    /// assert_eq!(q.len(), 2);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Pushes a work item onto the queue.
    ///
    /// ```
    /// use gtether::worker::WorkQueue;
    ///
    /// let q = WorkQueue::new();
    ///
    /// let task = q.execute(|| {});
    /// ```
    #[inline]
    pub fn execute<W: Work>(&self, work: W) -> WorkTask<W::Output> {
        self.execute_boxed(Box::new(work))
    }

    /// Pushes an already boxed work item onto the queue.
    ///
    /// This lets unsized work items be executed.
    ///
    /// ```
    /// use gtether::worker::{Work, WorkQueue};
    ///
    /// let q = WorkQueue::new();
    ///
    /// let work: Box<dyn Work<Output=()>> = Box::new(|| {});
    /// let task = q.execute_boxed(work);
    /// ```
    pub fn execute_boxed<W: Work + ?Sized>(&self, work: Box<W>) -> WorkTask<W::Output> {
        let (work, task) = WorkTask::spawn(work);
        self.queue.push(work);
        self.waker.wake();
        task
    }
}

impl WorkSource for WorkQueue {
    #[inline]
    fn find_work(&self) -> FindWorkResult {
        self.queue.pop().ok_or(FindWorkError::NoWork)
    }

    #[inline]
    fn set_worker_waker(&self, waker: &Waker) {
        self.waker.register(waker);
    }
}