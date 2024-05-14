use std::thread;

#[cfg(feature = "graphics")]
pub mod render;

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
        EngineMetadata {
            application_name: None,
            _ne: NonExhaustive(()),
        }
    }
}

/// Represents an instance of the gTether Engine.
///
/// All engine usage stems from this structure, and it is required to construct one first.
pub struct Engine {
    metadata: EngineMetadata,

    #[cfg(feature = "graphics")]
    window_manager: render::window::WindowManagerClient,
    #[cfg(feature = "graphics")]
    window_manager_join_handle: thread::JoinHandle<()>,
}

impl Engine {
    /// Construct a new gTether Engine.
    ///
    /// # Examples
    ///
    /// ```
    /// use gtether::{Engine, EngineMetadata};
    ///
    /// let engine = Engine::new(
    ///     EngineMetadata {
    ///         application_name: Some(String::from("My Application")),
    ///         ..Default::default()
    ///     }
    /// );
    /// ```
    pub fn new(
        metadata: EngineMetadata,
    ) -> Self {
        #[cfg(feature = "graphics")]
        let (window_manager, window_manager_join_handle)
            = render::window::WindowManager::start(metadata.clone());

        Engine {
            metadata,
            #[cfg(feature = "graphics")]
            window_manager,
            #[cfg(feature = "graphics")]
            window_manager_join_handle,
        }
    }

    #[inline]
    pub fn metadata(&self) -> &EngineMetadata { &self.metadata }

    #[cfg(feature = "graphics")]
    #[inline]
    pub fn window_manager(&self) -> &render::window::WindowManagerClient { &self.window_manager }

    fn join(self) -> thread::Result<()> {
        #[cfg(feature = "graphics")]
        self.window_manager_join_handle.join()?;

        Ok(())
    }

    pub fn exit(self) {
        #[cfg(feature = "graphics")]
        self.window_manager.request_exit();

        self.join().unwrap();
    }
}

/// A helper type for non-exhaustive structs.
///
/// This allows structures to be created via a constructor function or by update syntax in
/// combination with `Default::default()`. Copied from and inspired by Vulkano's NonExhaustive.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NonExhaustive(pub(crate) ());
