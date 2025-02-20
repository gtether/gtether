//! An implementation of gTether networking APIs using
//! [Game Networking Sockets](https://github.com/ValveSoftware/GameNetworkingSockets).
//!
//! When GNS is used, it will spawn a subsystem that runs on a separate thread. That subsystem will
//! handle all GNS related logic.
//!
//! Users will almost never directly interact with GNS, but instead can retrieve a handle to the GNS
//! subsystem via [GnsSubsystem::get()]. This handle does not do much by itself, but it does
//! implement both [RawClientFactory] and [RawServerFactory], making it suitable to use as an
//! implementation for [ClientNetworking](crate::net::client::ClientNetworking) and
//! [ServerNetworking](crate::net::server::ServerNetworking).
//!
//! # Examples
//!
//! To use for client networking:
//! ```no_run
//! use gtether::net::client::RawClientFactory;
//! use gtether::net::gns::GnsSubsystem;
//!
//! let client_factory: Box<dyn RawClientFactory> = Box::new(GnsSubsystem::get());
//! ```
//!
//! To use for server networking:
//! ```no_run
//! use gtether::net::gns::GnsSubsystem;
//! use gtether::net::server::RawServerFactory;
//!
//! let server_factory: Box<dyn RawServerFactory> = Box::new(GnsSubsystem::get());
//! ```

use ahash::{HashMap, HashMapExt};
use async_trait::async_trait;
use gns::sys::ESteamNetworkingConnectionState as NetConnectState;
use gns::{GnsConnection, GnsGlobal, GnsSocket, GnsUtils, IsClient, IsServer};
use ouroboros::self_referencing;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::net::client::{ClientNetworkingError, RawClient, RawClientFactory};
use crate::net::server::{RawServer, RawServerFactory, ServerNetworkingError};
use crate::util::tick_loop::{TickLoopBuilder, TickLoopHandle};

#[derive(Debug)]
enum ClientMessage {
    Close,
}

struct GnsClient<'gu> {
    socket: Option<GnsSocket<'gu, 'gu, IsClient>>,
    msg_recv: ump::Server<ClientMessage, (), ClientNetworkingError>,
}

impl<'gu> GnsClient<'gu> {
    fn new(
        gns_global: &'gu GnsGlobal,
        gns_utils: &'gu GnsUtils,
        socket_addr: SocketAddr,
    ) -> Result<(Self, Arc<dyn RawClient>), ClientNetworkingError> {
        let socket = GnsSocket::new(gns_global, gns_utils)
            .ok_or(ClientNetworkingError::internal_error("Failed to create socket"))?
            .connect(socket_addr.ip(), socket_addr.port())
            .map_err(|_| ClientNetworkingError::InvalidConnection(socket_addr))?;

        let (msg_recv, msg_send) = ump::channel();

        let client = Self {
            socket: Some(socket),
            msg_recv,
        };

        let client_handle = Arc::new(GnsClientHandle {
            msg_send,
        });

        Ok((client, client_handle))
    }

    fn close(&mut self) {
        // Take and drop the socket
        if let Some(socket) = self.socket.take() {
            socket.poll_callbacks();
        }
    }

    fn tick(&mut self) -> bool {
        while let Some((msg, reply_ctx)) = match self.msg_recv.try_pop() {
            Ok(val) => val,
            Err(_) => {
                // Handle is dropped, so socket should close
                self.close();
                return false;
            }
        } {
            match msg {
                ClientMessage::Close => {
                    self.close();
                    reply_ctx.reply(()).unwrap();
                    return false;
                }
            }
        }

        let socket = match self.socket.as_ref() {
            Some(socket) => socket,
            None => return false,
        };

        socket.poll_callbacks();

        let mut quit = false;
        socket.poll_event::<100>(|event| {
            match (event.old_state(), event.info().state()) {
                (
                    _,
                    NetConnectState::k_ESteamNetworkingConnectionState_ClosedByPeer
                    | NetConnectState::k_ESteamNetworkingConnectionState_ProblemDetectedLocally,
                ) => {
                    // Got disconnected or lost the connection
                    quit = true;
                    return;
                }

                (previous, current) => {
                    debug!(?previous, ?current, "Connection changing state");
                }
            }
        });

        !quit
    }
}

struct GnsClientHandle {
    msg_send: ump::Client<ClientMessage, (), ClientNetworkingError>,
}

#[async_trait]
impl RawClient for GnsClientHandle {
    #[inline]
    async fn close(&self) {
        let result = self.msg_send.areq(ClientMessage::Close).await
            .map_err(ClientNetworkingError::from);
        match result {
            Ok(_) | Err(ClientNetworkingError::Closed) => {},
            result => result
                .expect("Closing a server should not error"),
        }
    }
}

#[derive(Debug)]
enum ServerMessage {
    Close,
}

struct GnsServer<'gu> {
    socket: Option<GnsSocket<'gu, 'gu, IsServer>>,
    // TODO: What value should this store?
    clients: HashMap<GnsConnection, ()>,
    msg_recv: ump::Server<ServerMessage, (), ServerNetworkingError>,
}

impl<'gu> GnsServer<'gu> {
    fn new(
        gns_global: &'gu GnsGlobal,
        gns_utils: &'gu GnsUtils,
        socket_addr: SocketAddr,
    ) -> Result<(Self, Arc<dyn RawServer>), ServerNetworkingError> {
        let socket = GnsSocket::new(gns_global, gns_utils)
            .ok_or(ServerNetworkingError::internal_error("Failed to create socket"))?
            .listen(socket_addr.ip(), socket_addr.port())
            .map_err(|_| ServerNetworkingError::InvalidListen(socket_addr))?;

        let (msg_recv, msg_send) = ump::channel();

        let server = Self {
            socket: Some(socket),
            clients: HashMap::new(),
            msg_recv,
        };

        let server_handle = Arc::new(GnsServerHandle {
            msg_send,
        });

        Ok((server, server_handle))
    }

    fn close(&mut self) {
        // Take and drop the socket
        if let Some(socket) = self.socket.take() {
            for (connection, _) in self.clients.drain() {
                socket.close_connection(connection, 0, "", false);
            }
            socket.poll_callbacks();
        }
    }

    fn tick(&mut self) -> bool {
        while let Some((msg, reply_ctx)) = match self.msg_recv.try_pop() {
            Ok(val) => val,
            Err(_) => {
                // Handle is dropped, so socket should close
                self.close();
                return false;
            }
        } {
            match msg {
                ServerMessage::Close => {
                    self.close();
                    reply_ctx.reply(()).unwrap();
                    return false;
                }
            }
        }

        let socket = match self.socket.as_ref() {
            Some(socket) => socket,
            None => return false,
        };

        socket.poll_callbacks();

        socket.poll_event::<100>(|event| {
            match (event.old_state(), event.info().state()) {
                // A client is connecting
                (
                    NetConnectState::k_ESteamNetworkingConnectionState_None,
                    NetConnectState::k_ESteamNetworkingConnectionState_Connecting,
                ) => {
                    let connection = event.connection();
                    let remote_addr = event.info().remote_address();
                    match socket.accept(connection) {
                        Ok(_) => {
                            info!(?remote_addr, "Client connected");
                            self.clients.insert(connection, ());
                        },
                        Err(_) => {
                            warn!(?remote_addr, "Failed to accept client");
                        },
                    }
                }

                (
                    _,
                    NetConnectState::k_ESteamNetworkingConnectionState_ClosedByPeer
                    | NetConnectState::k_ESteamNetworkingConnectionState_ProblemDetectedLocally,
                ) => {
                    let connection = event.connection();
                    let remote_addr = event.info().remote_address();
                    info!(?remote_addr, "Client disconnected");
                    self.clients.remove(&connection);
                    socket.close_connection(connection, 0, "", false);
                }

                // A client state is changing
                (previous, current) => {
                    let remote_addr = event.info().remote_address();
                    debug!(?remote_addr, ?previous, ?current, "Client connection changing state");
                }
            }
        });

        true
    }
}

struct GnsServerHandle {
    msg_send: ump::Client<ServerMessage, (), ServerNetworkingError>,
}

#[async_trait]
impl RawServer for GnsServerHandle {
    #[inline]
    async fn close(&self) {
        let result = self.msg_send.areq(ServerMessage::Close).await
            .map_err(ServerNetworkingError::from);
        match result {
            Ok(_) | Err(ServerNetworkingError::Closed) => {},
            result => result
                .expect("Closing a server should not error"),
        }
    }
}

static SUBSYSTEM: LazyLock<Arc<GnsSubsystem>> = LazyLock::new(|| {
    // Not really a good way to error handle this. Let it just panic for now.
    Arc::new(GnsSubsystem::new())
});

#[self_referencing]
struct SubsystemData {
    gns_global: GnsGlobal,
    gns_utils: GnsUtils,
    #[borrows(gns_global, gns_utils)]
    #[not_covariant]
    clients: Vec<GnsClient<'this>>,
    #[borrows(gns_global, gns_utils)]
    #[not_covariant]
    servers: Vec<GnsServer<'this>>,
    msg_create_client_recv: ump::Server<SocketAddr, Arc<dyn RawClient>, ClientNetworkingError>,
    msg_create_server_recv: ump::Server<SocketAddr, Arc<dyn RawServer>, ServerNetworkingError>,
}

/// Handle for the GNS subsystem.
///
/// This handle can be used to interact with the internally running GNS thread. This handle also
/// implements both [RawClientFactory] and [RawServerFactory] for Arcs.
///
/// For users, you probably want [GnsSubsystem::get()], which retrieves a reference to this handle.
pub struct GnsSubsystem {
    #[allow(unused)]
    tick_loop: TickLoopHandle,
    msg_create_client_send: ump::Client<SocketAddr, Arc<dyn RawClient>, ClientNetworkingError>,
    msg_create_server_send: ump::Client<SocketAddr, Arc<dyn RawServer>, ServerNetworkingError>,
}

impl GnsSubsystem {
    fn tick(data: &mut SubsystemData, _delta: Duration) -> bool {
        data.with_mut(|data| {
            while let Some((socket_addr, reply_ctx))
                    = data.msg_create_client_recv.try_pop().unwrap_or(None) {
                match GnsClient::new(
                    data.gns_global,
                    data.gns_utils,
                    socket_addr,
                ) {
                    Ok((client, client_handle)) => {
                        data.clients.push(client);
                        reply_ctx.reply(client_handle).unwrap();
                    },
                    Err(err) => reply_ctx.fail(err).unwrap(),
                }
            }

            while let Some((socket_addr, reply_ctx))
                    = data.msg_create_server_recv.try_pop().unwrap_or(None) {
                match GnsServer::new(
                    data.gns_global,
                    data.gns_utils,
                    socket_addr,
                ) {
                    Ok((server, server_handle)) => {
                        data.servers.push(server);
                        reply_ctx.reply(server_handle).unwrap();
                    },
                    Err(err) => reply_ctx.fail(err).unwrap(),
                }
            }

            data.clients.retain_mut(|client| client.tick());
            data.servers.retain_mut(|server| server.tick());
        });
        true
    }

    fn new() -> Self {
        debug!("Starting GNS subsystem");
        let (msg_create_client_recv, msg_create_client_send) = ump::channel();
        let (msg_create_server_recv, msg_create_server_send) = ump::channel();
        let tick_loop = TickLoopBuilder::new()
            .name("gns-subsystem")
            .init(move || {
                let gns_global = GnsGlobal::get().unwrap();
                let gns_utils = GnsUtils::new().unwrap();

                Ok::<_, ()>(SubsystemDataBuilder {
                    gns_global,
                    gns_utils,
                    clients_builder: move |_, _| Vec::new(),
                    servers_builder: move |_, _| Vec::new(),
                    msg_create_client_recv,
                    msg_create_server_recv,
                }.build())
            })
            .tick(Self::tick)
            .spawn()
            // TODO: Error handle
            .unwrap();

        Self {
            tick_loop,
            msg_create_client_send,
            msg_create_server_send,
        }
    }

    /// Get a reference to the [GnsSubsystem] handle.
    ///
    /// If the subsystem has not yet been started, this will start it.
    #[inline]
    pub fn get() -> Arc<Self> {
        SUBSYSTEM.clone()
    }
}

#[async_trait]
impl RawClientFactory for Arc<GnsSubsystem> {
    async fn connect(&self, socket_addr: SocketAddr) -> Result<Arc<dyn RawClient>, ClientNetworkingError> {
        self.msg_create_client_send.areq(socket_addr).await
            .map_err(|err| match err {
                ump::Error::App(err) => err,
                _ => ClientNetworkingError::internal_error("No response from subsystem"),
            })
    }
}

#[async_trait]
impl RawServerFactory for Arc<GnsSubsystem> {
    async fn listen(&self, socket_addr: SocketAddr) -> Result<Arc<dyn RawServer>, ServerNetworkingError> {
        self.msg_create_server_send.areq(socket_addr).await
            .map_err(|err| match err {
                ump::Error::App(err) => err,
                _ => ServerNetworkingError::internal_error("No response from subsystem"),
            })
    }
}