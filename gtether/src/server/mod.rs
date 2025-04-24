//! Server-side specific logic.
//!
//! This module contains anything that may be considered to be "server-side".

use smol::future;

use crate::{Application, EngineStage, EngineState, Side};
use crate::util::tick_loop::TickLoopBuilder;

/// Server [Side] implementation.
///
/// # Examples
/// ```no_run
/// use std::net::{Ipv4Addr, SocketAddr};
/// use std::sync::Arc;
/// use std::time::Duration;
/// use async_trait::async_trait;
/// use gtether::Engine;
/// use gtether::Application;
/// use gtether::net::Networking;
/// use gtether::net::gns::{GnsServerDriver, GnsSubsystem};
/// use gtether::server::Server;
///
/// struct MyApp {}
///
/// #[async_trait(?Send)]
/// impl Application<Server> for MyApp {
///     type NetworkingDriver = GnsServerDriver;
///
///     async fn init(&self, engine: &Arc<Engine<Self, Server>>) {
///         engine.net().listen(SocketAddr::new(
///             Ipv4Addr::LOCALHOST.into(),
///             9001,
///         )).await.unwrap();
///     }
///
///     fn tick(&self, engine: &Arc<Engine<Self, Server>>, delta: Duration) { todo!() }
/// }
///
/// let app = MyApp {};
///
/// let side = Server::builder()
///     // 60 ticks per second
///     .tick_rate(60)
///     .build();
///
/// let engine = Engine::builder()
///     .app(app)
///     .side(side)
///     .networking_driver(GnsSubsystem::get())
///     .build();
/// ```
pub struct Server {
    tick_rate: usize,
}

impl Server {
    /// Create a builder for [Server].
    ///
    /// This is the recommended way to create a new [Server].
    #[inline]
    pub fn builder() -> ServerBuilder {
        ServerBuilder::new()
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

/// Builder pattern for [Server].
///
/// # Examples
///
/// Minimal example
/// ```no_run
/// use gtether::server::Server;
///
/// let side = Server::builder()
///     .build();
/// ```
///
/// Custom tick rate
/// ```no_run
/// use gtether::server::Server;
///
/// let side = Server::builder()
///     .tick_rate(120)
///     .build();
/// ```
pub struct ServerBuilder {
    tick_rate: Option<usize>,
}

impl ServerBuilder {
    /// Create a new [ServerBuilder].
    ///
    /// It is recommended to use [Server::builder()] instead.
    #[inline]
    pub fn new() -> Self {
        Self {
            tick_rate: None,
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

    /// Build a new [Server].
    pub fn build(self) -> Server {
        let tick_rate = self.tick_rate
            .unwrap_or(60);

        Server {
            tick_rate,
        }
    }
}