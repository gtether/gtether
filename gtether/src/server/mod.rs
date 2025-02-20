//! Server-side specific logic.
//!
//! This module contains anything that may be considered to be "server-side".

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use smol::future;

use crate::{Application, EngineStage, EngineState, Side};
use crate::net::gns::GnsSubsystem;
use crate::net::server::{RawServerFactory, ServerNetworkingError, ServerNetworking};
use crate::util::tick_loop::TickLoopBuilder;

/// Server [Side] implementation.
///
/// # Examples
/// ```
/// # use std::sync::Arc;
/// # use std::time::Duration;
/// # use async_trait::async_trait;
/// use gtether::Engine;
/// # use gtether::Application;
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
/// let side = Server::builder()
///     // 60 ticks per second
///     .tick_rate(60)
///     .port(9001)
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
    NetworkingError(ServerNetworkingError),
}

impl Display for ServerBuildError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingOption { name } => write!(f, "Missing required option: '{name}'"),
            Self::NetworkingError(err) => Display::fmt(err, f),
        }
    }
}

impl Error for ServerBuildError {}

impl ServerBuildError {
    fn missing_option(name: impl Into<String>) -> Self {
        Self::MissingOption { name: name.into() }
    }
}

impl From<ServerNetworkingError> for ServerBuildError {
    #[inline]
    fn from(value: ServerNetworkingError) -> Self {
        Self::NetworkingError(value)
    }
}

/// Builder pattern for [Server].
///
/// # Examples
///
/// Minimal example
/// ```
/// use gtether::server::Server;
/// # use gtether::server::ServerBuildError;
///
/// let side = Server::builder()
///     .port(9001)
///     .build()?;
/// #
/// # Ok::<(), ServerBuildError>(())
/// ```
///
/// Custom tick rate
/// ```
/// use gtether::server::Server;
/// # use gtether::server::ServerBuildError;
///
/// let side = Server::builder()
///     .tick_rate(120)
///     .port(9001)
///     .build()?;
/// #
/// # Ok::<(), ServerBuildError>(())
/// ```
///
/// Use a different network interface
/// ```
/// use std::net::Ipv4Addr;
/// use gtether::server::Server;
/// # use gtether::server::ServerBuildError;
///
/// let side = Server::builder()
///     .ip_addr(Ipv4Addr::new(192, 168, 0, 42))
///     .port(9001)
///     .build()?;
/// #
/// # Ok::<(), ServerBuildError>(())
/// ```
pub struct ServerBuilder {
    tick_rate: Option<usize>,
    ip_addr: Option<IpAddr>,
    port: Option<u16>,
    raw_server_factory: Option<Box<dyn RawServerFactory>>,
}

impl ServerBuilder {
    /// Create a new [ServerBuilder].
    ///
    /// It is recommended to use [Server::builder()] instead.
    #[inline]
    pub fn new() -> Self {
        Self {
            tick_rate: None,
            ip_addr: None,
            port: None,
            raw_server_factory: None,
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

    /// Set the IP address that this server listens on.
    ///
    /// Default is [Ipv4Addr::LOCALHOST].
    #[inline]
    pub fn ip_addr(mut self, ip_addr: impl Into<IpAddr>) -> Self {
        self.ip_addr = Some(ip_addr.into());
        self
    }

    /// Set the port that this server listens on.
    ///
    /// This option is required.
    #[inline]
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Set the socket address that this server listens on.
    ///
    /// This is effectively the same as:
    /// ```no_run
    /// # use std::net::{Ipv4Addr, SocketAddr};
    /// # use gtether::server::Server;
    /// # let socket = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9001);
    /// Server::builder()
    ///     .ip_addr(socket.ip())
    ///     .port(socket.port())
    /// ```
    #[inline]
    pub fn socket_addr(mut self, socket_addr: impl Into<SocketAddr>) -> Self {
        let socket_addr = socket_addr.into();
        self.ip_addr = Some(socket_addr.ip());
        self.port = Some(socket_addr.port());
        self
    }

    /// Set the [RawServerFactory] for networking.
    ///
    /// This determines the implementation used for server-side networking.
    ///
    /// Default uses [GNS](crate::net::gns).
    #[inline]
    pub fn raw_server_factory(mut self, raw_server_factory: impl RawServerFactory) -> Self {
        self.raw_server_factory = Some(Box::new(raw_server_factory));
        self
    }

    /// Build a new [Server].
    pub fn build(self) -> Result<Server, ServerBuildError> {
        let tick_rate = self.tick_rate
            .unwrap_or(60);

        let ip_addr = self.ip_addr
            .unwrap_or(Ipv4Addr::LOCALHOST.into());
        let port = self.port.clone()
            .ok_or(ServerBuildError::missing_option("port"))?;
        let socket_addr = SocketAddr::new(ip_addr, port);
        let raw_server_factory = self.raw_server_factory
            .unwrap_or_else(|| Box::new(GnsSubsystem::get()));
        let net = ServerNetworking::new(
            raw_server_factory,
            socket_addr,
        )?;

        Ok(Server {
            tick_rate,
            net,
        })
    }
}