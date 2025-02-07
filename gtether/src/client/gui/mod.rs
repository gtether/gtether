//! Client-side GUI logic.
//!
//! While the [`client`](crate::client) module provides basic "client-side" logic support, this module
//! provides client-side GUI support, which includes both windowing management and input handling.
//!
//! For windowing management, see the [`window`] module.
//!
//! For input handling, see the [`input`] module.
//!
//! To get started with a client-side GUI, see [ClientGui], or for a starting example:
//! ```no_run
//! # use std::sync::Arc;
//! # use std::time::Duration;
//! use async_trait::async_trait;
//! use gtether::client::gui::ClientGui;
//! use gtether::{Application, Engine};
//!
//! struct StartingGuiApp {}
//!
//! #[async_trait(?Send)]
//! impl Application<ClientGui> for StartingGuiApp {
//!     // Implement relevant functions...
//! #     async fn init(&self, engine: &Arc<Engine<Self, ClientGui>>) {}
//! #     fn tick(&self, engine: &Arc<Engine<Self, ClientGui>>, delta: Duration) {}
//! }
//!
//! let app = StartingGuiApp {};
//!
//! let side = ClientGui::builder()
//!     .application_name("StartingApplication")
//!     // 60 ticks per second
//!     .tick_rate(60)
//!     .build();
//!
//! Engine::builder()
//!     .app(app)
//!     .side(side)
//!     .build()
//!     .start();
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::RwLock;
use smol::future;
use smol::future::FutureExt;
use vulkano::instance::InstanceExtensions;
use vulkano::swapchain::Surface;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use window::WindowManager;

use crate::{Application, EngineStage, EngineState, Side};
use crate::client::gui::window::{CreateWindowInfo, WindowHandle};
use crate::render::Instance;

pub mod window;
pub mod input;

pub(crate) struct WindowOrchestrator<A: Application<ClientGui>> {
    engine_state: EngineState<A, ClientGui>,
    manager: WindowManager,
    min_tick_duration: Duration,
    last_tick: Instant,
}

impl<A: Application<ClientGui>> ApplicationHandler for WindowOrchestrator<A> {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        match (cause, self.engine_state.stage()) {
            (StartCause::Poll, EngineStage::Running) => {
                // Always run bookkeeping
                self.manager.tick(event_loop);

                // TODO: May want to move this after other event processing, in order to process input first
                let tick_start = Instant::now();
                let delta = tick_start - self.last_tick;
                // Try to match configured tick rate
                if delta > self.min_tick_duration {
                    let engine = self.engine_state.engine();
                    engine.app().tick(&engine, delta);
                    self.last_tick = tick_start;
                }
            },
            (StartCause::Poll, EngineStage::Stopping) => {
                event_loop.exit();
                self.engine_state.set_stage(EngineStage::Stopped).unwrap();
            }
            _ => {},
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match self.engine_state.stage() {
            EngineStage::Init => {
                let engine = self.engine_state.engine();

                future::block_on(
                    engine.app().init(&engine)
                        .or(self.manager.run_async_msg_loop(event_loop))
                );

                self.engine_state.set_stage(EngineStage::Running).unwrap();
            },
            _ => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        self.manager.window_event(window_id, event, event_loop);
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _device_id: DeviceId, event: DeviceEvent) {
        self.manager.device_event(event);
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // TODO
    }
}

enum ClientGuiState {
    Created(ClientGuiBuilder),
    Started {
        render_instance: Arc<Instance>,
        msg_create_client: ump::Client<CreateWindowInfo, WindowHandle, ()>,
    },
}

/// [Side] implementation with GUI support.
///
/// A ClientGui provides windowing support, and therefore should be the preferred Side
/// implementation to use for most games on the player side. Windowing support includes providing a
/// surface/s to render on using the [render module](crate::render), as well as input handling
/// capabilities.
///
/// Windowing support *is* platform-specific, however, and so ClientGui is only supported on some
/// platforms. Currently, that list is as follows:
///  * x86_64 windows
///    * aarch64 is likely to work fine, but isn't tested
///  * x86_64 linux
///    * aarch64 is likely to work fine, but isn't tested
pub struct ClientGui {
    state: RwLock<ClientGuiState>,
}

impl ClientGui {
    /// Create a builder for ClientGui.
    ///
    /// This is the recommended way to create a new ClientGui.
    #[inline]
    pub fn builder() -> ClientGuiBuilder {
        ClientGuiBuilder::new()
    }

    fn new(builder: ClientGuiBuilder) -> Self {
        Self {
            state: RwLock::new(ClientGuiState::Created(builder)),
        }
    }

    /// The [render instance](Instance) for this ClientGui.
    #[inline]
    pub fn render_instance(&self) -> Arc<Instance> {
        match &*self.state.read() {
            ClientGuiState::Started { render_instance, .. } => render_instance.clone(),
            _ => panic!("ClientGuiSide not started yet"),
        }
    }

    /// Create a new window.
    ///
    /// Sends a request to the main event loop to create a new window. Yields a [ump::WaitReply]
    /// which can be used to retrieve the [WindowHandle] once the window has been created.
    ///
    /// <div class="warning">
    ///
    /// NOTE: Do NOT wait on the [ump::WaitReply] in the same thread as the main event loop. At the
    /// moment, this means the thread running [Application::tick()]. This will cause a deadlock, as
    /// control will never be yielded to the event loop to actually do the window creation.
    ///
    /// Asynchronously awaiting in [Application::init()] is fine however, as control can context
    /// switch while awaiting.
    ///
    /// </div>
    #[inline]
    pub fn create_window(&self, create_info: CreateWindowInfo) -> ump::WaitReply<WindowHandle, ()> {
        match &*self.state.read() {
            ClientGuiState::Started { msg_create_client, .. } => {
                msg_create_client.req_async(create_info).unwrap()
            },
            _ => panic!("ClientGuiSide not started yet"),
        }
    }
}

impl Side for ClientGui {
    fn start<A: Application<Self>>(&self, engine_state: EngineState<A, Self>) {
        let (event_loop, mut handler) = {
            let mut state = self.state.write();
            let (new_state, event_loop, handler) = match &*state {
                ClientGuiState::Created(builder) => {
                    builder.start(engine_state)
                },
                _ => panic!("ClientGuiSide can only be started once"),
            };
            *state = new_state;
            (event_loop, handler)
        };

        event_loop.run_app(&mut handler)
            .expect("Failed to run winit loop");
    }
}

/// Builder pattern for [ClientGui].
///
/// # Examples
/// ```no_run
/// # use std::sync::Arc;
/// # use std::time::Duration;
/// # use async_trait::async_trait;
/// use gtether::client::gui::ClientGui;
/// use gtether::Engine;
/// # use gtether::Application;
/// #
/// # struct MyApp {}
/// #
/// # #[async_trait(?Send)]
/// # impl Application<ClientGui> for MyApp {
/// #     // Implement relevant functions...
/// #     async fn init(&self, engine: &Arc<Engine<Self, ClientGui>>) {}
/// #     fn tick(&self, engine: &Arc<Engine<Self, ClientGui>>, delta: Duration) {}
/// # }
/// #
/// # let app = MyApp {};
///
/// let side = ClientGui::builder()
///     .application_name("MyApplication")
///     // 60 ticks per second
///     .tick_rate(60)
///     .build();
///
/// let engine = Engine::builder()
///     .app(app)
///     .side(side)
///     .build();
/// ```
#[derive(Clone)]
pub struct ClientGuiBuilder {
    application_name: Option<String>,
    tick_rate: usize,
    render_extensions: InstanceExtensions,
}

impl ClientGuiBuilder {
    /// Create a new ClientGuiBuilder.
    ///
    /// It is recommended to use [ClientGui::builder()] instead.
    #[inline]
    pub fn new() -> Self {
        Self {
            application_name: None,
            tick_rate: 60,
            render_extensions: InstanceExtensions::default(),
        }
    }

    /// Set the application name.
    ///
    /// This name is used e.g. in Vulkan drivers to identify your application.
    ///
    /// Default is `None`.
    #[inline]
    pub fn application_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.application_name = Some(name.into());
        self
    }

    /// Set the tick rate.
    ///
    /// [ClientGui] will attempt to drive the [Application] at this tick rate. If ticks take too
    /// long, some ticks may be skipped resulting in a lower tick rate, but the tick rate will never
    /// be higher than this number.
    ///
    /// Default is `60`.
    #[inline]
    pub fn tick_rate(&mut self, tick_rate: usize) -> &mut Self {
        self.tick_rate = tick_rate;
        self
    }

    /// Define any additional Vulkan render extensions that are required.
    ///
    /// Any extensions required for Surface rendering for the current platform will be automatically
    /// requested, but this method can be used to request additional extensions.
    ///
    /// This method may be called multiple times, and any additional calls will simply add to the
    /// set of required extensions.
    #[inline]
    pub fn render_extensions(&mut self, extensions: InstanceExtensions) -> &mut Self {
        self.render_extensions |= extensions;
        self
    }

    /// Build a new [ClientGui].
    #[inline]
    pub fn build(&self) -> ClientGui {
        ClientGui::new(self.clone())
    }

    fn start<A: Application<ClientGui>>(&self, engine_state: EngineState<A, ClientGui>)
        -> (ClientGuiState, EventLoop<()>, WindowOrchestrator<A>)
    {
        let event_loop = EventLoop::builder()
            .build().unwrap();
        event_loop.set_control_flow(ControlFlow::Poll);

        let render_extensions = self.render_extensions.clone()
            | Surface::required_extensions(&event_loop);
        let render_instance = Arc::new(Instance::new(
            self.application_name.clone(),
            render_extensions,
        ));

        let min_tick_duration = Duration::from_secs_f32(1.0 / self.tick_rate as f32);

        let (manager, msg_create_client) = WindowManager::new(
            self.application_name.clone(),
            render_instance.clone(),
        );

        let handler = WindowOrchestrator {
            engine_state,
            manager,
            min_tick_duration,
            last_tick: Instant::now(),
        };

        let new_state = ClientGuiState::Started {
            render_instance,
            msg_create_client,
        };

        (new_state, event_loop, handler)
    }
}