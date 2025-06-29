//! Utilities around custom tick loops.
//!
//! This module contains utilities for creating tick loops with custom configurations. A tick loop
//! is a loop that attempts to "tick", or execute, at a consistent rate. A tick loop can be
//! considered similar to an event loop, but it is driven by a timer rather than events.
//!
//! The primary way to create tick loops is via [TickLoopBuilder]; see that type for examples.
use educe::Educe;
use parking_lot::{Condvar, Mutex};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::warn;

#[derive(Debug)]
enum TickType {
    Rate(usize),
    MinDuration(Duration),
}

impl Default for TickType {
    #[inline]
    fn default() -> Self {
        Self::MinDuration(Duration::from_millis(10))
    }
}

impl TickType {
    fn next_tick(&self, last_tick: Instant, loop_name: &str) -> Option<Instant> {
        let now = Instant::now();
        match self {
            Self::Rate(rate) => {
                // Increment timeslot
                let tick_size = Duration::from_secs_f32(1.0 / *rate as f32);
                let mut next_tick = last_tick;
                next_tick += tick_size;
                let mut skipped_ticks = 0;
                while next_tick < now {
                    next_tick += tick_size;
                    skipped_ticks += 1;
                }
                if skipped_ticks > 0 {
                    let tick_time = now - last_tick;
                    warn!(skipped_ticks, ?tick_time, loop_name, "Tick(s) took too long");
                }
                Some(next_tick)
            },
            Self::MinDuration(min_duration) => {
                let next_tick = last_tick + *min_duration;
                if next_tick > now {
                    Some(next_tick)
                } else {
                    None
                }
            },
        }
    }
}

#[derive(Educe)]
#[educe(Debug)]
struct TickLoop<FI, FT, D, E>
where
    FI: FnOnce() -> Result<D, E>,
    FT: FnMut(&mut D, Duration) -> bool,
{
    name: String,
    tick_type: TickType,
    #[educe(Debug(ignore))]
    fn_init: FI,
    #[educe(Debug(ignore))]
    fn_tick: FT,
}

impl<FI, FT, D, E> TickLoop<FI, FT, D, E>
where
    FI: FnOnce() -> Result<D, E>,
    FT: FnMut(&mut D, Duration) -> bool,
{
    fn run(
        should_exit: Arc<AtomicBool>,
        name: &str,
        tick_type: TickType,
        mut fn_tick: FT,
        mut data: D,
    ) {
        let mut last_tick = Instant::now();
        let mut next_tick = tick_type.next_tick(last_tick, name);
        while !should_exit.load(Ordering::Relaxed) {
            // Sleep until next timeslot
            if let Some(next_tick) = next_tick {
                let now = Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                }
            }

            // Tick
            let tick_start = Instant::now();
            let delta = tick_start - last_tick;
            if !(&mut fn_tick)(&mut data, delta) {
                // Tick said to exit
                break;
            }

            last_tick = tick_start;
            next_tick = tick_type.next_tick(tick_start, name);
        }
    }

    fn start(self) -> Result<(), E> {
        match (self.fn_init)() {
            Ok(data) => {
                let should_exit = Arc::new(AtomicBool::new(false));
                Self::run(
                    should_exit,
                    &self.name,
                    self.tick_type,
                    self.fn_tick,
                    data,
                );
                Ok(())
            },
            Err(e) => Err(e),
        }
    }
}

impl<FI, FT, D, E> TickLoop<FI, FT, D, E>
where
    FI: (FnOnce() -> Result<D, E>) + Send + 'static,
    FT: (FnMut(&mut D, Duration) -> bool) + Send + 'static,
    D: 'static,
    E: Send + 'static,
{
    fn spawn(self) -> Result<TickLoopHandle, E> {
        let should_exit = Arc::new(AtomicBool::new(false));
        let should_exit_thread = should_exit.clone();

        let pair = Arc::new((Mutex::new(None), Condvar::new()));
        let pair_thread = pair.clone();

        let join_handle = Some(std::thread::Builder::new()
            .name(self.name.clone())
            .spawn(move || {
                let &(ref lock, ref cvar) = &*pair_thread;
                match (self.fn_init)() {
                    Ok(data) => {
                        {
                            let mut result = lock.lock();
                            *result = Some(Ok(()));
                            cvar.notify_one();
                        }
                        Self::run(
                            should_exit_thread,
                            &self.name,
                            self.tick_type,
                            self.fn_tick,
                            data,
                        );
                    },
                    Err(e) => {
                        let mut result = lock.lock();
                        *result = Some(Err(e));
                        cvar.notify_one();
                    }
                }
            })
            .unwrap());

        let &(ref lock, ref cvar) = &*pair;
        let mut result = lock.lock();
        if !result.is_some() {
            cvar.wait(&mut result);
        }
        result.take().unwrap()?;

        Ok(TickLoopHandle {
            join_handle,
            should_exit,
        })
    }
}

/// Handle for a threaded tick loop.
///
/// This handle doesn't really do anything by itself, but represents a tick loop running in a
/// separate thread. This handle is not cloneable, and will stop and join the tick loop thread when
/// it is dropped.
///
/// ```no_run
/// use gtether::util::tick_loop::TickLoopBuilder;
/// # use gtether::util::tick_loop::TickLoopBuildError;
///
/// let join_handle = TickLoopBuilder::new()
///     // These init()/tick() closures are noops
///     .init(|| Ok::<(), ()>(()))
///     .tick(|_, _| true)
///     // Spawn a threaded tick loop
///     .spawn()?;
///
/// // Drop the join_handle to stop and join the threaded tick loop
/// drop(join_handle);
/// #
/// # Ok::<(), TickLoopBuildError<_>>(())
/// ```
#[derive(Educe)]
#[educe(Debug)]
pub struct TickLoopHandle {
    #[educe(Debug(ignore))]
    join_handle: Option<JoinHandle<()>>,
    should_exit: Arc<AtomicBool>,
}

impl Drop for TickLoopHandle {
    fn drop(&mut self) {
        if let Some(join_handle) = self.join_handle.take() {
            self.should_exit.store(true, Ordering::Relaxed);
            join_handle.join().unwrap();
        } else {
            warn!("TickLoop internal thread already joined");
        }
    }
}

/// Error that can occur when building a tick loop.
#[derive(Debug)]
pub enum TickLoopBuildError<E> {
    /// A required option was not specified.
    MissingOption { name: String },
    /// The init() callback yielded an error.
    InitError(E),
}

impl<E: Display> Display for TickLoopBuildError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingOption { name } => write!(f, "Missing required option: '{name}'"),
            Self::InitError(err) => write!(f, "Initialization failed: {err}"),
        }
    }
}

impl<E: Debug + Display> Error for TickLoopBuildError<E> {}

impl<E> TickLoopBuildError<E> {
    fn missing_option(name: impl Into<String>) -> Self {
        Self::MissingOption { name: name.into() }
    }
}

/// Builder pattern for tick loops.
///
/// Tick loops are generally comprised of an [`init()`](TickLoopBuilder::init) closure, a
/// [`tick()`](TickLoopBuilder::tick) closure, and several configuration options. When starting a
/// loop, the `init()` closure will be executed to run any onetime initialization logic, and
/// generate an [associated data structure](#associated-data-structure), if any. Afterward, the
/// `tick()` closure will be repeatedly executed at configured intervals.
///
/// # Associated Data Structure
///
/// Sometimes data needs to initialized _within_ the context of the loop, and cannot be created
/// ahead of time. For example, if a [threaded tick loop](#threaded-tick-loops) needs some data
/// owned solely within the context of the loop that _isn't_ `Send`, it cannot be moved into the
/// loop's `tick()` closure. For these situations, the `init()` closure can yield an arbitrary type.
/// That type will be fed into every invocation of the `tick()` closure via a mutable borrow.
///
/// # Threaded Tick Loops
///
/// When building a tick loop, it can either be built threaded with
/// [`spawn()`](TickLoopBuilder::spawn) or local with [`start()`](TickLoopBuilder::start). When
/// building a threaded tick loop, a separate thread will be started to run the tick loop, and a
/// [TickLoopHandle] will be yielded to keep track of it. The `init()` and `tick()` closures used
/// to build a threaded tick loop must also be `Send` and `'static`, as they have to be sent to the
/// started thread.
///
/// # Examples
///
/// Start a local tick loop with a given tick rate.
/// ```no_run
/// use gtether::util::tick_loop::TickLoopBuilder;
/// # use gtether::util::tick_loop::TickLoopBuildError;
///
/// TickLoopBuilder::new()
///     // 60 ticks per second
///     .tick_rate(60)
///     .init(|| Ok::<usize, ()>(0))
///     .tick(|count, _| {
///         *count += 1;
///         // Will stop the tick loop after ~10 minutes
///         *count < 600
///     })
///     .start()?;
/// #
/// # Ok::<(), TickLoopBuildError<_>>(())
/// ```
///
/// Spawn a threaded tick loop
/// ```no_run
/// use gtether::util::tick_loop::TickLoopBuilder;
/// # use gtether::util::tick_loop::TickLoopBuildError;
///
/// let join_handle = TickLoopBuilder::new()
///     .init(|| Ok::<usize, ()>(0))
///     .tick(|count, _| {
///         *count += 1;
///         // Will stop the tick loop after ~10 minutes
///         *count < 600
///     })
///     .spawn()?;
/// #
/// # Ok::<(), TickLoopBuildError<_>>(())
/// ```
pub struct TickLoopBuilder<FI, FT, D: 'static = (), E = ()>
where
    FI: FnOnce() -> Result<D, E>,
    FT: FnMut(&mut D, Duration) -> bool,
{
    name: Option<String>,
    tick_type: Option<TickType>,
    fn_init: Option<FI>,
    fn_tick: Option<FT>,
}

impl<FI, FT, D, E> TickLoopBuilder<FI, FT, D, E>
where
    FI: FnOnce() -> Result<D, E>,
    FT: FnMut(&mut D, Duration) -> bool,
{
    /// Create a new [TickLoopBuilder].
    #[inline]
    pub fn new() -> Self {
        Self {
            name: None,
            tick_type: None,
            fn_init: None,
            fn_tick: None,
        }
    }

    /// Set the name of the tick loop.
    ///
    /// This value is used for setting the thread name if building a threaded tick loop, and for any
    /// logging or other diagnostics that may be emitted.
    ///
    /// Defaults to `"tick-loop"`.
    #[inline]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the minimum duration for a single tick.
    ///
    /// When executing the tick loop, if a tick takes less time than this duration, the tick loop
    /// will sleep until this duration is met. If a tick takes more time than this duration, the
    /// tick loop will continue on and immediately execute the next tick.
    ///
    /// This option is mutually exclusive with [`tick_rate()`](Self::tick_rate), and setting one
    /// will override the other.
    ///
    /// Defaults to 10ms.
    #[inline]
    pub fn min_tick_duration(mut self, min_tick_duration: Duration) -> Self {
        self.tick_type = Some(TickType::MinDuration(min_tick_duration));
        self
    }

    /// Set the tick rate per second.
    ///
    /// When executing the tick loop, will attempt to execute ticks at a rate that is consistent to
    /// the given rate. If a tick takes less time than the calculated duration based on this rate,
    /// the tick loop will sleep until that duration has passed. If a tick takes more time than the
    /// calculated duration and would cause one or more ticks to be "skipped" in a single second,
    /// the tick loop will sleep until the next timeslot - in multiples the calculated per-tick
    /// duration - and log a warning that one or more ticks have been skipped.
    ///
    /// This option is mutually exclusive with [`min_tick_duration()`](Self::min_tick_duration), and
    /// setting one will override the other.
    ///
    /// This setting is disabled by default, and a [`min_tick_duration`](Self::min_tick_duration)
    /// of 10ms is used as the default instead.
    #[inline]
    pub fn tick_rate(mut self, tick_rate: usize) -> Self {
        self.tick_type = Some(TickType::Rate(tick_rate));
        self
    }

    /// Specify the initialization closure.
    ///
    /// This closure will be called before the tick loop starts executing, in the same context as
    /// the tick loop. For example, if the tick loop is threaded, this will be called in the same
    /// thread.
    ///
    /// This is a required option.
    #[inline]
    pub fn init(mut self, f: FI) -> Self {
        self.fn_init = Some(f);
        self
    }

    /// Specify the tick closure.
    ///
    /// This closure will be called once for each tick, and is where the majority of the tick loops
    /// work is expected to occur.
    ///
    /// This is a required option.
    #[inline]
    pub fn tick(mut self, f: FT) -> Self {
        self.fn_tick = Some(f);
        self
    }

    fn build(self) -> Result<TickLoop<FI, FT, D, E>, TickLoopBuildError<E>> {
        let name = self.name
            .unwrap_or("tick-loop".to_owned());
        let tick_type = self.tick_type.unwrap_or_default();
        let fn_init = self.fn_init
            .ok_or(TickLoopBuildError::missing_option("init"))?;
        let fn_tick = self.fn_tick
            .ok_or(TickLoopBuildError::missing_option("tick"))?;

        Ok(TickLoop {
            name,
            tick_type,
            fn_init,
            fn_tick,
        })
    }

    /// Start the tick loop locally.
    ///
    /// Build the tick loop, and start it locally, in the thread that called this method. This
    /// method will not return until the tick loop has stopped executing.
    ///
    /// # Errors
    ///
    /// Errors if there was a problem while building the tick loop.
    pub fn start(self) -> Result<(), TickLoopBuildError<E>> {
        let tick_loop = self.build()?;
        match tick_loop.start() {
            Ok(_) => Ok(()),
            Err(err) => Err(TickLoopBuildError::InitError(err)),
        }
    }
}

impl<FI, FT, D, E> TickLoopBuilder<FI, FT, D, E>
where
    FI: (FnOnce() -> Result<D, E>) + Send + 'static,
    FT: (FnMut(&mut D, Duration) -> bool) + Send + 'static,
    D: 'static,
    E: Send + 'static,
{
    /// Start the tick loop in a thread.
    ///
    /// Build the tick loop, and spawn a thread to run it in. This method yields a [TickLoopHandle]
    /// that can be used to keep track of the threaded tick loop. When this method returns, the tick
    /// loop will be started (but possibly still initializing).
    ///
    /// # Errors
    ///
    /// Errors if there was a problem while building the tick loop.
    pub fn spawn(self) -> Result<TickLoopHandle, TickLoopBuildError<E>> {
        let tick_loop = self.build()?;
        match tick_loop.spawn() {
            Ok(tlh) => Ok(tlh),
            Err(err) => Err(TickLoopBuildError::InitError(err)),
        }
    }
}