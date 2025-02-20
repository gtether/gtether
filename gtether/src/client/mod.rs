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
use std::error::Error;
use std::fmt::{Display, Formatter};

use crate::net::client::{ClientNetworking, ClientNetworkingError, RawClientFactory};
use crate::net::gns::GnsSubsystem;
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
/// # use std::sync::Arc;
/// # use std::time::Duration;
/// # use async_trait::async_trait;
/// # use gtether::client::ClientBuildError;
/// use gtether::client::Client;
/// use gtether::Engine;
/// # use gtether::Application;
/// #
/// # struct MyApp {}
/// #
/// # #[async_trait(?Send)]
/// # impl Application<Client> for MyApp {
/// #     // Implement relevant functions...
/// #     async fn init(&self, engine: &Arc<Engine<Self, Client>>) {}
/// #     fn tick(&self, engine: &Arc<Engine<Self, Client>>, delta: Duration) {}
/// # }
/// #
/// # let app = MyApp {};
///
/// // 60 ticks per second
/// let side = Client::builder()
///     .tick_rate(60)
///     .build()?;
///
/// let engine = Engine::builder()
///     .app(app)
///     .side(side)
///     .build();
/// #
/// # Ok::<(), ClientBuildError>(())
/// ```
pub struct Client {
    application_name: String,
    tick_rate: usize,
    net: ClientNetworking,
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

    /// Reference to the client's [ClientNetworking] instance.
    #[inline]
    pub fn net(&self) -> &ClientNetworking {
        &self.net
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

/// Errors that can occur while building a [Client].
#[derive(Debug)]
pub enum ClientBuildError {
    /// There was an error while initializing the [ClientNetworking] instance.
    NetworkingError(ClientNetworkingError),
}

impl Display for ClientBuildError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NetworkingError(err) => Display::fmt(err, f),
        }
    }
}

impl Error for ClientBuildError {}

impl From<ClientNetworkingError> for ClientBuildError {
    #[inline]
    fn from(value: ClientNetworkingError) -> Self {
        Self::NetworkingError(value)
    }
}

/// Builder pattern for [Client].
///
/// # Examples
/// ```
/// use std::sync::Arc;
/// # use std::time::Duration;
/// use async_trait::async_trait;
/// use gtether::client::Client;
/// # use gtether::client::ClientBuildError;
/// use gtether::{Application, Engine};
/// #
/// # struct MyApp {}
///
/// #[async_trait(?Send)]
/// impl Application<Client> for MyApp {
///     // Implement relevant functions...
/// #     async fn init(&self, engine: &Arc<Engine<Self, Client>>) {}
/// #     fn tick(&self, engine: &Arc<Engine<Self, Client>>, delta: Duration) {}
/// }
///
/// let app = MyApp {};
///
/// // 60 ticks per second
/// let side = Client::builder()
///     .application_name("MyApplication")
///     .tick_rate(60)
///     .build()?;
///
/// let engine = Engine::builder()
///     .app(app)
///     .side(side)
///     .build();
/// #
/// # Ok::<(), ClientBuildError>(())
/// ```
pub struct ClientBuilder {
    application_name: Option<String>,
    tick_rate: Option<usize>,
    raw_client_factory: Option<Box<dyn RawClientFactory>>,
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
            raw_client_factory: None,
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

    /// Set the [RawClientFactory] for networking.
    ///
    /// This determines the implementation used for client-side networking.
    ///
    /// Default uses [GNS](crate::net::gns).
    #[inline]
    pub fn raw_client_factory(mut self, raw_client_factory: impl RawClientFactory) -> Self {
        self.raw_client_factory = Some(Box::new(raw_client_factory));
        self
    }

    /// Build a new [Client].
    pub fn build(self) -> Result<Client, ClientBuildError> {
        let application_name = self.application_name
            .unwrap_or("client".to_owned());
        let tick_rate = self.tick_rate
            .unwrap_or(60);

        let raw_client_factory = self.raw_client_factory
            .unwrap_or_else(|| Box::new(GnsSubsystem::get()));
        let net = ClientNetworking::new(raw_client_factory);

        Ok(Client {
            application_name,
            tick_rate,
            net,
        })
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