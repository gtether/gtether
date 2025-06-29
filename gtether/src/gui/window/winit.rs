use async_trait::async_trait;
use crossbeam::queue::SegQueue;
use educe::Educe;
use parking_lot::{Mutex, RwLock};
use smol::future;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::{fmt, thread};
use tracing::{event, info, warn, Level};
use vulkano::instance::InstanceExtensions;
use vulkano::swapchain::Surface;
use winit::application::ApplicationHandler;
use winit::error::ExternalError;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
pub use winit::window::WindowAttributes;
use winit::window::{CursorGrabMode, Window as WinitWindow, WindowId};

use crate::app::driver::AppDriver;
use crate::app::Application;
use crate::gui::input::{InputEvent, InputState};
use crate::event::Event;
use crate::render::{AppDriverGraphicsVulkan, Renderer};
use crate::render::{EngineDevice, Instance, RenderTarget};
use crate::{Engine, EngineStage, EngineStageChangedEvent, EngineState, NonExhaustive};
use crate::gui::window::{AppDriverWindowManager, WindowManager};

#[derive(Debug)]
struct WindowRenderTarget {
    winit_window: Arc<WinitWindow>,
    surface: Arc<Surface>,
}

impl WindowRenderTarget {
    fn new(winit_window: Arc<WinitWindow>, instance: Arc<Instance>) -> Arc<dyn RenderTarget> {
        let surface = Surface::from_window(
            instance.vk_instance().clone(),
            winit_window.clone(),
        ).unwrap();

        Arc::new(Self {
            winit_window,
            surface,
        })
    }
}

impl RenderTarget for WindowRenderTarget {
    #[inline]
    fn surface(&self) -> &Arc<Surface> { &self.surface }

    #[inline]
    fn extent(&self) -> glm::TVec2<u32> {
        let physical_size = self.winit_window.inner_size();
        glm::vec2(physical_size.width, physical_size.height)
    }

    #[inline]
    fn scale_factor(&self) -> f64 { self.winit_window.scale_factor() }
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

#[derive(Debug)]
struct WindowModifyRequest {
    cursor_visible: Option<bool>,
}

impl Default for WindowModifyRequest {
    fn default() -> Self {
        Self {
            cursor_visible: None,
        }
    }
}

#[derive(Debug)]
enum WindowEventRequest {
    Window(WindowEvent),
    Device(DeviceEvent),
}

#[derive(Debug)]
enum WindowRequest {
    Modify(WindowModifyRequest),
    Event(WindowEventRequest),
}

/// Handle for a specific window.
///
/// Provides access to window information and manipulation methods.
#[derive(Educe, Clone)]
#[educe(Debug)]
pub struct WindowHandle {
    id: WindowId,
    #[educe(Debug(ignore))]
    renderer: Arc<Renderer>,
    #[educe(Debug(ignore))]
    input: Arc<InputState>,
    requests: Arc<SegQueue<WindowRequest>>,
}

impl WindowHandle {
    /// The [winit::window::WindowId] for this window.
    #[inline]
    pub fn id(&self) -> WindowId { self.id }

    /// A reference to this window's [Renderer].
    #[inline]
    pub fn renderer(&self) -> &Arc<Renderer> { &self.renderer }

    /// The current [InputState] for this window.
    #[inline]
    pub fn input_state(&self) -> &Arc<InputState> { &self.input }

    pub fn set_cursor_visible(&self, visible: bool) {
        let modify_request = WindowModifyRequest {
            cursor_visible: Some(visible),
            ..Default::default()
        };

        self.requests.push(WindowRequest::Modify(modify_request));
    }

    /// Requests this window to close.
    ///
    /// Sends a request which may be honored by the window when available. Does not block.
    ///
    /// # Panics
    ///
    /// - Panics if the window's main thread has stopped or is otherwise unresponsive.
    pub fn request_close(&self) {
        let event_request = WindowEventRequest::Window(WindowEvent::CloseRequested);
        self.requests.push(WindowRequest::Event(event_request));
    }

    fn handle_event(&self, event_request: WindowEventRequest) {
        self.requests.push(WindowRequest::Event(event_request));
    }
}

struct Window {
    winit_window: Arc<WinitWindow>,
    renderer: Arc<Renderer>,
    input: Arc<InputState>,
    cursor_visible: bool,
    requests: Arc<SegQueue<WindowRequest>>,
}

impl Window {
    fn new(
        attributes: WindowAttributes,
        event_loop: &ActiveEventLoop,
        event_loop_proxy: EventLoopProxy<EngineEvent>,
        instance: &Arc<Instance>,
        application_name: String,
    ) -> (WindowHandle, thread::JoinHandle<()>) {
        let mut attributes = attributes;
        if &attributes.title == "winit window" {
            attributes.title = application_name;
        }
        let title = attributes.title.clone();

        let winit_window = Arc::new(event_loop.create_window(attributes).unwrap());

        let window_id = winit_window.id();

        let target = WindowRenderTarget::new(
            winit_window.clone(),
            instance.clone(),
        );
        let device = Arc::new(EngineDevice::for_surface(
            instance.clone(),
            target.surface().clone(),
        ));
        let renderer = Arc::new(Renderer::new(
            target,
            device,
        ).unwrap());

        let input = Arc::new(InputState::default());

        let requests = Arc::new(SegQueue::new());

        let window_handle = WindowHandle {
            id: window_id,
            renderer: renderer.clone(),
            input: input.clone(),
            requests: requests.clone(),
        };

        let join_handle = thread::Builder::new().name(title).spawn(move || {
            let mut window = Self {
                winit_window,
                renderer,
                input,
                cursor_visible: true,
                requests,
            };

            loop {
                if !window.tick().is_ok() {
                    break;
                }
            }
            // TODO: somehow check if the window process exited before this
            let _ = event_loop_proxy.send_event(EngineEvent::ClosingWindow(window_id));
        }).unwrap();

        (window_handle, join_handle)
    }

    fn set_cursor_visible(&mut self, cursor_visible: bool) {
        self.winit_window.set_cursor_visible(cursor_visible);
        let result = if cursor_visible {
            self.winit_window.set_cursor_grab(CursorGrabMode::None)
        } else {
            self.winit_window.set_cursor_grab(CursorGrabMode::Confined).or_else(|e| {
                if matches!(e, ExternalError::NotSupported(..)) {
                    self.winit_window.set_cursor_grab(CursorGrabMode::Locked)
                } else {
                    Err(e)
                }
            })
        };
        if let Err(e) = result {
            event!(Level::WARN, "Failed to set cursor grab mode: {e}")
        }
    }

    fn tick(&mut self) -> Result<(), ()> {
        let mut status = Ok(());

        self.renderer.render();

        while let Some(request) = self.requests.pop() {
            match request {
                WindowRequest::Modify(modify_request) => {
                    if let Some(visible) = modify_request.cursor_visible {
                        self.cursor_visible = visible;
                        self.set_cursor_visible(visible);
                    }
                },
                WindowRequest::Event(event_request) => {
                    let window_id = self.winit_window.id();
                    match event_request {
                        WindowEventRequest::Window(event) => match event {
                            WindowEvent::CloseRequested => {
                                event!(Level::INFO, "Window {window_id:?} has received the signal to close");
                                status = Err(());
                            },
                            WindowEvent::Resized(_) => {
                                self.renderer.mark_stale();
                            },
                            WindowEvent::Focused(focused) => {
                                if focused {
                                    self.set_cursor_visible(self.cursor_visible);
                                } else {
                                    self.set_cursor_visible(true);
                                }
                            },
                            WindowEvent::Destroyed => {
                                return Err(());
                            },
                            event => {
                                if let Some(input_event) = InputEvent::from_window_event(event) {
                                    self.input.handle_event(input_event);
                                }
                            }
                        },
                        WindowEventRequest::Device(event) => {
                            if let Some(input_event) = InputEvent::from_device_event(event) {
                                self.input.handle_event(input_event);
                            }
                        }
                    }
                }
            }
        }

        status
    }
}

type CreateWindowFuturePair = Arc<Mutex<(Option<WindowHandle>, Option<Waker>)>>;

/// Future representing the creation of a window.
///
/// Yields a [WindowHandle] when the window has been created.
pub struct CreateWindowFuture {
    pair: CreateWindowFuturePair,
}

impl CreateWindowFuture {
    fn new() -> Self {
        Self {
            pair: Arc::new(Mutex::new((None, None))),
        }
    }
}

impl Future for CreateWindowFuture {
    type Output = WindowHandle;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut lock = self.pair.lock();
        let (ref handle, ref mut waker) = *lock;
        if let Some(handle) = handle {
            Poll::Ready(handle.clone())
        } else {
            *waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecyclePhase {
    Init,
    Resume,
    Suspend,
    Terminate,
}

impl Display for LifecyclePhase {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Init => write!(f, "init"),
            Self::Resume => write!(f, "resume"),
            Self::Suspend => write!(f, "suspend"),
            Self::Terminate => write!(f, "terminate"),
        }
    }
}

impl LifecyclePhase {
    fn exec_engine_phase<A: Application>(&self, engine: &Arc<Engine<A>>) {
        match self {
            Self::Init => future::block_on(engine.app().init(engine)),
            Self::Resume => future::block_on(engine.app().resume(engine)),
            Self::Suspend => future::block_on(engine.app().suspend(engine)),
            Self::Terminate => future::block_on(engine.app().terminate(engine)),
        }
    }

    fn target_stage(&self) -> EngineStage {
        match self {
            Self::Init => EngineStage::Running,
            Self::Resume => EngineStage::Running,
            Self::Suspend => EngineStage::Suspended,
            Self::Terminate => EngineStage::Stopped,
        }
    }

    fn spawn<A: Application>(
        &self,
        engine: Arc<Engine<A>>,
        event_loop_proxy: EventLoopProxy<EngineEvent>,
    ) -> thread::JoinHandle<()> {
        let spawn_self = *self;
        thread::Builder::new()
            .name(format!("engine-task-{}", self.to_string()))
            .spawn(move || {
                spawn_self.exec_engine_phase(&engine);
                event_loop_proxy.send_event(EngineEvent::FinishPhase(spawn_self)).unwrap();
            })
            .unwrap()
    }
}

#[derive(Debug)]
enum EngineEvent {
    CreateWindow{
        create_info: CreateWindowInfo,
        pair: CreateWindowFuturePair,
    },
    ClosingWindow(WindowId),
    FinishPhase(LifecyclePhase),
    Stopping,
}

struct WindowEntry {
    window_handle: WindowHandle,
    join_handle: thread::JoinHandle<()>,
    exit_on_close: bool,
}

struct WindowOrchestrator<A: Application> {
    engine_state: EngineState<A>,
    event_loop_proxy: EventLoopProxy<EngineEvent>,
    application_name: String,
    render_instance: Arc<Instance>,
    windows: HashMap<WindowId, WindowEntry>,
    background_handle: Option<(LifecyclePhase, thread::JoinHandle<()>)>,
}

impl<A: Application> WindowOrchestrator<A> {
    fn new(
        engine_state: EngineState<A>,
        event_loop_proxy: EventLoopProxy<EngineEvent>,
        application_name: impl Into<String>,
        render_instance: Arc<Instance>,
    ) -> Self {
        Self {
            engine_state,
            event_loop_proxy,
            application_name: application_name.into(),
            render_instance,
            windows: HashMap::new(),
            background_handle: None,
        }
    }

    fn create_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        create_info: CreateWindowInfo,
    ) -> WindowHandle {
        let (handle, join_handle) = Window::new(
            create_info.attributes,
            event_loop,
            self.event_loop_proxy.clone(),
            &self.render_instance,
            self.application_name.clone(),
        );
        self.windows.insert(handle.id(), WindowEntry {
            window_handle: handle.clone(),
            join_handle,
            exit_on_close: create_info.exit_on_close,
        });
        handle
    }
}

impl<A: Application> ApplicationHandler<EngineEvent> for WindowOrchestrator<A> {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        match self.engine_state.stage() {
            EngineStage::Init => {
                if let Some((phase, ref _handle)) = self.background_handle {
                    match phase {
                        // Already initializing; this is a repeat 'resumed' event
                        LifecyclePhase::Init => return,
                        phase => panic!("Invalid background task phase '{phase}' during Resume event"),
                    }
                } else {
                    self.background_handle = Some((
                        LifecyclePhase::Init,
                        LifecyclePhase::Init.spawn(
                            self.engine_state.deref().clone(),
                            self.event_loop_proxy.clone(),
                        ),
                    ))
                }
            },
            EngineStage::Suspended => {
                self.engine_state.set_stage(EngineStage::Resuming).unwrap();
                if let Some((phase, ref _handle)) = self.background_handle {
                    match phase {
                        // Already resuming; this is a repeat 'resumed' event
                        LifecyclePhase::Resume => return,
                        phase => panic!("Invalid background task phase '{phase}' during Resume event"),
                    }
                } else {
                    self.background_handle = Some((
                        LifecyclePhase::Resume,
                        LifecyclePhase::Resume.spawn(
                            self.engine_state.deref().clone(),
                            self.event_loop_proxy.clone(),
                        ),
                    ))
                }
            },
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: EngineEvent) {
        match event {
            EngineEvent::CreateWindow{ create_info, pair } => {
                let window = self.create_window(event_loop, create_info);
                let mut lock = pair.lock();
                let (ref mut handle, ref mut waker) = *lock;
                *handle = Some(window);
                if let Some(waker) = waker.take() {
                    waker.wake();
                }
            },
            EngineEvent::ClosingWindow(window_id) => {
                if let Some(entry) = self.windows.remove(&window_id) {
                    match entry.join_handle.join() {
                        Ok(_) => info!(?window_id, "Window closed"),
                        Err(_) => warn!(?window_id, "Window errored while closing"),
                    }
                    if entry.exit_on_close {
                        if let Err(error) = self.engine_state.stop() {
                            warn!(?window_id, ?error, "Failed to stop engine after primary window closed");
                        }
                    }
                }
            },
            EngineEvent::FinishPhase(phase) => {
                if let Some((handle_phase, handle)) = self.background_handle.take() {
                    debug_assert_eq!(handle_phase, phase);
                    handle.join().unwrap();
                }
                if phase == LifecyclePhase::Terminate {
                    event_loop.exit();
                }
                let target = phase.target_stage();
                if let Err(error) = self.engine_state.set_stage(target) {
                    warn!(?target, ?error, "Could not advance engine stage to target stage");
                    if phase == LifecyclePhase::Terminate {
                        // Crash out anyway, as this is non-recoverable
                        Err::<(), _>(error).unwrap();
                    }
                }
            }
            EngineEvent::Stopping => {
                if self.background_handle.is_some() {
                    panic!("EngineEvent::Stopping should not be sent twice; existing handle={:?}", self.background_handle);
                } else {
                    self.background_handle = Some((
                        LifecyclePhase::Terminate,
                        LifecyclePhase::Terminate.spawn(
                            self.engine_state.deref().clone(),
                            self.event_loop_proxy.clone(),
                        ),
                    ))
                }
            }
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        if let Some(entry) = self.windows.get(&window_id) {
            entry.window_handle.handle_event(WindowEventRequest::Window(event));
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _device_id: DeviceId, event: DeviceEvent) {
        for entry in self.windows.values() {
            entry.window_handle.handle_event(WindowEventRequest::Device(event.clone()))
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // TODO: Suspend event may be sent at basically anytime, regardless of Engine stage, so need
        //  to handle when the Engine is in the middle of another stage transition.
        if let Some((phase, ref _handle)) = self.background_handle {
            match phase {
                // Already suspending; this is a repeat 'suspend' event
                LifecyclePhase::Suspend => return,
                phase => panic!("Invalid background task phase '{phase}' during Suspend event"),
            }
        } else {
            self.background_handle = Some((
                LifecyclePhase::Suspend,
                LifecyclePhase::Suspend.spawn(
                    self.engine_state.deref().clone(),
                    self.event_loop_proxy.clone(),
                ),
            ))
        }
    }
}

enum WinitDriverState {
    Created {
        render_extensions: InstanceExtensions,
    },
    Started {
        render_instance: Arc<Instance>,
        event_loop_proxy: EventLoopProxy<EngineEvent>,
    },
}

/// [AppDriver] implementation with windowing support, using Winit.
///
/// A WinitDriver provides windowing support, and therefore should be the preferred [AppDriver]
/// implementation to use for most games on the player side. Windowing support includes providing a
/// surface/s to render on using the [render module](crate::render), as well as input handling
/// capabilities.
///
/// Windowing support *is* platform-specific, however, and so WinitDriver is only supported on some
/// platforms. Currently, that list is as follows:
///  * x86_64 windows
///    * aarch64 is likely to work fine, but isn't tested
///  * x86_64 linux
///    * aarch64 is likely to work fine, but isn't tested
pub struct WinitDriver {
    application_name: String,
    state: RwLock<WinitDriverState>,
}

impl WinitDriver {
    /// Create a [WinitDriverBuilder].
    ///
    /// This is the recommended way to create a [WinitDriver].
    #[inline]
    pub fn builder() -> WinitDriverBuilder {
        WinitDriverBuilder::new()
    }
}

impl AppDriver for WinitDriver {
    fn run_main_loop<A: Application>(&self, engine_state: EngineState<A>) {
        let (event_loop, mut handler) = {
            let mut state = self.state.write();
            let (new_state, event_loop, handler) = match &*state {
                WinitDriverState::Created {
                    render_extensions,
                } => {
                    let event_loop = EventLoop::<EngineEvent>::with_user_event()
                        .build().unwrap();
                    event_loop.set_control_flow(ControlFlow::Wait);

                    let render_extensions = render_extensions.clone()
                        | Surface::required_extensions(&event_loop);
                    let render_instance = Arc::new(Instance::new(
                        Some(self.application_name.clone()),
                        render_extensions,
                    ));

                    let event_loop_proxy = event_loop.create_proxy();

                    let handler_event_loop_proxy = event_loop_proxy.clone();
                    engine_state.event_bus().register(move |event: &mut Event<EngineStageChangedEvent>| {
                        if event.current() == EngineStage::Stopping {
                            // If event loop is invalid, this will effectively be a noop
                            let _ = handler_event_loop_proxy.send_event(EngineEvent::Stopping);
                        }
                    }).unwrap();

                    let handler = WindowOrchestrator::new(
                        engine_state,
                        event_loop_proxy.clone(),
                        self.application_name.clone(),
                        render_instance.clone(),
                    );

                    let new_state = WinitDriverState::Started {
                        render_instance,
                        event_loop_proxy,
                    };

                    (new_state, event_loop, handler)
                },
                _ => panic!("WinitDriver can only be started once"),
            };
            *state = new_state;
            (event_loop, handler)
        };

        event_loop.run_app(&mut handler)
            .expect("Failed to run winit loop");
    }
}

impl AppDriverGraphicsVulkan for WinitDriver {
    fn render_instance(&self) -> Arc<Instance> {
        match &*self.state.read() {
            WinitDriverState::Started { render_instance, .. } => render_instance.clone(),
            _ => panic!("WinitDriver main loop not started yet"),
        }
    }
}

#[async_trait]
impl WindowManager for WinitDriver {
    async fn create_window(&self, create_info: CreateWindowInfo) -> WindowHandle {
        match &*self.state.read() {
            WinitDriverState::Started { event_loop_proxy, .. } => {
                let fut = CreateWindowFuture::new();
                event_loop_proxy.send_event(EngineEvent::CreateWindow {
                    create_info,
                    pair: fut.pair.clone(),
                }).unwrap();
                fut.await
            },
            _ => panic!("WinitDriver not started yet"),
        }
    }
}

impl AppDriverWindowManager for WinitDriver {
    #[inline]
    fn window_manager(&self) -> &dyn WindowManager {
        self
    }
}

/// Builder pattern for [WinitDriver].
///
/// # Examples
/// ```no_run
/// use gtether::gui::window::winit::WinitDriver;
///
/// let driver = WinitDriver::builder()
///     .application_name("StartingApplication")
///     .build();
/// ```
pub struct WinitDriverBuilder {
    application_name: Option<String>,
    render_extensions: InstanceExtensions,
}

impl WinitDriverBuilder {
    /// Create a new builder.
    ///
    /// It is recommended to use [WinitDriver::builder()] instead.
    #[inline]
    pub fn new() -> Self {
        Self {
            application_name: None,
            render_extensions: InstanceExtensions::default(),
        }
    }

    /// Set the application name.
    ///
    /// Default is `"client"`.
    #[inline]
    pub fn application_name(mut self, name: impl Into<String>) -> Self {
        self.application_name = Some(name.into());
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

    /// Build a new [WinitDriver].
    #[inline]
    pub fn build(self) -> WinitDriver {
        let application_name = self.application_name
            .unwrap_or("client".to_owned());
        let render_extensions = self.render_extensions;

        WinitDriver {
            application_name,
            state: RwLock::new(WinitDriverState::Created {
                render_extensions,
            }),
        }
    }
}