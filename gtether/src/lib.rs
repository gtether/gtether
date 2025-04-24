#[cfg(test)] #[macro_use]
extern crate assert_matches;
extern crate nalgebra_glm as glm;

use std::any::Any;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use async_trait::async_trait;
use parking_lot::RwLock;
use net::driver::NetDriverFactory;

use crate::net::driver::NetDriver;
use crate::net::Networking;
use crate::resource::manager::ResourceManager;

pub mod client;
pub mod console;
pub mod event;
pub mod net;
#[cfg(feature = "graphics")]
pub mod render;
pub mod resource;
pub mod server;
pub mod util;

/// User provided application.
///
/// The Application is the main entry point for user-defined logic in the [Engine]. It is
/// implemented by the user, and consists of a series of callbacks that the engine executes at
/// defined points in its lifecycle.
///
/// An Application is also typed with a [Side], in order to be able to work with references to the
/// [Engine] itself. Note that this means that Application can be implemented for the same type
/// multiple times with different Sides, if desired.
///
/// # Examples
///
/// ```
/// use std::sync::{Arc, OnceLock};
/// use std::time::Duration;
/// use async_trait::async_trait;
/// use gtether::{Application, Engine};
/// use gtether::client::Client;
/// use gtether::net::driver::NoNetDriver;
///
/// struct BasicApp {
///     value_a: u32,
///     value_b: OnceLock<u32>,
/// }
///
/// #[async_trait(?Send)]
/// impl Application<Client> for BasicApp {
///     type NetworkingDriver = NoNetDriver;
///
///     async fn init(&self, _engine: &Arc<Engine<Self, Client>>) {
///         self.value_b.set(9001).unwrap();
///     }
///
///     fn tick(&self, engine: &Arc<Engine<Self, Client>>, delta: Duration) {
///         // Do something with value_a and value_b...
///     }
/// }
/// ```
#[async_trait(?Send)]
pub trait Application<S>: Sized + Send + Sync
where
    S: Side,
{
    /// [NetDriver] type that will be used to handle networking for this application.
    ///
    /// If networking is not required, [NoNetDriver](net::driver::NoNetDriver) is an appropriate
    /// type to use.
    type NetworkingDriver: NetDriver;

    /// Initialize this Application.
    ///
    /// This will be called once when the [Engine] is [initializing](EngineStage::Init). It may be
    /// called again if the engine re-initializes e.g. after being suspended. Applications are
    /// expected to handle this accordingly if they expect the possibility of suspension.
    ///
    /// This method is asynchronous, and certain [Side] implementations may run additional logic
    /// asynchronously during this method. For example, [ClientGui](client::gui::ClientGui) runs
    /// window creation logic asynchronously, so that initializing logic can request window
    /// creations with deadlocking.
    async fn init(&self, engine: &Arc<Engine<Self, S>>);

    /// Execute a single main loop tick for this Application.
    ///
    /// Different [Side] implementations can run the main loop in different ways, but they should
    /// always execute this once per "tick" of their main loop. The `delta` duration can be used
    /// to calculate delta-based interactions, such as how far something should have moved if
    /// travelling at a given speed.
    fn tick(&self, engine: &Arc<Engine<Self, S>>, delta: Duration);
}

/// Application sided interface.
///
/// A Side represents how an [Application] is driven by the [Engine]. For example, a client Side
/// may contain GUI logic and drive the Application through a window-based event loop, while a
/// server Side may use a simpler and more direct main loop, or have different logic for handling
/// many client connections.
///
/// The engine provides several useful Side implementations, including the following:
///  * [Client](client::Client) - Basic main loop, no GUI.
///  * [ClientGui](client::gui::ClientGui) - Window-based event loop and input handling.
///
/// # Implementation
///
/// If the engine provided implementations don't fit your use-case, you can also implement your own.
/// Certain patterns should be followed when implementing your own Side.
///
/// ## Application lifecycle
///
/// Any implementation of [Side] should adhere to following lifecycle constraints:
///  * [Application::init()] should be executed once per engine [initialization](EngineStage::Init).
///  * [Application::tick()] should be executed on a regular basis while the engine is
///    [running](EngineStage::Running). The exact frequency is left up to the Side implementation.
///
/// The implementation is also responsible for transitioning engine [stages](EngineStage) as
/// appropriate.
///
/// ## Basic example
///
/// An extremely simplified example can be found as follows:
/// ```
/// use std::time::Instant;
/// use smol::future;
/// use gtether::{Application, EngineStage, EngineState, Side};
///
/// struct BasicSide {}
///
/// impl Side for BasicSide {
///     fn start<A: Application<Self>>(&self, mut engine_state: EngineState<A, Self>) {
///         {
///             // Initialization
///             let engine = engine_state.engine();
///             future::block_on(engine.app().init(&engine));
///             engine_state.set_stage(EngineStage::Running).unwrap();
///         }
///
///         let mut last_tick = Instant::now();
///         loop {
///             // Execute main loop ticks
///             // A more complex example may wish to target a certain amount of ticks per second
///             let tick_start = Instant::now();
///             let engine = engine_state.engine();
///             engine.app().tick(&engine, tick_start - last_tick);
///             last_tick = tick_start;
///
///             // Check for exit
///             if engine_state.stage() == EngineStage::Stopping {
///                 engine_state.set_stage(EngineStage::Stopped).unwrap();
///                 break;
///             }
///         }
///     }
/// }
/// ```
pub trait Side: Sized + Send + Sync {
    /// Start the [Engine].
    ///
    /// Begins executing the [Engine] lifecycle, as determined by this Side. Generally, that
    /// involves [initializing](EngineStage::Init) the engine, followed by
    /// [running](EngineStage::Running) some sort of main loop until the engine is stopped.
    fn start<A: Application<Self>>(&self, engine_state: EngineState<A, Self>);
}

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
/// ```
///
/// Stages are idempotent, and can always be transitioned to themselves; this should for all intents
/// and purposes be a "noop".
///
/// If a user attempts to transition the engine into an invalid stage, an
/// [InvalidEngineStageTransition] error will be generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStage {
    /// The [Engine] is currently initializing.
    ///
    /// This is where resources should first be loaded/created, and any long term application data
    /// is initialized via [Application::init()].
    Init,

    /// The [Engine] is currently running.
    ///
    /// While running, the engine should periodically call [Application::tick()] in order to advance
    /// application state.
    Running,

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
            (Self::Running, Self::Stopping) => Ok(()),
            (Self::Stopping, Self::Stopped) => Ok(()),
            _ => Err(InvalidEngineStageTransition { current: *self, next })
        }
    }
}

#[derive(Debug, Default)]
struct EngineStateInner {
    stage: EngineStage,
}

/// Mutable [Engine] state.
///
/// An instance of EngineState can be used to mutate the engine's internal state. Generally, these
/// instances cannot be created by the user, and are only given to [Side] implementations. Any
/// mutation to engine state is expected to come from [Side] implementations _only_.
pub struct EngineState<A, S>
where
    S: Side,
    A: Application<S>,
{
    engine: Arc<Engine<A, S>>,
}

impl<A, S> EngineState<A, S>
where
    S: Side,
    A: Application<S>,
{
    /// Get a reference to the [Engine] this EngineState represents.
    #[inline]
    pub fn engine(&self) -> &Arc<Engine<A, S>> {
        &self.engine
    }

    /// Get the current [EngineStage].
    ///
    /// This is effectively a shortcut for `self.engine().stage()`.
    #[inline]
    pub fn stage(&self) -> EngineStage {
        self.engine.stage()
    }

    /// Attempt to transition this [engine's](Engine) [stage](EngineStage).
    ///
    /// Will validate the given `stage` using [EngineStage::validate_transition()].
    pub fn set_stage(&self, stage: EngineStage) -> Result<(), InvalidEngineStageTransition> {
        let mut state = self.engine.state.write();
        state.stage.validate_transition(stage)?;
        state.stage = stage;
        Ok(())
    }
}

/// Represents an instance of the gTether Engine.
///
/// All engine usage stems from this structure, and it is required to construct one first.
pub struct Engine<A, S>
where
    S: Side,
    A: Application<S>,
{
    app: A,
    side: S,
    state: Arc<RwLock<EngineStateInner>>,
    resources: Arc<ResourceManager>,
    networking: Arc<Networking<A::NetworkingDriver>>,
}

impl<A, S> Engine<A, S>
where
    S: Side,
    A: Application<S>,
{
    #[inline]
    pub fn builder() -> EngineBuilder<A, S> {
        EngineBuilder::new()
    }

    #[inline]
    pub fn app(&self) -> &A { &self.app }

    #[inline]
    pub fn side(&self) -> &S { &self.side }

    #[inline]
    pub fn stage(&self) -> EngineStage {
        self.state.read().stage
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

        self.side.start(engine_state);
    }

    pub fn stop(&self) -> Result<(), InvalidEngineStageTransition> {
        let mut state = self.state.write();
        if state.stage == EngineStage::Stopped {
            // Already stopped, nothing to do
            Ok(())
        } else {
            // Attempt to begin stopping
            state.stage.validate_transition(EngineStage::Stopping)?;
            state.stage = EngineStage::Stopping;
            Ok(())
        }
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
pub struct EngineJoinHandle<A, S>
where
    S: Side,
    A: Application<S>,
{
    join_handle: JoinHandle<()>,
    engine: Arc<Engine<A, S>>,
}

impl<A, S> EngineJoinHandle<A, S>
where
    S: Side,
    A: Application<S>,
{
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
    pub fn engine(&self) -> Arc<Engine<A, S>> {
        self.engine.clone()
    }
}

/// Build a new gTether Engine.
///
/// # Examples
///
/// ```
/// # use std::sync::Arc;
/// # use std::time::Duration;
/// use async_trait::async_trait;
/// use gtether::{Application, Engine};
/// use gtether::client::Client;
/// use gtether::net::driver::NoNetDriver;
///
/// struct MyApp {}
///
/// #[async_trait(?Send)]
/// impl Application<Client> for MyApp {
///    type NetworkingDriver = NoNetDriver;
///     // Implement relevant functions...
/// #    async fn init(&self, engine: &Arc<Engine<Self, Client>>) {}
/// #    fn tick(&self, engine: &Arc<Engine<Self, Client>>, delta: Duration) {}
/// }
///
/// let app = MyApp {};
/// let engine = Engine::builder()
///     .app(app)
///     .side(Client::builder().build())
///     .networking_driver(NoNetDriver::new())
///     .build();
/// ```
pub struct EngineBuilder<A, S>
where
    S: Side,
    A: Application<S>,
{
    app: Option<A>,
    side: Option<S>,
    resources: Option<Arc<ResourceManager>>,
    networking: Option<Arc<Networking<A::NetworkingDriver>>>,
}

impl<A, S> EngineBuilder<A, S>
where
    S: Side,
    A: Application<S>,
{
    pub fn new() -> Self {
        Self {
            app: None,
            side: None,
            resources: None,
            networking: None,
        }
    }

    pub fn app(mut self, app: A) -> Self {
        self.app = Some(app);
        self
    }

    pub fn side(mut self, side: S) -> Self {
        self.side = Some(side);
        self
    }

    /// Configure a main [ResourceManager] for the engine.
    ///
    /// If no [ResourceManager] is specified, a default one with no sources - effectively a noop
    /// manager - will be created.
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

    pub fn build(self) -> Arc<Engine<A, S>> {
        let app = self.app
            .expect(".app() is required");

        let side = self.side
            .expect(".side() is required");

        let resources = self.resources
            .unwrap_or_else(|| ResourceManager::builder().build());

        let networking = self.networking
            .expect(".networking_driver() is required");

        Arc::new(Engine {
            app,
            side,
            state: Arc::new(RwLock::new(EngineStateInner::default())),
            resources,
            networking,
        })
    }
}

impl<A, S> EngineBuilder<A, S>
where
    S: Side + 'static,
    A: Application<S> + 'static,
{
    /// Start the [Engine] in a separate thread.
    ///
    /// Spawn a separate thread, and start the [Engine] in it. This will yield an [EngineJoinHandle]
    /// to manage the new [Engine] thread.
    pub fn spawn(self) -> EngineJoinHandle<A, S> {
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