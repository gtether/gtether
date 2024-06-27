use std::sync::atomic::Ordering;
use std::time::Instant;

use vulkano::instance::InstanceExtensions;
use vulkano::swapchain::Surface;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use window::WindowManager;

use crate::{Application, Engine, EngineState, Registry};
use crate::gui::window::{CreateWindowInfo, WindowHandle};

pub mod window;
pub mod input;

pub struct WindowRegistry<'a> {
    event_loop: &'a ActiveEventLoop,
    manager: &'a mut WindowManager,
}

impl WindowRegistry<'_> {
    /// Create a new window.
    ///
    /// Immediately starts the window and returns a handle. This handle cannot be retrieved again
    /// later, so it should be stored by the user.
    #[inline]
    pub fn create_window(&mut self, create_info: CreateWindowInfo) -> WindowHandle {
        self.manager.create_window(create_info, self.event_loop)
    }
}

pub(crate) struct WindowOrchestrator<A: Application> {
    engine: Engine<A>,
    manager: WindowManager,
    last_tick: Instant,
}

impl<A: Application> ApplicationHandler for WindowOrchestrator<A> {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        match (cause, self.engine.state) {
            (StartCause::Poll, EngineState::Running) => {
                // Always run bookkeeping
                self.manager.tick(event_loop);
                if self.engine.should_exit.load(Ordering::Relaxed) {
                    event_loop.exit();
                }

                // TODO: May want to move this after other event processing, in order to process input first
                let tick_start = Instant::now();
                let delta = tick_start - self.last_tick;
                // Try to tick ~60 times a second
                // TODO: Make this configurable
                if delta.as_secs_f32() > 1.0 / 60.0 {
                    self.engine.app.tick(&self.engine, delta);
                    self.last_tick = tick_start;
                }
            },
            _ => {},
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match self.engine.state {
            EngineState::Init => {
                let window = WindowRegistry {
                    event_loop,
                    manager: &mut self.manager,
                };

                let mut registry = Registry {
                    window,
                };

                self.engine.app.init(&self.engine, &mut registry);

                self.engine.state = EngineState::Running;
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

impl<A: Application> WindowOrchestrator<A> {
    pub(crate) fn start(engine: Engine<A>, event_loop: EventLoop<()>) {
        let manager = WindowManager::new(
            engine.metadata().clone(),
            engine.render_instance().clone(),
        );

        let mut handler = Self {
            engine,
            manager,
            last_tick: Instant::now(),
        };

        event_loop.run_app(&mut handler)
            .expect("Failed to run winit loop");
    }
}

pub(crate) struct WindowOrchestratorStarter {
    event_loop: EventLoop<()>,
}

impl WindowOrchestratorStarter {
    pub(crate) fn new() -> Self {
        let event_loop = EventLoop::builder()
            .build().unwrap();
        event_loop.set_control_flow(ControlFlow::Poll);

        Self {
            event_loop,
        }
    }

    pub(crate) fn render_extensions(&self) -> InstanceExtensions {
        Surface::required_extensions(&self.event_loop)
    }

    pub(crate) fn start<A: Application>(self, engine: Engine<A>) {
        WindowOrchestrator::start(engine, self.event_loop)
    }
}
