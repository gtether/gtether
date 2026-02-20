#[cfg(test)] #[macro_use]
extern crate assert_matches;
extern crate nalgebra_glm as glm;

#[cfg(test)]
// Fixes derive macro in tests
extern crate self as gtether;

use educe::Educe;
use parking_lot::RwLock;
use std::any::Any;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::ops::Deref;
use std::sync::Arc;
use std::thread::JoinHandle;
use tracing::debug;

use crate::app::driver::AppDriver;
use crate::app::Application;
use crate::event::{EventBus, EventBusRegistry};
use crate::net::driver::NetDriverFactory;
use crate::net::Networking;
use crate::resource::manager::ResourceManager;

pub mod app;
pub mod console;
pub mod event;
#[cfg(feature = "gui")]
pub mod gui;
pub mod net;
#[cfg(feature = "graphics")]
pub mod render;
pub mod resource;
pub mod util;
pub mod worker;

/// Error indicating the [Engine] tried to transition to an invalid [stage](EngineStage).
///
/// When the [Engine] is in any given stage, it is only allowed to transition into certain other
/// stages. If a user attempts to transition the engine to a stage that is not allowed, this error
/// will be yielded.
///
/// See [EngineStage#transitioning] for more.
#[derive(Debug, Clone)]
pub struct InvalidEngineStageTransition {
    /// The [stage](EngineStage) the [Engine] is currently in.
    pub current: EngineStage,

    /// The [stage](EngineStage) that was attempted to be transitioned to.
    pub next: EngineStage,
}

impl Display for InvalidEngineStageTransition {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid transition from stage '{:?}' to stage '{:?}", &self.current, &self.next)
    }
}

impl Error for InvalidEngineStageTransition {}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// The current stage that the [Engine] is in.
///
/// The engine's lifecycle is kept track of using a series of stages. These stages indicate at what
/// point of the lifecycle the engine is in, as well as what the engine is expected to be doing at
/// that point in time.
///
/// Engine stages can largely only be changed via an instance of [EngineState], which is only given
/// to [Side] implementations, but the current stage can always be queried via [Engine::stage()].
///
/// # Transitioning
///
/// When the engine needs to transition from one stage into another, there are a limited amount of
/// options that are valid. These options make up a flow graph of stages, as can be seen below:
/// ```mermaid
/// flowchart TD
///     I[Init] --> R[Running]
///     R --> Sg[Stopping]
///     Sg --> S[Stopped]
///     R --> Ssg[Suspending]
///     Ssg --> Ss[Suspended]
///     Ss --> Rg[Resuming]
///     Rg --> R
///     Ss --> Sg
/// ```
///
/// Stages are idempotent, and can always be transitioned to themselves; this should for all intents
/// and purposes be a "noop".
///
/// If a user attempts to transition the engine into an invalid stage, an
/// [InvalidEngineStageTransition] error will be generated.
///
/// # Suspension
///
/// Some platforms and/or [AppDrivers](AppDriver) support the concept of suspending the application.
/// The details of this may vary from platform to platform, but in general any rendering or heavy
/// workloads should be halted while the application is suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStage {
    /// The [Engine] is currently initializing.
    ///
    /// This is where resources should first be loaded/created, and any long term application data
    /// is initialized via [Application::init()].
    Init,

    /// The [Engine] is currently running.
    Running,

    /// The [Engine] is currently resuming from being suspended.
    ///
    /// This is where rendering/etc should be reinitialized via [Application::resume()], and after
    /// resuming is completed, the [Engine] will be [Running](EngineStage::Running).
    Resuming,

    /// The [Engine] is currently suspending.
    ///
    /// This is where rendering/etc should be shutdown via [Application::suspend()]. After
    /// suspending is completed, the [Engine] will be [Suspended](EngineStage::Suspended) until it
    /// is resumed.
    Suspending,

    /// The [Engine] is currently suspended.
    ///
    /// No active rendering or other processing intensive tasks should be running during this stage,
    /// but some light background tasks may be running.
    Suspended,

    /// The [Engine] has been told to stop, and is currently stopping.
    ///
    /// This is a temporary state where the engine is actively stopping and cleaning up resources,
    /// but has not fully stopped.
    Stopping,

    /// The [Engine] has stopped.
    ///
    /// This is a terminal state, and the engine is expected to no longer perform any tasks at this
    /// point.
    Stopped,
}

impl Default for EngineStage {
    #[inline]
    fn default() -> Self {
        Self::Init
    }
}

impl EngineStage {
    /// Validate if a transition from this stage to another is valid.
    ///
    /// Follows the rules outlined in [EngineStage#transitioning].
    pub fn validate_transition(&self, next: EngineStage)
            -> Result<(), InvalidEngineStageTransition> {
        if self == &next {
            // Transition to the same stage is always allowed
            return Ok(())
        }
        match (self, next) {
            (Self::Init, Self::Running) => Ok(()),
            (Self::Running, Self::Suspending) => Ok(()),
            (Self::Suspending, Self::Suspended) => Ok(()),
            (Self::Suspended, Self::Resuming) => Ok(()),
            (Self::Resuming, Self::Running) => Ok(()),
            (Self::Running | Self::Suspended, Self::Stopping) => Ok(()),
            (Self::Stopping, Self::Stopped) => Ok(()),
            _ => Err(InvalidEngineStageTransition { current: *self, next })
        }
    }
}

/// The [EngineStage] has changed.
///
/// This event is fired as the [Engine] is changing [stages](EngineStage), immediately after the
/// stage change has been completed.
#[derive(Debug)]
pub struct EngineStageChangedEvent {
    previous: EngineStage,
    current: EngineStage,
}

impl EngineStageChangedEvent {
    /// The [EngineStage] that was previously set.
    #[inline]
    pub fn previous(&self) -> EngineStage { self.previous }

    /// The new [EngineStage] that was just transitioned to.
    #[inline]
    pub fn current(&self) -> EngineStage { self.current }
}

#[derive(Educe)]
#[educe(Debug)]
struct EngineStateInner {
    stage: EngineStage,
    #[educe(Debug(ignore))]
    event_bus: Arc<EventBus>,
}

impl EngineStateInner {
    fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            stage: EngineStage::default(),
            event_bus,
        }
    }

    fn transition_stage(&mut self, stage: EngineStage) -> Result<(), InvalidEngineStageTransition> {
        if stage != self.stage {
            self.stage.validate_transition(stage)?;
            let previous = self.stage;
            debug!(?previous, current = ?stage, "Engine transitioning stage");
            self.stage = stage;
            self.event_bus.fire(EngineStageChangedEvent {
                previous,
                current: stage,
            });
        }
        Ok(())
    }
}

/// Mutable [Engine] state.
///
/// An instance of EngineState can be used to mutate the engine's internal state. Generally, these
/// instances cannot be created by the user, and are only given to [AppDriver] implementations. Any
/// mutation to engine state is expected to come from [AppDriver] implementations _only_.
pub struct EngineState<A: Application> {
    engine: Arc<Engine<A>>,
}

impl<A: Application> EngineState<A> {
    /// Attempt to transition this [engine's](Engine) [stage](EngineStage).
    ///
    /// Will validate the given `stage` using [EngineStage::validate_transition()].
    pub fn set_stage(&self, stage: EngineStage) -> Result<(), InvalidEngineStageTransition> {
        self.engine.state.write().transition_stage(stage)
    }
}

impl<A: Application> Deref for EngineState<A> {
    type Target = Arc<Engine<A>>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}

/// Represents an instance of the gTether Engine.
///
/// All engine usage stems from this structure, and it is required to construct one first.
pub struct Engine<A: Application> {
    app: A,
    app_driver: A::ApplicationDriver,
    state: Arc<RwLock<EngineStateInner>>,
    event_bus: Arc<EventBus>,
    resources: Arc<ResourceManager>,
    networking: Arc<Networking<A::NetworkingDriver>>,
}

impl<A: Application> Engine<A> {
    #[inline]
    pub fn builder() -> EngineBuilder<A> {
        EngineBuilder::new()
    }

    #[inline]
    pub fn app(&self) -> &A { &self.app }

    #[inline]
    pub fn stage(&self) -> EngineStage {
        self.state.read().stage
    }

    #[inline]
    pub fn event_bus(&self) -> &EventBusRegistry {
        self.event_bus.registry()
    }

    /// The engine's main [ResourceManager].
    #[inline]
    pub fn resources(&self) -> &Arc<ResourceManager> { &self.resources }

    /// The engine's [Networking] stack.
    #[inline]
    pub fn net(&self) -> &Arc<Networking<A::NetworkingDriver>> {
        &self.networking
    }

    pub fn start(self: &Arc<Self>) {
        let engine_state = EngineState {
            engine: self.clone(),
        };

        // TODO: Need to ensure this isn't called multiple times

        self.app_driver.run_main_loop(engine_state);
    }

    pub fn stop(&self) -> Result<(), InvalidEngineStageTransition> {
        let mut state = self.state.write();
        if state.stage == EngineStage::Stopped {
            // Already stopped, nothing to do
            Ok(())
        } else {
            // Attempt to begin stopping
            state.transition_stage(EngineStage::Stopping)
        }
    }
}

#[cfg(feature = "graphics")]
impl<A> Engine<A>
where
    A: Application,
    A::ApplicationDriver: render::AppDriverGraphicsVulkan,
{
    /// The Vulkan [render instance](render::Instance).
    #[inline]
    pub fn render_instance(&self) -> Arc<render::Instance> {
        use render::AppDriverGraphicsVulkan;
        self.app_driver.render_instance()
    }
}

#[cfg(feature = "gui")]
impl<A> Engine<A>
where
    A: Application,
    A::ApplicationDriver: gui::window::AppDriverWindowManager,
{
    /// A reference to the [WindowManager](gui::window::WindowManager).
    #[inline]
    pub fn window_manager(&self) -> &dyn gui::window::WindowManager {
        use gui::window::AppDriverWindowManager;
        self.app_driver.window_manager()
    }
}

/// Error that can occur when joining or stopping an [EngineJoinHandle].
pub enum EngineJoinHandleError {
    /// The [Engine] thread panicked.
    Panicked(Box<dyn Any + Send + 'static>),

    /// Could not stop the [Engine] because the stage [transition](EngineStage#transitioning) would
    /// be [invalid](InvalidEngineStageTransition).
    InvalidStageTransition(InvalidEngineStageTransition),
}

impl Debug for EngineJoinHandleError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panicked(_) =>
                f.debug_tuple("EngineJoinHandleError::Panicked")
                    .finish_non_exhaustive(),
            Self::InvalidStageTransition(err) =>
                f.debug_tuple("EngineJoinHandleError::InvalidStageTransition")
                    .field(err)
                    .finish(),
        }
    }
}

impl Display for EngineJoinHandleError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panicked(_) => write!(f, "Engine thread panicked!"),
            Self::InvalidStageTransition(err) => Display::fmt(err, f),
        }
    }
}

impl Error for EngineJoinHandleError {}

/// Handle for a threaded [Engine].
///
/// This handle can be used to manage an [Engine] instance that is running in a separate thread. It
/// provides methods to stop the running [Engine], as well as join a stopped [Engine] thread.
pub struct EngineJoinHandle<A: Application> {
    join_handle: JoinHandle<()>,
    engine: Arc<Engine<A>>,
}

impl<A: Application> EngineJoinHandle<A> {
    /// Join the [Engine] thread.
    ///
    /// Does not stop the [Engine] beforehand, so this will block until the [Engine] has been
    /// stopped externally, if it hasn't been already.
    pub fn join(self) -> Result<(), EngineJoinHandleError> {
        self.join_handle.join()
            .map_err(|err| EngineJoinHandleError::Panicked(err))
    }

    /// Stop the [Engine] thread.
    ///
    /// Signals the [Engine] to stop, and joins the [Engine] thread afterward.
    pub fn stop(self) -> Result<(), EngineJoinHandleError> {
        self.engine.stop()
            .map_err(|err| EngineJoinHandleError::InvalidStageTransition(err))?;
        self.join()
    }

    /// Get a reference to the [Engine].
    #[inline]
    pub fn engine(&self) -> Arc<Engine<A>> {
        self.engine.clone()
    }
}

/// Build a new gTether Engine.
///
/// # Examples
///
/// ```
/// use async_trait::async_trait;
/// use gtether::Engine;
/// use gtether::app::Application;
/// use gtether::app::driver::MinimalAppDriver;
/// use gtether::net::driver::NoNetDriver;
/// use gtether::resource::manager::ResourceManager;
/// use gtether::worker::WorkerPool;
///
/// struct MyApp {}
///
/// #[async_trait(?Send)]
/// impl Application for MyApp {
///     type ApplicationDriver = MinimalAppDriver;
///     type NetworkingDriver = NoNetDriver;
///     // Implement relevant functions...
/// }
///
/// let app = MyApp {};
///
/// let workers = WorkerPool::single().start();
/// let resources = ResourceManager::builder()
///     .worker_config((), &workers)
///     .build();
///
/// let engine = Engine::builder()
///     .app(app)
///     .app_driver(MinimalAppDriver::new())
///     .resources(resources)
///     .networking_driver(NoNetDriver::new())
///     .build();
/// ```
pub struct EngineBuilder<A: Application> {
    app: Option<A>,
    app_driver: Option<A::ApplicationDriver>,
    resources: Option<Arc<ResourceManager>>,
    networking: Option<Arc<Networking<A::NetworkingDriver>>>,
}

impl<A: Application> EngineBuilder<A> {
    pub fn new() -> Self {
        Self {
            app: None,
            app_driver: None,
            resources: None,
            networking: None,
        }
    }

    pub fn app(mut self, app: A) -> Self {
        self.app = Some(app);
        self
    }

    pub fn app_driver(mut self, app_driver: A::ApplicationDriver) -> Self {
        self.app_driver = Some(app_driver);
        self
    }

    /// Configure a main [ResourceManager] for the engine.
    ///
    /// This is required.
    pub fn resources(mut self, resources: Arc<ResourceManager>) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Configure the [networking driver](NetDriver) for the engine.
    ///
    /// This is required.
    pub fn networking_driver(
        mut self,
        driver_factory: impl NetDriverFactory<A::NetworkingDriver>,
    ) -> Self {
        self.networking = Some(Arc::new(Networking::new(driver_factory)));
        self
    }

    pub fn build(self) -> Arc<Engine<A>> {
        let app = self.app
            .expect(".app() is required");

        let app_driver = self.app_driver
            .expect(".app_driver() is required");

        let resources = self.resources
            .expect(".resources() is required");

        let networking = self.networking
            .expect(".networking_driver() is required");

        let event_bus = Arc::new(EventBus::builder()
            .event_type::<EngineStageChangedEvent>()
            .build());

        let state = Arc::new(RwLock::new(EngineStateInner::new(event_bus.clone())));

        Arc::new(Engine {
            app,
            app_driver,
            state,
            event_bus,
            resources,
            networking,
        })
    }

    /// Start the [Engine] in a separate thread.
    ///
    /// Spawn a separate thread, and start the [Engine] in it. This will yield an [EngineJoinHandle]
    /// to manage the new [Engine] thread.
    pub fn spawn(self) -> EngineJoinHandle<A> {
        let engine = self.build();
        let engine_thread = engine.clone();

        let join_handle = std::thread::Builder::new()
            // TODO: Get engine thread name
            // .name()
            .spawn(move || {
                engine_thread.start();
            }).unwrap();

        EngineJoinHandle {
            join_handle,
            engine,
        }
    }
}

/// A helper type for non-exhaustive structs.
///
/// This allows structures to be created via a constructor function or by update syntax in
/// combination with `Default::default()`. Copied from and inspired by Vulkano's NonExhaustive.
///
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NonExhaustive(pub(crate) ());