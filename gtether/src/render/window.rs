use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Instant;

use ump::Error;
use vulkano::instance::{Instance, InstanceCreateInfo};
use vulkano::swapchain::Surface;
use vulkano::VulkanLibrary;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, DeviceId, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window as WinitWindow, WindowId};

use crate::{Engine, EngineMetadata, EngineState, Application, NonExhaustive, Registry};
use crate::render::{Device, Dimensions, RenderTarget};
use crate::render::render_pass::{EngineRenderPass, EngineRenderPassBuilder};
use crate::render::Renderer;

pub use winit::window::WindowAttributes;

struct WindowRenderTarget {
    winit_window: Arc<WinitWindow>,
    surface: Arc<Surface>,
    device: Arc<Device>,
}

impl WindowRenderTarget {
    fn new(winit_window: Arc<WinitWindow>, instance: Arc<Instance>) -> Self {
        let surface = Surface::from_window(
            instance.clone(),
            winit_window.clone(),
        ).unwrap();

        let device = Arc::new(Device::for_surface(
            instance.clone(),
            surface.clone(),
        ));

        Self {
            winit_window,
            surface,
            device,
        }
    }

    #[inline]
    fn winit_window(&self) -> &Arc<WinitWindow> { &self.winit_window }
}

impl From<PhysicalSize<u32>> for Dimensions {
    #[inline]
    fn from(value: PhysicalSize<u32>) -> Self { Dimensions([value.width, value.height]) }
}

impl RenderTarget for WindowRenderTarget {
    #[inline]
    fn surface(&self) -> &Arc<Surface> { &self.surface }

    #[inline]
    fn dimensions(&self) -> Dimensions { self.winit_window.inner_size().into() }

    #[inline]
    fn device(&self) -> &Arc<Device> { &self.device }
}

/// Parameters to create a new window.
#[derive(Clone, Debug)]
pub struct CreateWindowInfo {
    /// Attributes to use for the window.
    pub attributes: WindowAttributes,

    /// Should the engine exit when this window is closed.
    ///
    /// Default is true.
    pub exit_on_close: bool,

    pub _ne: NonExhaustive,
}

impl Default for CreateWindowInfo {
    fn default() -> Self {
        Self {
            attributes: WindowAttributes::default(),
            exit_on_close: true,
            _ne: NonExhaustive(()),
        }
    }
}

struct WindowModifyRequest {
    render_pass: Option<Box<dyn EngineRenderPass>>,
}

impl Default for WindowModifyRequest {
    fn default() -> Self {
        Self {
            render_pass: None,
        }
    }
}

/// Handle for a specific window.
///
/// Provides access to window information and manipulation methods.
#[derive(Clone)]
pub struct WindowHandle {
    id: WindowId,
    target: Arc<dyn RenderTarget>,
    sender_modify: ump::Client<WindowModifyRequest, (), ()>,
    sender_event: ump::Client<WindowEvent, (), ()>,
}

impl WindowHandle {
    /// The [winit::window::WindowId] for this window.
    #[inline]
    pub fn id(&self) -> WindowId { self.id }

    /// The [RenderTarget] for this window.
    #[inline]
    pub fn render_target(&self) -> &Arc<dyn RenderTarget> { &self.target }

    /// Replace this window's [RenderTarget] with a new one.
    ///
    /// Sends the new [RenderTarget] to the window's main thread, and blocks until a response has
    /// been received which indicates the [RenderTarget] was successfully replaced.
    ///
    /// # Panics
    ///
    /// - Panics if the window's main thread has stopped or is otherwise unresponsive.
    pub fn set_render_pass(&self, render_pass: Box<dyn EngineRenderPass>) {
        let modify_request = WindowModifyRequest {
            render_pass: Some(render_pass),
            ..Default::default()
        };

        self.sender_modify.req(modify_request).unwrap();
    }

    /// Requests this window to close.
    ///
    /// Sends a request which may be honored by the window when available. Does not block.
    ///
    /// # Panics
    ///
    /// - Panics if the window's main thread has stopped or is otherwise unresponsive.
    pub fn request_close(&self) {
        self.sender_event.req(WindowEvent::CloseRequested).unwrap();
    }

    fn handle_event(&self, event: WindowEvent) -> Result<(), Error<()>> {
        // Send the event without blocking
        match self.sender_event.req_async(event) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

struct Window {
    target: Arc<WindowRenderTarget>,
    renderer: Renderer,
    endpoint_modify: ump::Server<WindowModifyRequest, (), ()>,
    endpoint_event: ump::Server<WindowEvent, (), ()>,
}

impl Window {
    fn new(
        attributes: WindowAttributes,
        event_loop: &ActiveEventLoop,
        instance: &Arc<Instance>,
        engine_metadata: &EngineMetadata,
    ) -> (WindowHandle, thread::JoinHandle<()>) {
        let (endpoint_modify, sender_modify) = ump::channel();
        let (endpoint_event, sender_event) = ump::channel();

        let mut attributes = attributes;
        if attributes.title == "winit window".to_owned() {
            attributes.title = engine_metadata.application_name.clone()
                .unwrap_or("gTether Window".into());
        }
        let title = attributes.title.clone();

        let winit_window = Arc::new(event_loop.create_window(attributes).unwrap());

        let window_id = winit_window.id();

        let target = Arc::new(WindowRenderTarget::new(
            winit_window,
            instance.clone(),
        ));

        let window_handle = WindowHandle {
            id: window_id,
            target: target.clone(),
            sender_modify,
            sender_event,
        };

        let join_handle = thread::Builder::new().name(title).spawn(move || {
            let dyn_target: Arc<dyn RenderTarget> = target.clone();
            let render_pass = EngineRenderPassBuilder::noop(&dyn_target);
            let renderer = Renderer::new(&dyn_target, render_pass);

            let mut window = Self {
                target,
                renderer,
                endpoint_modify,
                endpoint_event,
            };

            loop {
                if !window.tick().is_ok() {
                    break;
                }
            }
        }).unwrap();

        (window_handle, join_handle)
    }

    fn tick(&mut self) -> Result<(), ()> {
        let mut status = Ok(());

        self.renderer.render();

        while let Some((modify_request, rctx)) = self.endpoint_modify.try_pop().unwrap() {
            if let Some(render_pass) = modify_request.render_pass {
                self.renderer.set_render_pass(render_pass);
            }

            rctx.reply(()).unwrap();
        }

        while let Some((event, rctx)) = self.endpoint_event.try_pop().unwrap() {
            let window_id = self.target.winit_window().id();
            match event {
                WindowEvent::CloseRequested => {
                    println!("Window {window_id:?} has received the signal to close");
                    status = Err(());
                },
                WindowEvent::Resized(_) => {
                    println!("Window {window_id:?} has resized");
                    self.renderer.mark_stale();
                },
                _ => ()
            }

            rctx.reply(()).unwrap();
        }

        status
    }
}

struct WindowEntry {
    window_handle: WindowHandle,
    join_handle: thread::JoinHandle<()>,
    exit_on_close: bool,
}

struct WindowManager {
    engine_metadata: EngineMetadata,
    vulkan_instance: Arc<Instance>,
    windows: HashMap<WindowId, WindowEntry>,
}

impl WindowManager {
    fn new(engine_metadata: EngineMetadata, vulkan_instance: Arc<Instance>) -> Self {
        Self {
            engine_metadata,
            vulkan_instance,
            windows: HashMap::new(),
        }
    }

    fn tick(&mut self, event_loop: &ActiveEventLoop) {
        // Check for any exited windows
        self.windows.retain(|_, entry| {
            let finished = entry.join_handle.is_finished();
            if finished && entry.exit_on_close {
                event_loop.exit();
            }
            !finished
        });
    }

    fn window_event(&mut self, window_id: WindowId, event: WindowEvent, event_loop: &ActiveEventLoop) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(entry) = self.windows.remove(&window_id) {
                    // Don't care if this errors, because we'll be removing the window anyway
                    entry.window_handle.handle_event(event).unwrap_or_default();
                    // TODO: Do we care about joining this? Possibly just drop the reference and move on
                    entry.join_handle.join().unwrap();
                    if entry.exit_on_close {
                        event_loop.exit();
                    }
                }
            },
            _ => {
                if let Some(entry) = self.windows.get(&window_id) {
                    entry.window_handle.handle_event(event).unwrap_or_else(|err| {
                        println!("Window {window_id:?} errored when handling event ({err:?}), dropping");
                        self.windows.remove(&window_id);
                    });
                }
            },
        }
    }
}

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
        let (handle, join_handle) = Window::new(
            create_info.attributes,
            self.event_loop,
            &self.manager.vulkan_instance,
            &self.manager.engine_metadata,
        );

        self.manager.windows.insert(handle.id(), WindowEntry {
            window_handle: handle.clone(),
            join_handle,
            exit_on_close: create_info.exit_on_close,
        });
        handle
    }
}

pub(crate) struct WindowAppHandler<A: Application> {
    engine: Engine<A>,
    manager: WindowManager,
    last_tick: Instant,
}

impl<A: Application> ApplicationHandler for WindowAppHandler<A> {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        match (cause, self.engine.state) {
            (StartCause::Poll, EngineState::Running) => {
                let now = Instant::now();

                self.manager.tick(event_loop);

                self.engine.app.tick(&self.engine, now - self.last_tick);

                if self.engine.should_exit.load(Ordering::Relaxed) {
                    event_loop.exit();
                }

                self.last_tick = now;
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

                self.engine.game().init(&self.engine, &mut registry);

                self.engine.state = EngineState::Running;
            },
            _ => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        self.manager.window_event(window_id, event, event_loop);
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _device_id: DeviceId, _event: DeviceEvent) {
        // TODO
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // TODO
    }
}

impl<A: Application> WindowAppHandler<A> {
    pub(crate) fn start(engine: Engine<A>) {
        let event_loop = EventLoop::builder()
            .build().unwrap();
        event_loop.set_control_flow(ControlFlow::Poll);

        // TODO: extract library + instance if they are required somewhere besides windowing
        let library = VulkanLibrary::new()
            .expect("Failed to load Vulkan library/DLL");

        let required_extensions = Surface::required_extensions(&event_loop);

        let vulkan_instance = Instance::new(library, InstanceCreateInfo {
            application_name: engine.metadata().application_name.clone(),
            engine_name: Some("gTether".to_owned()),
            // TODO: Engine version
            // engine_version: ,
            enabled_extensions: required_extensions,
            ..Default::default()
        }).expect("Failed to create instance");

        let manager = WindowManager::new(engine.metadata().clone(), vulkan_instance);

        let mut handler = Self {
            engine,
            manager,
            last_tick: Instant::now(),
        };

        event_loop.run_app(&mut handler)
            .expect("Failed to run winit loop");
    }
}