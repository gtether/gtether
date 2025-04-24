//! Client-side specific logic.
//!
//! This module contains anything that may be considered to be "client-side". This includes e.g.
//! windowing logic and input handling.
//!
//! At a bare minimum, this module contains a [Client] [Side] implementation that can be used to
//! run a simple main loop.
//!
//! Most of the more advanced client-side logic, such as windowing, is locked behind the `gui`
//! feature, as it involves windowing libraries that are platform specific. See the [`gui`] module
//! for more.

use smol::future;

use crate::util::tick_loop::TickLoopBuilder;
use crate::{Application, EngineStage, EngineState, Side};

#[cfg(feature = "gui")]
pub mod gui;

/// Simple barebones [Side] implementation.
///
/// Can be configured with a specific tick rate to maintain. Tick rate may fall below the given
/// value if ticks take too long, but never above.
///
/// # Examples
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use async_trait::async_trait;
/// use gtether::client::Client;
/// use gtether::Engine;
/// use gtether::Application;
///
/// use gtether::net::gns::{GnsClientDriver, GnsSubsystem};
///
/// struct MyApp {}
///
/// #[async_trait(?Send)]
/// impl Application<Client> for MyApp {
///     type NetworkingDriver = GnsClientDriver;
///
///     async fn init(&self, engine: &Arc<Engine<Self, Client>>) { todo!() }
///     fn tick(&self, engine: &Arc<Engine<Self, Client>>, delta: Duration) { todo!() }
/// }
///
/// let app = MyApp {};
///
/// // 60 ticks per second
/// let side = Client::builder()
///     .tick_rate(60)
///     .build();
///
/// let engine = Engine::builder()
///     .app(app)
///     .side(side)
///     .networking_driver(GnsSubsystem::get())
///     .build();
/// ```
pub struct Client {
    application_name: String,
    tick_rate: usize,
}

impl Client {
    /// Create a builder for [Client].
    ///
    /// This is the recommended way to create a new [Client].
    #[inline]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// The application name of the [Client].
    #[inline]
    pub fn application_name(&self) -> &str {
        &self.application_name
    }
}

impl Side for Client {
    fn start<A: Application<Self>>(&self, engine_state: EngineState<A, Self>) {
        TickLoopBuilder::new()
            .name(self.application_name.clone())
            .tick_rate(self.tick_rate)
            .init(|| {
                let engine = engine_state.engine();
                future::block_on(engine.app().init(&engine));
                engine_state.set_stage(EngineStage::Running).unwrap();
                Ok::<(), ()>(())
            })
            .tick(|_, delta| {
                let engine = engine_state.engine();
                engine.app().tick(&engine, delta);

                // Check for exit
                if engine_state.stage() == EngineStage::Stopping {
                    engine_state.set_stage(EngineStage::Stopped).unwrap();
                    false
                } else {
                    true
                }
            })
            .start().unwrap();
    }
}

/// Builder pattern for [Client].
///
/// # Examples
/// ```
/// use gtether::client::Client;
///
/// // 60 ticks per second
/// let side = Client::builder()
///     .application_name("MyApplication")
///     .tick_rate(60)
///     .build();
/// ```
pub struct ClientBuilder {
    application_name: Option<String>,
    tick_rate: Option<usize>,
}

impl ClientBuilder {
    /// Create a new ClientBuilder.
    ///
    /// It is recommended to use [Client::builder()] instead.
    #[inline]
    pub fn new() -> Self {
        Self {
            application_name: None,
            tick_rate: None,
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

    /// Set the tick rate.
    ///
    /// [Client] will attempt to drive the [Application] at this tick rate. If ticks take too
    /// long, some ticks may be skipped resulting in a lower tick rate, but the tick rate will never
    /// be higher than this number.
    ///
    /// Default is `60`.
    #[inline]
    pub fn tick_rate(mut self, tick_rate: usize) -> Self {
        self.tick_rate = Some(tick_rate);
        self
    }

    /// Build a new [Client].
    pub fn build(self) -> Client {
        let application_name = self.application_name
            .unwrap_or("client".to_owned());
        let tick_rate = self.tick_rate
            .unwrap_or(60);

        Client {
            application_name,
            tick_rate,
        }
    }

    /// Enable a GUI.
    ///
    /// This enables things like window management and input handling for this [Client]. Using this
    /// method will transform this [ClientBuilder] into a [ClientGuiBuilder], so any
    /// ClientBuilder-specific options should be set before calling this method.
    #[cfg(feature = "gui")]
    #[inline]
    pub fn enable_gui(self) -> gui::ClientGuiBuilder {
        gui::ClientGuiBuilder::new(self)
    }
}