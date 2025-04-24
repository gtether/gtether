//! An implementation of gTether networking [drivers](NetDriver) using
//! [Game Networking Sockets](https://github.com/ValveSoftware/GameNetworkingSockets).
//!
//! When GNS is used, it will spawn a subsystem that runs on a separate thread. That subsystem will
//! handle all GNS related logic.
//!
//! Users will almost never directly interact with GNS, but instead can retrieve a handle to the GNS
//! subsystem via [GnsSubsystem::get()]. This handle does not do much by itself, but it does
//! implement [NetDriverFactory] for both [GnsClientDriver] and [GnsServerDriver], making it
//! suitable to use when creating a [Networking](super::Networking) stack.
//!
//! # Examples
//!
//! To use for client networking:
//! ```no_run
//! use gtether::net::gns::{GnsClientDriver, GnsSubsystem};
//! use gtether::net::driver::NetDriverFactory;
//!
//! let client_factory: Box<dyn NetDriverFactory<GnsClientDriver>> = Box::new(GnsSubsystem::get());
//! ```
//!
//! To use for server networking:
//! ```no_run
//! use gtether::net::gns::{GnsServerDriver, GnsSubsystem};
//! use gtether::net::driver::NetDriverFactory;
//!
//! let server_factory: Box<dyn NetDriverFactory<GnsServerDriver>> = Box::new(GnsSubsystem::get());
//! ```

use async_trait::async_trait;
use bimap::BiHashMap;
use educe::Educe;
use flagset::FlagSet;
use gns::sys::{k_nSteamNetworkingSend_Reliable, ESteamNetworkingConnectionState as NetConnectState, ESteamNetworkingSocketsDebugOutputType};
use gns::{GnsConnection, GnsGlobal, GnsSocket, GnsUtils, IsClient, IsServer};
use image::EncodableLayout;
use itertools::Either;
use ouroboros::self_referencing;
use parking_lot::RwLock;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::net::{Connection, DisconnectReason, NetworkingApi, NetworkingError};
use crate::net::driver::{NetDriver, NetDriverBroadcast, NetDriverCloseConnection, NetDriverConnect, NetDriverFactory, NetDriverListen, NetDriverSend, NetDriverSendTo};
use crate::net::message::MessageFlags;
use crate::util::tick_loop::{TickLoopBuilder, TickLoopHandle};

fn msg_send_flags_to_gns(flags: FlagSet<MessageFlags>) -> std::os::raw::c_int {
    let mut gns_flags = 0;

    if  flags.contains(MessageFlags::Reliable) {
        gns_flags |= k_nSteamNetworkingSend_Reliable;
    }

    gns_flags
}

#[derive(Debug)]
enum ClientMessage {
    Send {
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    },
    Close,
}

struct GnsClient<'gu> {
    api: Arc<NetworkingApi>,
    api_connection: Connection,
    socket: Option<GnsSocket<'gu, 'gu, IsClient>>,
    msg_recv: ump::Server<ClientMessage, (), NetworkingError>,
}

impl<'gu> GnsClient<'gu> {
    fn new(
        gns_global: &'gu GnsGlobal,
        gns_utils: &'gu GnsUtils,
        api: Arc<NetworkingApi>,
        api_connection: Connection,
        socket_addr: SocketAddr,
    ) -> Result<(Self, GnsClientHandle), NetworkingError> {
        let socket = GnsSocket::new(gns_global, gns_utils)
            .ok_or(NetworkingError::internal_error("Failed to create socket"))?
            .connect(socket_addr.ip(), socket_addr.port())
            .map_err(|_| NetworkingError::InvalidAddress(socket_addr))?;

        let (msg_recv, msg_send) = ump::channel();

        let client = Self {
            api,
            api_connection,
            socket: Some(socket),
            msg_recv,
        };

        let client_handle = GnsClientHandle(msg_send);

        Ok((client, client_handle))
    }

    fn close(&mut self) {
        // Take and drop the socket
        if let Some(socket) = self.socket.take() {
            socket.poll_callbacks();
            self.api.disconnect(self.api_connection, DisconnectReason::ClosedLocally);
        }
    }

    fn tick(&mut self) -> bool {
        let mut msgs_to_send = Vec::new();

        while let Some((msg, reply_ctx)) = match self.msg_recv.try_pop() {
            Ok(val) => val,
            Err(_) => {
                // Handle is dropped, so socket should close
                self.close();
                return false;
            }
        } {
            match msg {
                ClientMessage::Send { msg, flags } => {
                    msgs_to_send.push((msg, flags));
                },
                ClientMessage::Close => {
                    self.close();
                    reply_ctx.reply(()).unwrap();
                    return false;
                },
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
                (_, NetConnectState::k_ESteamNetworkingConnectionState_ClosedByPeer) => {
                    // Got disconnected or lost the connection
                    self.api.disconnect(self.api_connection, DisconnectReason::ClosedByPeer);
                    quit = true;
                    return;
                }

                (_, NetConnectState::k_ESteamNetworkingConnectionState_ProblemDetectedLocally) => {
                    // Lost the connection
                    self.api.disconnect(self.api_connection, DisconnectReason::Unexpected);
                    quit = true;
                    return;
                }

                (previous, current) => {
                    debug!(?previous, ?current, "Connection changing state");
                }
            }
        });

        socket.poll_messages::<100>(|message| {
            match self.api.dispatch_message(self.api_connection, &mut message.payload()) {
                Ok(_) => {},
                Err(error) => {
                    warn!(?error, "Failed to dispatch message");
                }
            }
        });

        let msgs_to_send = msgs_to_send.into_iter()
            .map(|(msg, flags)| socket.utils().allocate_message(
                socket.connection(),
                msg_send_flags_to_gns(flags),
                msg.as_bytes(),
            ))
            .collect::<Vec<_>>();
        if !msgs_to_send.is_empty() {
            for msg in socket.send_messages(msgs_to_send) {
                match msg {
                    Either::Left(_) => {},
                    Either::Right(error) =>
                        warn!(?error, "Failed to send GNS message"),
                }
            }
        }

        !quit
    }
}

struct GnsClientHandle(ump::Client<ClientMessage, (), NetworkingError>);

#[derive(Debug)]
enum ServerMessage {
    Send {
        connection: Connection,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    },
    Broadcast {
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    },
    Close,
    CloseConnection {
        connection: Connection,
    },
}

type ClientsMap = BiHashMap<GnsConnection, Connection, ahash::RandomState, ahash::RandomState>;

struct GnsServer<'gu> {
    api: Arc<NetworkingApi>,
    socket: Option<GnsSocket<'gu, 'gu, IsServer>>,
    clients: Arc<RwLock<ClientsMap>>,
    msg_recv: ump::Server<ServerMessage, (), NetworkingError>,
}

impl<'gu> GnsServer<'gu> {
    fn new(
        gns_global: &'gu GnsGlobal,
        gns_utils: &'gu GnsUtils,
        api: Arc<NetworkingApi>,
        socket_addr: SocketAddr,
    ) -> Result<(Self, GnsServerHandle), NetworkingError> {
        let socket = GnsSocket::new(gns_global, gns_utils)
            .ok_or(NetworkingError::internal_error("Failed to create socket"))?
            .listen(socket_addr.ip(), socket_addr.port())
            .map_err(|_| NetworkingError::InvalidAddress(socket_addr))?;

        let (msg_recv, msg_send) = ump::channel();

        let clients = Arc::new(RwLock::new(BiHashMap::with_hashers(
            ahash::RandomState::default(),
            ahash::RandomState::default(),
        )));

        let server = Self {
            api,
            socket: Some(socket),
            clients: clients.clone(),
            msg_recv,
        };

        let server_handle = GnsServerHandle {
            msg_send,
            clients,
        };

        Ok((server, server_handle))
    }

    fn close(&mut self) {
        // Take and drop the socket
        if let Some(socket) = self.socket.take() {
            let mut clients = self.clients.write();
            clients.retain(|connection, api_connection| {
                socket.close_connection(*connection, 0, "", false);
                self.api.disconnect(*api_connection, DisconnectReason::ClosedLocally);
                false
            });
            socket.poll_callbacks();
        }
    }

    fn tick(&mut self) -> bool {
        let mut msgs_to_send = Vec::new();
        let mut msgs_to_broadcast = Vec::new();
        let mut connections_to_close = Vec::new();

        while let Some((msg, reply_ctx)) = match self.msg_recv.try_pop() {
            Ok(val) => val,
            Err(_) => {
                // Handle is dropped, so socket should close
                self.close();
                return false;
            }
        } {
            match msg {
                ServerMessage::Send { connection, msg, flags } => {
                    msgs_to_send.push((connection, msg, flags));
                },
                ServerMessage::Broadcast { msg, flags } => {
                    msgs_to_broadcast.push((msg, flags));
                },
                ServerMessage::Close => {
                    self.close();
                    reply_ctx.reply(()).unwrap();
                    return false;
                },
                ServerMessage::CloseConnection {connection} => {
                    connections_to_close.push(connection);
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
                            if let Some(api_connection) = self.api.try_init_connect(remote_addr) {
                                info!(?remote_addr, ?api_connection, "Client connected");
                                let mut clients = self.clients.write();
                                clients.insert(connection, api_connection);
                            } else {
                                info!(?remote_addr, "Client connection cancelled");
                            }
                        },
                        Err(_) => {
                            warn!(?remote_addr, "Failed to accept client");
                        },
                    }
                }

                (_, NetConnectState::k_ESteamNetworkingConnectionState_ClosedByPeer) => {
                    let connection = event.connection();
                    let remote_addr = event.info().remote_address();
                    info!(?remote_addr, "Client disconnected");
                    let mut clients = self.clients.write();
                    let (_, api_connection) = clients.remove_by_left(&connection).unwrap();
                    socket.close_connection(connection, 0, "", false);
                    self.api.disconnect(api_connection, DisconnectReason::ClosedByPeer);
                }

                (_, NetConnectState::k_ESteamNetworkingConnectionState_ProblemDetectedLocally) => {
                    let connection = event.connection();
                    let remote_addr = event.info().remote_address();
                    info!(?remote_addr, "Client disconnected unexpectedly");
                    let mut clients = self.clients.write();
                    let (_, api_connection) = clients.remove_by_left(&connection).unwrap();
                    socket.close_connection(connection, 0, "", false);
                    self.api.disconnect(api_connection, DisconnectReason::Unexpected);
                }

                // A client state is changing
                (previous, current) => {
                    let remote_addr = event.info().remote_address();
                    debug!(?remote_addr, ?previous, ?current, "Client connection changing state");
                }
            }
        });

        let clients = self.clients.read();

        socket.poll_messages::<100>(|message| {
            let connection = message.connection();
            if let Some(api_connection) = clients.get_by_left(&connection) {
                match self.api.dispatch_message(*api_connection, &mut message.payload()) {
                    Ok(_) => {},
                    Err(error) => {
                        warn!(?api_connection, ?error, "Failed to dispatch message");
                    }
                }
            } else {
                warn!(?connection, "Received message for untracked GNS connection");
            }
        });

        let msgs_to_send = msgs_to_send.into_iter()
            .filter_map(|(connection, msg, flags)| {
                if let Some(gns_connection) = clients.get_by_right(&connection) {
                    Some(socket.utils().allocate_message(
                        *gns_connection,
                        msg_send_flags_to_gns(flags),
                        msg.as_bytes(),
                    ))
                } else {
                    debug!(?connection, "Dropping message for untracked connection");
                    None
                }
            })
            .chain(msgs_to_broadcast.into_iter()
                .map(|(msg, flags)| {
                    let flags = msg_send_flags_to_gns(flags);
                    clients.left_values()
                        .map(move |gns_connection| socket.utils().allocate_message(
                            *gns_connection,
                            flags,
                            msg.as_bytes(),
                        ))
                })
                .flatten())
            .collect::<Vec<_>>();
        if !msgs_to_send.is_empty() {
            for msg in socket.send_messages(msgs_to_send) {
                match msg {
                    Either::Left(_) => {},
                    Either::Right(error) =>
                        warn!(?error, "Failed to send GNS message"),
                }
            }
        }

        for api_connection in connections_to_close {
            let mut clients = self.clients.write();
            if let Some((connection, _)) = clients.remove_by_right(&api_connection) {
                info!(?api_connection, "Closing connection");
                socket.close_connection(connection, 0, "", false);
                self.api.disconnect(api_connection, DisconnectReason::ClosedLocally);
            }
        }

        true
    }
}

struct GnsServerHandle {
    msg_send: ump::Client<ServerMessage, (), NetworkingError>,
    clients: Arc<RwLock<ClientsMap>>,
}

static SUBSYSTEM: LazyLock<Arc<GnsSubsystem>> = LazyLock::new(|| {
    // Not really a good way to error handle this. Let it just panic for now.
    Arc::new(GnsSubsystem::new())
});

#[derive(Educe)]
#[educe(Debug)]
struct CreateClientReq {
    connection: Connection,
    socket_addr: SocketAddr,
    #[educe(Debug(ignore))]
    api: Arc<NetworkingApi>,
}

#[derive(Educe)]
#[educe(Debug)]
struct CreateServerReq {
    socket_addr: SocketAddr,
    #[educe(Debug(ignore))]
    api: Arc<NetworkingApi>,
}

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
    msg_create_client_recv: ump::Server<CreateClientReq, GnsClientHandle, NetworkingError>,
    msg_create_server_recv: ump::Server<CreateServerReq, GnsServerHandle, NetworkingError>,
}

/// Handle for the GNS subsystem.
///
/// This handle can be used to interact with the internally running GNS thread. This handle also
/// implements [NetDriverFactory] for both [GnsClientDriver] and [GnsServerDriver] for Arcs.
///
/// For users, you probably want [GnsSubsystem::get()], which retrieves a reference to this handle.
pub struct GnsSubsystem {
    #[allow(unused)]
    tick_loop: TickLoopHandle,
    msg_create_client_send: ump::Client<CreateClientReq, GnsClientHandle, NetworkingError>,
    msg_create_server_send: ump::Client<CreateServerReq, GnsServerHandle, NetworkingError>,
}

impl GnsSubsystem {
    fn tick(data: &mut SubsystemData, _delta: Duration) -> bool {
        data.with_mut(|data| {
            while let Some((req, reply_ctx))
                    = data.msg_create_client_recv.try_pop().unwrap_or(None) {
                match GnsClient::new(
                    data.gns_global,
                    data.gns_utils,
                    req.api,
                    req.connection,
                    req.socket_addr,
                ) {
                    Ok((client, client_handle)) => {
                        data.clients.push(client);
                        reply_ctx.reply(client_handle).unwrap();
                    },
                    Err(err) => reply_ctx.fail(err).unwrap(),
                }
            }

            while let Some((req, reply_ctx))
                    = data.msg_create_server_recv.try_pop().unwrap_or(None) {
                match GnsServer::new(
                    data.gns_global,
                    data.gns_utils,
                    req.api,
                    req.socket_addr,
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

                gns_utils.enable_debug_output(
                    ESteamNetworkingSocketsDebugOutputType::k_ESteamNetworkingSocketsDebugOutputType_Debug,
                    |ty, msg| {
                        match ty {
                            ESteamNetworkingSocketsDebugOutputType::k_ESteamNetworkingSocketsDebugOutputType_Debug |
                            ESteamNetworkingSocketsDebugOutputType::k_ESteamNetworkingSocketsDebugOutputType_Verbose =>
                                debug!(msg),
                            ESteamNetworkingSocketsDebugOutputType::k_ESteamNetworkingSocketsDebugOutputType_Msg =>
                                info!(msg),
                            ESteamNetworkingSocketsDebugOutputType::k_ESteamNetworkingSocketsDebugOutputType_Warning |
                            ESteamNetworkingSocketsDebugOutputType::k_ESteamNetworkingSocketsDebugOutputType_Important =>
                                warn!(msg),
                            ESteamNetworkingSocketsDebugOutputType::k_ESteamNetworkingSocketsDebugOutputType_Error |
                            ESteamNetworkingSocketsDebugOutputType::k_ESteamNetworkingSocketsDebugOutputType_Bug =>
                                error!(msg),
                            _ => {},
                        };
                    }
                );

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

    async fn connect_client(
        &self,
        api: Arc<NetworkingApi>,
        connection: Connection,
        socket_addr: SocketAddr,
    ) -> Result<GnsClientHandle, NetworkingError> {
        let req = CreateClientReq {
            connection,
            socket_addr,
            api,
        };
        self.msg_create_client_send.areq(req).await
            .map_err(|err| match err {
                ump::Error::App(err) => err,
                _ => NetworkingError::internal_error("No response from subsystem"),
            })
    }

    async fn listen_server(
        &self,
        api: Arc<NetworkingApi>,
        socket_addr: SocketAddr,
    ) -> Result<GnsServerHandle, NetworkingError> {
        let req = CreateServerReq {
            socket_addr,
            api,
        };
        self.msg_create_server_send.areq(req).await
            .map_err(|err| match err {
                ump::Error::App(err) => err,
                _ => NetworkingError::internal_error("No response from subsystem"),
            })
    }
}

/// Client-side [GNS](https://github.com/ValveSoftware/GameNetworkingSockets)-based [NetDriver].
///
/// Connects to a single endpoint to send messages. Connecting to a different endpoint will close
/// the existing connection.
///
/// Created using the [factory](NetDriverFactory) implementation on [GnsSubsystem].
pub struct GnsClientDriver {
    subsystem: Arc<GnsSubsystem>,
    api: Arc<NetworkingApi>,
    handle: smol::lock::RwLock<Option<GnsClientHandle>>,
}

#[async_trait]
impl NetDriver for GnsClientDriver {
    async fn close(&self) {
        if let Some(handle) = self.handle.write().await.take() {
            let result = handle.0.areq(ClientMessage::Close).await
                .map_err(NetworkingError::from);
            match result {
                Ok(_) | Err(NetworkingError::Closed) => {},
                result => result
                    .expect("Closing a client should not error"),
            }
        }
    }
}

#[async_trait]
impl NetDriverConnect for GnsClientDriver {
    async fn connect(
        &self,
        connection: Connection,
        socket_addr: SocketAddr,
    ) -> Result<(), NetworkingError> {
        self.close().await;
        let handle = self.subsystem.connect_client(
            self.api.clone(),
            connection,
            socket_addr,
        ).await?;
        *self.handle.write().await = Some(handle);
        Ok(())
    }
}

impl NetDriverSend for GnsClientDriver {
    fn send(&self, msg: Vec<u8>, flags: FlagSet<MessageFlags>) -> Result<(), NetworkingError> {
        if let Some(handle) = &*self.handle.read_blocking() {
            handle.0.req_async(ClientMessage::Send { msg, flags })
                // We're firing and forgetting, so we don't care about the WaitReply<>
                .map(|_| ())
                .map_err(NetworkingError::from)
        } else {
            Err(NetworkingError::Closed)
        }
    }
}

impl NetDriverFactory<GnsClientDriver> for Arc<GnsSubsystem> {
    fn create(&self, api: Arc<NetworkingApi>) -> Arc<GnsClientDriver> {
        Arc::new(GnsClientDriver {
            subsystem: self.clone(),
            api,
            handle: smol::lock::RwLock::new(None),
        })
    }
}

/// Server-side [GNS](https://github.com/ValveSoftware/GameNetworkingSockets)-based [NetDriver].
///
/// Listens on a socket and accepts many connections.
///
/// Created using the [factory](NetDriverFactory) implementation on [GnsSubsystem].
pub struct GnsServerDriver {
    subsystem: Arc<GnsSubsystem>,
    api: Arc<NetworkingApi>,
    handle: smol::lock::RwLock<Option<GnsServerHandle>>,
}

#[async_trait]
impl NetDriver for GnsServerDriver {
    async fn close(&self) {
        if let Some(handle) = self.handle.write().await.take() {
            let result = handle.msg_send.areq(ServerMessage::Close).await
                .map_err(NetworkingError::from);
            match result {
                Ok(_) | Err(NetworkingError::Closed) => {},
                result => result
                    .expect("Closing a server should not error"),
            }
        }
    }
}

#[async_trait]
impl NetDriverCloseConnection for GnsServerDriver {
    async fn close_connection(&self, connection: Connection) {
        if let Some(handle) = &*self.handle.read().await {
            let result = handle.msg_send.areq(ServerMessage::CloseConnection { connection }).await
                .map_err(NetworkingError::from);
            match result {
                Ok(_) | Err(NetworkingError::Closed) => {},
                result => result
                    .expect("Closing a connection should not error"),
            }
        }
    }
}

#[async_trait]
impl NetDriverListen for GnsServerDriver {
    async fn listen(&self, socket_addr: SocketAddr) -> Result<(), NetworkingError> {
        self.close().await;
        let handle = self.subsystem.listen_server(self.api.clone(), socket_addr).await?;
        *self.handle.write().await = Some(handle);
        Ok(())
    }
}

impl NetDriverSendTo for GnsServerDriver {
    fn send_to(
        &self,
        connection: Connection,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), NetworkingError> {
        if let Some(handle) = &*self.handle.read_blocking() {
            let clients = handle.clients.read();
            if clients.contains_right(&connection) {
                handle.msg_send.req_async(ServerMessage::Send { connection, msg, flags })
                    .map(|_| ())
                    .map_err(NetworkingError::from)
            } else {
                Err(NetworkingError::InvalidConnection(connection))
            }
        } else {
            Err(NetworkingError::Closed)
        }
    }
}

impl NetDriverBroadcast for GnsServerDriver {
    fn broadcast(
        &self,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), NetworkingError> {
        if let Some(handle) = &*self.handle.read_blocking() {
            handle.msg_send.req_async(ServerMessage::Broadcast { msg, flags })
                .map(|_| ())
                .map_err(NetworkingError::from)
        } else {
            Err(NetworkingError::Closed)
        }
    }
}

impl NetDriverFactory<GnsServerDriver> for Arc<GnsSubsystem> {
    fn create(&self, api: Arc<NetworkingApi>) -> Arc<GnsServerDriver> {
        Arc::new(GnsServerDriver {
            subsystem: self.clone(),
            api,
            handle: smol::lock::RwLock::new(None),
        })
    }
}