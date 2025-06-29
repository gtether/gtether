/// Application driver traits.
///
/// Contains the specification for [AppDriver]. See that trait for more.
///
/// Also contains the minimal implementation of AppDriver; [MinimalAppDriver].

use std::thread;
use smol::future;

use crate::app::Application;
use crate::{EngineStage, EngineStageChangedEvent, EngineState};
use crate::event::Event;

/// Application logic driver.
///
/// An AppDriver represents how an [Application] is driven by the [Engine](crate::Engine). For
/// example, a [MinimalAppDriver] may contain the simple logic of driving [EngineStage] transitions,
/// whereas a [WinitDriver][winit] can contain the full logic of driving a window-based event loop.
///
/// The engine provides several useful [AppDriver] implementations, including the following:
///  * [MinimalAppDriver] - Minimal logic to drive an [Application]. No GUI.
///  * [WinitDriver][winit] - Full GUI support using a windowing event loop provided by
///    [Winit](https://docs.rs/winit/latest/winit/).
///
/// [winit]: crate::gui::window::winit::WinitDriver
///
/// # Implementation
///
/// If the engine provided implementations don't fit your use-case, you can also implement your own.
/// Certain patterns should be followed when implementing your own AppDriver.
///
/// ## Application lifecycle
///
/// Any implementation of AppDriver should adhere to the following lifecycle contraints:
///  * [Application::init()] should be executed once per engine [initialization](EngineStage::Init).
///  * [Application::resume()] should be executed once per [resume cycle](EngineStage::Resuming),
///    where the engine transitions from [Suspended](EngineStage::Suspended) to
///    [Running](EngineStage::Running).
///  * [Application::suspend()] should be executed once per [suspend cycle](EngineStage::Suspending),
///    where the engine transitions from [Running](EngineStage::Running) to
///    [Suspended](EngineStage::Suspended).
///  * [Application::terminate()] should be executed once per engine
///    [termination](EngineStage::Stopping), at the very end of an application's lifecycle.
///
/// The implementation is also responsible for transitioning engine [stages](EngineStage) as
/// appropriate, using [EngineState::set_stage()]. For example, if the [Engine](crate::Engine)
/// begins terminating, it will be put into [EngineStage::Stopping]. It is up to the AppDriver to
/// recognize this, perform the necessary actions (such as executing [Application::terminate()]),
/// and then set the [EngineStage] to [Stopped](EngineStage::Stopped).
pub trait AppDriver: Send + Sync + 'static {
    fn run_main_loop<A: Application + 'static>(&self, engine_state: EngineState<A>);
}

/// Minimal implementation of [AppDriver].
///
/// This implementation performs the minimal logic required to execute an engine lifecycle. It will
/// call [Application::init()] when the engine starts, and [Application::terminate()] when the
/// engine stops, as well as transition the [EngineStage] as needed. It does *not* support engine
/// [suspension stages](EngineStage::Suspended), and as such will not execute the
/// [Application::suspend()] or [Application::resume()] hooks.
#[derive(Default)]
pub struct MinimalAppDriver {}

impl MinimalAppDriver {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }
}

impl AppDriver for MinimalAppDriver {
    fn run_main_loop<A: Application>(&self, engine_state: EngineState<A>) {
        // Init
        {
            future::block_on(engine_state.app().init(&*engine_state));
            engine_state.set_stage(EngineStage::Running).unwrap();
        }

        let current_thread = thread::current();
        engine_state.event_bus().register(move |event: &mut Event<EngineStageChangedEvent>| {
            if event.current() == EngineStage::Stopping {
                current_thread.unpark();
            }
        }).unwrap();
        while engine_state.stage() != EngineStage::Stopping {
            thread::park();
        }

        // Terminate
        {
            future::block_on(engine_state.app().terminate(&*engine_state));
            engine_state.set_stage(EngineStage::Stopped).unwrap();
        }
    }
}