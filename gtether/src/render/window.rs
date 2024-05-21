use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use ump::Error;
use vulkano::instance::{Instance, InstanceCreateInfo};
use vulkano::swapchain::Surface;
use vulkano::VulkanLibrary;
use winit::dpi::PhysicalSize;
use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopBuilder, EventLoopWindowTarget};
#[cfg(target_os = "windows")]
use winit::platform::windows::EventLoopBuilderExtWindows;
#[cfg(target_os = "linux")]
use winit::platform::x11::EventLoopBuilderExtX11;
use winit::window::{Window as WinitWindow, WindowId};
use winit::window::WindowBuilder as WinitWindowBuilder;

use crate::{EngineMetadata, NonExhaustive};
use crate::render::{Device, Dimensions, RenderTarget};
use crate::render::render_pass::{EngineRenderPass, EngineRenderPassBuilder};
use crate::render::Renderer;

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
    /// The title to use for the window.
    ///
    /// The default value is empty, which will use an auto-generated name based on the application.
    pub title: Option<String>,

    pub _ne: NonExhaustive,
}

impl Default for CreateWindowInfo {
    fn default() -> Self {
        Self {
            title: None,
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
        create_info: CreateWindowInfo,
        elwt: &EventLoopWindowTarget<()>,
        instance: &Arc<Instance>,
        engine_metadata: &EngineMetadata,
    ) -> (WindowHandle, thread::JoinHandle<()>) {
        let (endpoint_modify, sender_modify) = ump::channel();
        let (endpoint_event, sender_event) = ump::channel();

        let title = {
            if let Some(title) = create_info.title {
                title
            } else if let Some(title) = engine_metadata.application_name.clone() {
                title
            } else {
                "gTether Window".into()
            }
        };

        let winit_window = Arc::new(WinitWindowBuilder::new()
            .with_resizable(true)
            .with_title(title.clone())
            .build(elwt).unwrap()
        );

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

#[derive(Default)]
struct WindowStatusRequest {
    id: Option<WindowId>,
}

struct WindowStatus {
    window_handle: Option<WindowHandle>,
    window_count: u32,
}

/// Handle for the window manager.
///
/// Provides access to window creation and management methods.
#[derive(Clone)]
pub struct WindowManagerClient {
    sender_create: ump::Client<CreateWindowInfo, WindowHandle, ()>,
    sender_status: ump::Client<WindowStatusRequest, WindowStatus, ()>,
    should_exit: Arc<AtomicBool>,
}

impl WindowManagerClient {
    /// Create a new window.
    ///
    /// Blocks until the window is created and a handle is returned.
    ///
    /// # Panics
    ///
    /// - Panics if the window management thread has stopped or is otherwise unresponsive.
    #[inline]
    pub fn create_window(&self, create_info: CreateWindowInfo) -> WindowHandle {
        self.sender_create.req(create_info).unwrap()
    }

    /// Retrieves a [WindowHandle] for a given window.
    ///
    /// Blocks until the handle is returned. If there is no window associated with the given
    /// [WindowId], returns None.
    ///
    /// # Panics
    ///
    /// - Panics if the window management thread has stopped or is otherwise unresponsive.
    #[inline]
    pub fn get_handle(&self, window_id: WindowId) -> Option<WindowHandle> {
        let status_request = WindowStatusRequest {
            id: Some(window_id),
        };

        self.sender_status.req(status_request).unwrap().window_handle
    }

    /// Retrieves the total count of active windows.
    ///
    /// Blocks until a response is received, which means this method is not suitable for hot paths,
    /// and responses should be cached by the caller if reasonable to do so.
    ///
    /// # Panics
    ///
    /// - Panics if the window management thread has stopped or is otherwise unresponsive.
    #[inline]
    pub fn get_window_count(&self) -> u32 {
        self.sender_status.req(WindowStatusRequest::default()).unwrap().window_count
    }

    #[inline]
    pub(crate) fn request_exit(&self) {
        self.should_exit.store(true, Ordering::Relaxed);
    }
}

pub(crate) struct WindowManager {
    engine_metadata: EngineMetadata,
    vulkan_instance: Arc<Instance>,
    windows: HashMap<WindowId, (WindowHandle, thread::JoinHandle<()>)>,
    endpoint_create: ump::Server<CreateWindowInfo, WindowHandle, ()>,
    endpoint_status: ump::Server<WindowStatusRequest, WindowStatus, ()>,
    should_exit: Arc<AtomicBool>,
}

impl WindowManager {
    pub(crate) fn start(engine_metadata: EngineMetadata) -> (WindowManagerClient, thread::JoinHandle<()>) {
        let (endpoint_create, sender_create) = ump::channel();
        let (endpoint_status, sender_status) = ump::channel();
        let should_exit = Arc::new(AtomicBool::new(false));

        let client = WindowManagerClient {
            sender_create,
            sender_status,
            should_exit: should_exit.clone(),
        };

        let join_handle = thread::Builder::new().name("window-manager".into()).spawn(move || {
            let event_loop = EventLoopBuilder::new()
                // TODO: any_thread() may pose platform compat issues in the future, but should
                //  work on at least Unix + Windows
                .with_any_thread(true)
                .build().unwrap();
            event_loop.set_control_flow(ControlFlow::Poll);

            // TODO: extract library + instance if they are required somewhere besides windowing
            let library = VulkanLibrary::new()
                .expect("Failed to load Vulkan library/DLL");

            let required_extensions = Surface::required_extensions(&event_loop);

            let vulkan_instance = Instance::new(library, InstanceCreateInfo {
                application_name: engine_metadata.application_name.clone(),
                engine_name: Some("gTether".to_owned()),
                // TODO: Engine version
                // engine_version: ,
                enabled_extensions: required_extensions,
                ..Default::default()
            }).expect("Failed to create instance");

            let mut manager = Self {
                engine_metadata,
                vulkan_instance,
                windows: HashMap::new(),
                endpoint_create,
                endpoint_status,
                should_exit,
            };

            event_loop.run(move |event, elwt| {
                manager.handle_event(event, elwt);
            }).expect("Failed to run winit loop");
        }).unwrap();

        (client, join_handle)
    }

    fn handle_event(&mut self, event: Event<()>, elwt: &EventLoopWindowTarget<()>) {
        match event {
            Event::WindowEvent { event, window_id } => {
                self.handle_window_event(event, window_id);
            },
            Event::AboutToWait => {
                self.handle_messages(elwt);
                // Check for any exited windows
                self.windows.retain(|_, (_, join_handle)| {
                    !join_handle.is_finished()
                });
                if self.should_exit.load(Ordering::Relaxed) {
                    elwt.exit();
                }
            },
            _ => ()
        }
    }

    fn handle_window_event(&mut self, event: WindowEvent, window_id: WindowId) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some((window, join_handle)) = self.windows.remove(&window_id) {
                    // Don't care if this errors, because we'll be removing the window anyway
                    window.handle_event(event).unwrap_or_default();
                    // TODO: Do we care about joining this? Possibly just drop the reference and move on
                    join_handle.join().unwrap();
                }
            },
            _ => {
                if let Some((window, _)) = self.windows.get(&window_id) {
                    window.handle_event(event).unwrap_or_else(|err| {
                        println!("Window {window_id:?} errored when handling event ({err:?}), dropping");
                        self.windows.remove(&window_id);
                    });
                }
            },
        }
    }

    fn handle_messages(&mut self, elwt: &EventLoopWindowTarget<()>) {
        while let Some((create_info, rctx)) = self.endpoint_create.try_pop().unwrap() {
            let (handle, join_handle) = Window::new(
                create_info,
                elwt,
                &self.vulkan_instance,
                &self.engine_metadata,
            );

            self.windows.insert(handle.id(), (handle.clone(), join_handle));
            rctx.reply(handle).unwrap();
        }
        while let Some((status_request, rctx)) = self.endpoint_status.try_pop().unwrap() {
            let window_handle = if let Some(id) = status_request.id {
                self.windows.get(&id).map(|(handle, _)| handle.clone())
            } else {
                None
            };

            rctx.reply(WindowStatus {
                window_handle,
                window_count: self.windows.len() as u32,
            }).unwrap();
        }
    }
}
