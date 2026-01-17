use smol::future::FutureExt;
use smol::{future, Executor, Task};
use std::fmt::{Display, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use strum::EnumCount;
use tracing::warn;

#[repr(usize)]
#[derive(Debug, Clone, Copy, EnumCount)]
pub enum TaskPriority {
    Immediate = 0,
    Delayed = 1,
    Update = 2,
}

impl Display for TaskPriority {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Immediate => f.write_str("Immediate"),
            Self::Delayed => f.write_str("Delayed"),
            Self::Update => f.write_str("Update"),
        }
    }
}

pub type ManagerTask<T> = Task<T>;

pub struct ManagerExecutor {
    execs: Arc<[Executor<'static>; TaskPriority::COUNT]>,
    worker: Option<(smol::channel::Sender<()>, JoinHandle<()>)>,
}

impl ManagerExecutor {
    pub fn new() -> Self {
        let execs = Arc::new(std::array::from_fn(|_| Executor::<'static>::new()));
        let worker_execs = execs.clone();
        let (signal, shutdown) = smol::channel::unbounded::<()>();

        let join_handle = thread::Builder::new()
            .name("resource-manager".to_string())
            .spawn(move || future::block_on(async {
                let run_forever = async {
                    loop {
                        for _ in 0..200 {
                            let t0 = worker_execs[TaskPriority::Immediate as usize].tick();
                            let t1 = worker_execs[TaskPriority::Delayed as usize].tick();
                            let t2 = worker_execs[TaskPriority::Update as usize].tick();

                            // Wait until one of the ticks completes, trying them in order from
                            // the highest priority to the lowest priority
                            t0.or(t1).or(t2).await;
                        }

                        // Yield occasionally
                        future::yield_now().await;
                    }
                };

                let _ = shutdown.recv().or(run_forever).await;
            })).unwrap();

        Self {
            execs,
            worker: Some((signal, join_handle)),
        }
    }

    #[inline]
    pub fn spawn<T: Send + 'static>(
        &self,
        priority: TaskPriority,
        future: impl Future<Output = T> + Send + 'static,
    ) -> ManagerTask<T> {
        self.execs[priority as usize].spawn(future)
    }

    #[allow(unused)] // Used in e.g. tests
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.execs.iter().all(|exec| exec.is_empty())
    }
}

impl Drop for ManagerExecutor {
    fn drop(&mut self) {
        if let Some((signal, join_handle)) = self.worker.take() {
            drop(signal);
            match join_handle.join() {
                Ok(()) => (),
                Err(error) =>
                    warn!(?error, "ResourceManager background thread errored"),
            }
        } else {
            warn!("ResourceManager executor join handle already taken");
        }
    }
}