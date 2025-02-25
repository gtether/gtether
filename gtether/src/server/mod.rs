//! Server-side specific logic.
//!
//! This module contains anything that may be considered to be "server-side".

use std::error::Error;
use std::fmt::{Display, Formatter};
use smol::future;

use crate::{Application, EngineStage, EngineState, Side};
use crate::net::NetworkingBuildError;
use crate::net::server::ServerNetworking;
use crate::util::tick_loop::TickLoopBuilder;

/// Server [Side] implementation.
///
/// # Examples
/// ```no_run
/// # use std::sync::Arc;
/// # use std::time::Duration;
/// # use async_trait::async_trait;
/// use gtether::Engine;
/// # use gtether::Application;
/// use gtether::net::gns::GnsSubsystem;
/// use gtether::net::server::ServerNetworking;
/// use gtether::server::Server;
/// # use gtether::server::ServerBuildError;
///
/// # struct MyApp {}
/// #
/// # #[async_trait(?Send)]
/// # impl Application<Server> for MyApp {
/// #     // Implement relevant functions...
/// #     async fn init(&self, engine: &Arc<Engine<Self, Server>>) {}
/// #     fn tick(&self, engine: &Arc<Engine<Self, Server>>, delta: Duration) {}
/// # }
/// #
/// # let app = MyApp {};
///
/// let networking = ServerNetworking::builder()
///     .raw_factory(GnsSubsystem::get())
///     .port(9001)
///     .build()?;
///
/// let side = Server::builder()
///     // 60 ticks per second
///     .tick_rate(60)
///     .networking(networking)
///     .build()?;
///
/// let engine = Engine::builder()
///     .app(app)
///     .side(side)
///     .build();
/// #
/// # Ok::<(), ServerBuildError>(())
/// ```
pub struct Server {
    tick_rate: usize,
    net: ServerNetworking,
}

impl Server {
    /// Create a builder for [Server].
    ///
    /// This is the recommended way to create a new [Server].
    #[inline]
    pub fn builder() -> ServerBuilder {
        ServerBuilder::new()
    }

    /// Reference to the server's [ServerNetworking] instance.
    #[inline]
    pub fn net(&self) -> &ServerNetworking {
        &self.net
    }
}

impl Side for Server {
    fn start<A: Application<Self>>(&self, engine_state: EngineState<A, Self>) {
        TickLoopBuilder::new()
            .name("server")
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

/// Errors that can occur while building a [Server].
#[derive(Debug)]
pub enum ServerBuildError {
    /// A required option was not specified.
    MissingOption { name: String },

    /// There was an error while initializing the [ServerNetworking] instance.
    Networking(NetworkingBuildError),
}

impl Display for ServerBuildError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingOption { name } => write!(f, "Missing required option: '{name}'"),
            Self::Networking(err) => Display::fmt(err, f),
        }
    }
}

impl Error for ServerBuildError {}

impl ServerBuildError {
    fn missing_option(name: impl Into<String>) -> Self {
        Self::MissingOption { name: name.into() }
    }
}

impl From<NetworkingBuildError> for ServerBuildError {
    #[inline]
    fn from(value: NetworkingBuildError) -> Self {
        Self::Networking(value)
    }
}

/// Builder pattern for [Server].
///
/// # Examples
///
/// Minimal example
/// ```no_run
/// use gtether::net::gns::GnsSubsystem;
/// use gtether::net::server::ServerNetworking;
/// use gtether::server::Server;
/// # use gtether::server::ServerBuildError;
///
/// let side = Server::builder()
///     .networking(ServerNetworking::builder()
///         .raw_factory(GnsSubsystem::get())
///         .port(9001)
///         .build()?)
///     .build()?;
/// #
/// # Ok::<(), ServerBuildError>(())
/// ```
///
/// Custom tick rate
/// ```no_run
/// use gtether::net::gns::GnsSubsystem;
/// use gtether::net::server::ServerNetworking;
/// use gtether::server::Server;
/// # use gtether::server::ServerBuildError;
///
/// let side = Server::builder()
///     .tick_rate(120)
///     .networking(ServerNetworking::builder()
///         .raw_factory(GnsSubsystem::get())
///         .port(9001)
///         .build()?)
///     .build()?;
/// #
/// # Ok::<(), ServerBuildError>(())
/// ```
pub struct ServerBuilder {
    tick_rate: Option<usize>,
    networking: Option<ServerNetworking>,
}

impl ServerBuilder {
    /// Create a new [ServerBuilder].
    ///
    /// It is recommended to use [Server::builder()] instead.
    #[inline]
    pub fn new() -> Self {
        Self {
            tick_rate: None,
            networking: None,
        }
    }

    /// Set the tick rate.
    ///
    /// [Server] will attempt to drive the [Application] at this tick rate. If ticks take too
    /// long, some ticks may be skipped resulting in a lower tick rate, but the tick rate will never
    /// be higher than this number.
    ///
    /// Default is `60`.
    #[inline]
    pub fn tick_rate(mut self, tick_rate: usize) -> Self {
        self.tick_rate = Some(tick_rate);
        self
    }

    /// Set the networking stack for server-side networking.
    ///
    /// This is a required option.
    #[inline]
    pub fn networking(mut self, networking: ServerNetworking) -> Self {
        self.networking = Some(networking);
        self
    }

    /// Build a new [Server].
    pub fn build(self) -> Result<Server, ServerBuildError> {
        let tick_rate = self.tick_rate
            .unwrap_or(60);
        let net = self.networking
            .ok_or(ServerBuildError::missing_option("networking"))?;

        Ok(Server {
            tick_rate,
            net,
        })
    }
}