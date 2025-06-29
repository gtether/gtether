// TODO: Separate out winit-specific type from the generic window interfaces,
//  and finalize documentation

use async_trait::async_trait;

use crate::app::driver::AppDriver;
use crate::gui::window::winit::{CreateWindowInfo, WindowHandle};

pub mod winit;

/// Manager for creating windows.
///
/// References to this trait can be used to create new windows for the engine to manage.
#[async_trait]
pub trait WindowManager {
    /// Create a new window.
    ///
    /// This method is asynchronous, and will yield a future that can be used to await the actual
    /// window creation.
    async fn create_window(&self, create_info: CreateWindowInfo) -> WindowHandle;
}

/// Trait describing an [AppDriver] that provides a [WindowManager].
pub trait AppDriverWindowManager: AppDriver {
    fn window_manager(&self) -> &dyn WindowManager;
}