use async_task::{FallibleTask, Task};
use pin_project::pin_project;
use std::cmp::Ordering;
use std::fmt::{Display, Formatter};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::resource::manager::ResourceManagerWorkerPriorityConfig;
use crate::util::executor::{Executor, FifoExecutor, FifoMetadata, StaticMetadata, StaticPriorityExecutor};
use crate::util::priority::HasStaticPriority;
use crate::worker::WorkerPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPriority {
    Immediate(i64),
    Delayed(i64),
    Update,
}

impl PartialOrd for TaskPriority {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaskPriority {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Immediate(a), Self::Immediate(b)) |
            (Self::Delayed(a), Self::Delayed(b)) => {
                a.cmp(b)
            },
            (Self::Update, Self::Update) => {
                Ordering::Equal
            },
            (Self::Immediate(_), Self::Delayed(_) | Self::Update) |
            (Self::Delayed(_), Self::Update) => {
                Ordering::Greater
            },
            (Self::Delayed(_), Self::Immediate(_)) |
            (Self::Update, Self::Immediate(_) | Self::Delayed(_)) => {
                Ordering::Less
            },
        }
    }
}

impl Display for TaskPriority {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Immediate(val) => write!(f, "Immediate({val})"),
            Self::Delayed(val) => write!(f, "Delayed({val})"),
            Self::Update => write!(f, "Update"),
        }
    }
}

macro_rules! delegate_task_methods {
    ($outer:ident, $inner:ident, $original:ident) => {
        impl<T> $outer<T> {
            /// Detaches the task to let it keep running in the background.
            ///
            #[doc = concat!("See also: [`", stringify!($original), "::detach()`]")]
            #[inline]
            pub fn detach(self) {
                match self.0 {
                    $inner::Immediate(task) |
                    $inner::Delayed(task) => task.detach(),
                    $inner::Update(task) => task.detach(),
                }
            }

            /// Cancels the task and waits for it to stop running.
            ///
            /// Returns the task's output if it was completed just before it got canceled, or
            /// [`None`] if it didn't complete.
            ///
            /// While it's possible to simply drop the [`Task`] to cancel it, this is a cleaner way
            /// of canceling because it also waits for the task to stop running.
            ///
            #[doc = concat!("See also: [`", stringify!($original), "::cancel()`]")]
            #[inline]
            pub async fn cancel(self) -> Option<T> {
                match self.0 {
                    $inner::Immediate(task) |
                    $inner::Delayed(task) => task.cancel().await,
                    $inner::Update(task) => task.cancel().await,
                }
            }

            /// Returns `true` if the current task is finished.
            ///
            /// Note that in a multithreaded environment, this task can change finish immediately
            /// after calling this function.
            ///
            #[doc = concat!("See also: [`", stringify!($original), "::is_finished()`]")]
            #[inline]
            pub fn is_finished(&self) -> bool {
                match &self.0 {
                    $inner::Immediate(task) |
                    $inner::Delayed(task) => task.is_finished(),
                    $inner::Update(task) => task.is_finished(),
                }
            }
        }
    };
}

#[derive(Debug)]
#[pin_project(project = ManagerTaskInnerProj)]
enum ManagerTaskInner<T> {
    Immediate(#[pin] Task<T, StaticMetadata<i64>>),
    Delayed(#[pin] Task<T, StaticMetadata<i64>>),
    Update(#[pin] Task<T, FifoMetadata>),
}

/// Wrapper around a [Task].
///
/// This wrapper encapsulates internal manager logic in addition to the task it wraps. Its API is
/// designed to mimic the API of [Task], and delegates most methods as such.
#[derive(Debug)]
#[pin_project]
pub struct ManagerTask<T>(#[pin] ManagerTaskInner<T>);
delegate_task_methods!(ManagerTask, ManagerTaskInner, Task);

impl<T> ManagerTask<T> {
    /// Converts this task into a [`FallibleManagerTask`].
    ///
    /// Like [`ManagerTask`], a fallible task will poll the task's output until it is completed or
    /// cancelled due to its [`Runnable`](async_task::Runnable) being dropped without being run.
    /// Resolves to the task's output when completed, or [`None`] if it didn't complete.
    ///
    /// See also: [`Task::fallible()`]
    #[inline]
    pub fn fallible(self) -> FallibleManagerTask<T> {
        let inner = match self.0 {
            ManagerTaskInner::Immediate(task) =>
                FallibleManagerTaskInner::Immediate(task.fallible()),
            ManagerTaskInner::Delayed(task) =>
                FallibleManagerTaskInner::Delayed(task.fallible()),
            ManagerTaskInner::Update(task) =>
                FallibleManagerTaskInner::Update(task.fallible()),
        };
        FallibleManagerTask(inner)
    }
}

impl<T> Future for ManagerTask<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().0.project() {
            ManagerTaskInnerProj::Immediate(task) |
            ManagerTaskInnerProj::Delayed(task) => task.poll(cx),
            ManagerTaskInnerProj::Update(task) => task.poll(cx),
        }
    }
}

#[derive(Debug)]
#[pin_project(project = FallibleManagerTaskInnerProj)]
enum FallibleManagerTaskInner<T> {
    Immediate(#[pin] FallibleTask<T, StaticMetadata<i64>>),
    Delayed(#[pin] FallibleTask<T, StaticMetadata<i64>>),
    Update(#[pin] FallibleTask<T, FifoMetadata>),
}

/// Wrapper around a [FallibleTask].
///
/// This wrapper encapsulates internal manager logic in addition to the task it wraps. Its API is
/// designed to mimic the API of [FallibleTask], and delegates most methods as such.
#[derive(Debug)]
#[pin_project]
pub struct FallibleManagerTask<T>(#[pin] FallibleManagerTaskInner<T>);
delegate_task_methods!(FallibleManagerTask, FallibleManagerTaskInner, FallibleTask);

impl<T> Future for FallibleManagerTask<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().0.project() {
            FallibleManagerTaskInnerProj::Immediate(task) |
            FallibleManagerTaskInnerProj::Delayed(task) => task.poll(cx),
            FallibleManagerTaskInnerProj::Update(task) => task.poll(cx),
        }
    }
}

pub struct ManagerExecutor {
    immediate: StaticPriorityExecutor<'static, i64>,
    delayed: StaticPriorityExecutor<'static, i64>,
    update: FifoExecutor<'static>,
}

impl ManagerExecutor {
    pub fn new<PP>(
        priority_config: ResourceManagerWorkerPriorityConfig<PP>,
        workers: &WorkerPool<PP>,
    ) -> Self
    where
        PP: HasStaticPriority + Send + Sync + 'static,
    {
        let immediate = Executor::new_static_priority(
            priority_config.immediate,
            workers,
        );
        let delayed = Executor::new_static_priority(
            priority_config.delayed,
            workers,
        );
        let update = Executor::new_fifo(
            priority_config.update,
            workers,
        );

        Self {
            immediate,
            delayed,
            update,
        }
    }

    #[allow(unused)] // Used in tests
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.immediate.is_empty() && self.delayed.is_empty() && self.update.is_empty()
    }

    #[inline]
    pub fn spawn<T: Send + 'static>(
        &self,
        priority: TaskPriority,
        future: impl Future<Output = T> + Send + 'static,
    ) -> ManagerTask<T> {
        let inner = match priority {
            TaskPriority::Immediate(p) =>
                ManagerTaskInner::Immediate(self.immediate.spawn(p, future)),
            TaskPriority::Delayed(p) =>
                ManagerTaskInner::Delayed(self.delayed.spawn(p, future)),
            TaskPriority::Update =>
                ManagerTaskInner::Update(self.update.spawn(future)),
        };
        ManagerTask(inner)
    }
}