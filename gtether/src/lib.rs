#[cfg(test)] #[macro_use]
extern crate assert_matches;
extern crate nalgebra_glm as glm;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub mod event;
#[cfg(feature = "gui")]
pub mod gui;
#[cfg(feature = "graphics")]
pub mod render;
pub mod console;
pub mod resource;

pub struct Registry<'a> {
    #[cfg(feature = "gui")]
    pub window: gui::WindowRegistry<'a>,
}

pub trait Application: Sized {
    fn init(&self, engine: &Engine<Self>, registry: &mut Registry);
    fn tick(&self, engine: &Engine<Self>, delta: Duration);
}

/// Various metadata relating to an instance of the gTether Engine.
///
/// Used to construct a new Engine. Metadata includes things like the name of the application that
/// is utilizing the engine.
#[derive(Debug, Clone)]
pub struct EngineMetadata {
    pub application_name: Option<String>,

    pub _ne: NonExhaustive,
}

impl Default for EngineMetadata {
    fn default() -> Self {
        Self {
            application_name: None,
            _ne: NonExhaustive(()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EngineState {
    Init,
    Running,
}

/// Represents an instance of the gTether Engine.
///
/// All engine usage stems from this structure, and it is required to construct one first.
pub struct Engine<A: Application> {
    metadata: EngineMetadata,
    state: EngineState,
    app: A,
    pub(crate) should_exit: AtomicBool,
    #[cfg(feature = "graphics")]
    render_instance: Arc<render::Instance>,
    #[cfg(feature = "gui")]
    window_starter: Option<gui::WindowOrchestratorStarter>,
}

impl<A: Application> Engine<A> {
    #[inline]
    pub fn metadata(&self) -> &EngineMetadata { &self.metadata }

    #[inline]
    pub fn app(&self) -> &A { &self.app }

    #[inline]
    pub fn state(&self) -> EngineState { self.state }

    #[cfg(feature = "graphics")]
    #[inline]
    pub fn render_instance(&self) -> &Arc<render::Instance> { &self.render_instance }

    #[cfg(feature = "gui")]
    pub fn start(self) {
        let mut engine = self;
        let starter = engine.window_starter.take().unwrap();
        starter.start(engine);
    }

    #[cfg(not(feature = "gui"))]
    pub fn start(self) {
        todo!()
    }

    pub fn request_exit(&self) {
        self.should_exit.store(true, Ordering::Relaxed);
    }
}

/// Build a new gTether Engine.
///
/// # Examples
///
/// ```
/// # use std::time::Duration;
/// # use gtether::{Engine, Registry};
/// use gtether::{Application, EngineBuilder, EngineMetadata};
///
/// struct MyApp {};
///
/// impl Application for MyApp {
///     // Implement relevant functions...
/// #    fn init(&self, engine: &Engine<Self>, registry: &mut Registry) {}
/// #    fn tick(&self, engine: &Engine<Self>, delta: Duration) {}
/// }
/// 
/// let app = MyApp {};
/// let engine = EngineBuilder::new()
///     .metadata(EngineMetadata {
///         application_name: Some("My Application".into()),
///         ..Default::default()
///     })
///     .app(app)
///     .build();
/// ```
pub struct EngineBuilder<A: Application> {
    metadata: Option<EngineMetadata>,
    app: Option<A>,
    #[cfg(feature = "graphics")]
    render_extensions: vulkano::instance::InstanceExtensions,
}

impl<A: Application> EngineBuilder<A> {
    pub fn new() -> Self {
        Self {
            metadata: None,
            app: None,
            render_extensions: vulkano::instance::InstanceExtensions::default(),
        }
    }

    pub fn metadata(mut self, metadata: EngineMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn app(mut self, app: A) -> Self {
        self.app = Some(app);
        self
    }

    #[cfg(feature = "graphics")]
    pub fn render_extensions(mut self, extensions: vulkano::instance::InstanceExtensions) -> Self {
        self.render_extensions |= extensions;
        self
    }

    pub fn build(self) -> Engine<A> {
        let metadata = self.metadata
            .expect(".metadata() is required");

        let app = self.app
            .expect(".app() is required");

        #[cfg(feature = "gui")]
        let window_starter = gui::WindowOrchestratorStarter::new();

        #[cfg(feature = "graphics")]
        let render_instance = {
            let mut render_extensions = self.render_extensions;

            #[cfg(feature = "gui")]
            { render_extensions |= window_starter.render_extensions(); }

            Arc::new(render::Instance::new(&metadata, render_extensions))
        };

        Engine {
            metadata,
            state: EngineState::Init,
            app,
            should_exit: AtomicBool::new(false),
            #[cfg(feature = "graphics")]
            render_instance,
            #[cfg(feature = "gui")]
            window_starter: Some(window_starter),
        }
    }
}

/// A helper type for non-exhaustive structs.
///
/// This allows structures to be created via a constructor function or by update syntax in
/// combination with `Default::default()`. Copied from and inspired by Vulkano's NonExhaustive.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NonExhaustive(pub(crate) ());
