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

use std::fmt::{Display, Formatter};
use async_trait::async_trait;
use bimap::BiHashMap;
use educe::Educe;
use flagset::FlagSet;
use gns::sys::{k_nSteamNetworkingSend_Reliable, ESteamNetworkingConnectionState as NetConnectState, ESteamNetworkingSocketsDebugOutputType};
use gns::{GnsConnection, GnsGlobal, GnsSocket, IsClient, IsServer};
use itertools::Either;
use parking_lot::RwLock;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::sync::{Arc, LazyLock};
use std::sync::atomic::{AtomicU64, Ordering};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GnsClientId(NonZeroU64);

impl Display for GnsClientId {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "client-{}", self.0)
    }
}

struct GnsClientState {
    socket: GnsSocket<IsClient>,
    msg_recv: ump::Server<ClientMessage, (), NetworkingError>,
}

#[derive(Educe)]
#[educe(Debug)]
struct GnsClient {
    #[educe(Debug(method(std::fmt::Display::fmt)))]
    id: GnsClientId,
    #[educe(Debug(ignore))]
    api: Arc<NetworkingApi>,
    api_connection: Connection,
    #[educe(Debug(ignore))]
    gns_global: Arc<GnsGlobal>,
    #[educe(Debug(ignore))]
    state: Option<GnsClientState>,
}

impl GnsClient {
    fn new(
        gns_global: Arc<GnsGlobal>,
        id: GnsClientId,
        api: Arc<NetworkingApi>,
        api_connection: Connection,
        socket_addr: SocketAddr,
    ) -> Result<(Self, GnsClientHandle), NetworkingError> {
        let socket = GnsSocket::new(gns_global.clone())
            .connect(socket_addr.ip(), socket_addr.port())
            .map_err(|_| NetworkingError::InvalidAddress(socket_addr))?;

        let (msg_recv, msg_send) = ump::channel();

        let client = Self {
            id,
            api,
            api_connection,
            gns_global,
            state: Some(GnsClientState {
                socket,
                msg_recv,
            }),
        };

        let client_handle = GnsClientHandle(msg_send);

        Ok((client, client_handle))
    }

    fn close(&mut self, reason: DisconnectReason) {
        // Take and drop the socket
        if let Some(state) = self.state.take() {
            debug!(client = ?self, "Closing");
            // Explicitly drop the state before notifying the API, so we can ensure no more msgs
            //  send attempts are made after the API registers that we're closed
            drop(state);
            self.api.disconnect(self.api_connection, reason);
        }
    }

    fn tick(&mut self) -> bool {
        let mut msgs_to_send = Vec::new();

        let state = match self.state.as_ref() {
            Some(state) => state,
            None => return false,
        };

        while let Some((msg, reply_ctx)) = match state.msg_recv.try_pop() {
            Ok(val) => val,
            Err(_) => {
                // Handle is dropped, so socket should close
                self.close(DisconnectReason::ClosedLocally);
                return false;
            }
        } {
            match msg {
                ClientMessage::Send { msg, flags } => {
                    msgs_to_send.push((msg, flags));
                    // A reply isn't expected, but send one anyway for hygiene's sake
                    reply_ctx.reply(()).unwrap();
                },
                ClientMessage::Close => {
                    self.close(DisconnectReason::ClosedLocally);
                    reply_ctx.reply(()).unwrap();
                    return false;
                },
            }
        }

        state.socket.poll_messages::<100>(|message| {
            match self.api.dispatch_message(self.api_connection, &mut message.payload()) {
                Ok(_) => {},
                Err(error) => {
                    warn!(?error, "Failed to dispatch message");
                }
            }
        });

        let mut quit_reason = None;
        state.socket.poll_event::<100>(|event| {
            let (previous, current) = (event.old_state(), event.info().state());
            debug!(?previous, ?current, "Connection changing state");
            match (previous, current) {
                (_, NetConnectState::k_ESteamNetworkingConnectionState_ClosedByPeer) => {
                    // Got disconnected or lost the connection
                    quit_reason = Some(DisconnectReason::ClosedByPeer);
                    return;
                }

                (_, NetConnectState::k_ESteamNetworkingConnectionState_ProblemDetectedLocally) => {
                    // Lost the connection
                    quit_reason = Some(DisconnectReason::Unexpected);
                    return;
                }

                _ => {}
            }
        });
        if let Some(quit_reason) = quit_reason {
            self.close(quit_reason);
            return false;
        }

        let msgs_to_send = msgs_to_send.into_iter()
            .map(|(msg, flags)| self.gns_global.utils().allocate_message(
                state.socket.connection(),
                msg_send_flags_to_gns(flags),
                &msg,
            ))
            .collect::<Vec<_>>();
        if !msgs_to_send.is_empty() {
            for msg in state.socket.send_messages(msgs_to_send) {
                match msg {
                    Either::Left(_) => {},
                    Either::Right(error) =>
                        warn!(?error, "Failed to send GNS message"),
                }
            }
        }

        true
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GnsServerId(NonZeroU64);

impl Display for GnsServerId {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "server-{}", self.0)
    }
}

type ClientsMap = BiHashMap<GnsConnection, Connection, ahash::RandomState, ahash::RandomState>;

#[derive(Educe)]
#[educe(Debug)]
struct GnsServer {
    #[educe(Debug(method(std::fmt::Display::fmt)))]
    id: GnsServerId,
    #[educe(Debug(ignore))]
    api: Arc<NetworkingApi>,
    #[educe(Debug(ignore))]
    gns_global: Arc<GnsGlobal>,
    #[educe(Debug(ignore))]
    socket: Option<GnsSocket<IsServer>>,
    clients: Arc<RwLock<ClientsMap>>,
    #[educe(Debug(ignore))]
    msg_recv: ump::Server<ServerMessage, (), NetworkingError>,
}

impl GnsServer {
    fn new(
        gns_global: Arc<GnsGlobal>,
        id: GnsServerId,
        api: Arc<NetworkingApi>,
        socket_addr: SocketAddr,
    ) -> Result<(Self, GnsServerHandle), NetworkingError> {
        let socket = GnsSocket::new(gns_global.clone())
            .listen(socket_addr.ip(), socket_addr.port())
            .map_err(|_| NetworkingError::InvalidAddress(socket_addr))?;

        let (msg_recv, msg_send) = ump::channel();

        let clients = Arc::new(RwLock::new(BiHashMap::with_hashers(
            ahash::RandomState::default(),
            ahash::RandomState::default(),
        )));

        let server = Self {
            id,
            api,
            gns_global,
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
            debug!(server = ?self, "Closing");
            let mut clients = self.clients.write();
            clients.retain(|connection, api_connection| {
                socket.close_connection(*connection, 0, "", false);
                self.api.disconnect(*api_connection, DisconnectReason::ClosedLocally);
                false
            });
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

        let mut clients = self.clients.upgradable_read();

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
                                clients.with_upgraded(|clients| {
                                    clients.insert(connection, api_connection);
                                });
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
                    let (_, api_connection) = clients.with_upgraded(|clients| {
                        clients.remove_by_left(&connection).unwrap()
                    });
                    socket.close_connection(connection, 0, "", false);
                    self.api.disconnect(api_connection, DisconnectReason::ClosedByPeer);
                }

                (_, NetConnectState::k_ESteamNetworkingConnectionState_ProblemDetectedLocally) => {
                    let connection = event.connection();
                    let remote_addr = event.info().remote_address();
                    info!(?remote_addr, "Client disconnected unexpectedly");
                    let (_, api_connection) = clients.with_upgraded(|clients| {
                        clients.remove_by_left(&connection).unwrap()
                    });
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

        let msgs_to_send = msgs_to_send.into_iter()
            .filter_map(|(connection, msg, flags)| {
                if let Some(gns_connection) = clients.get_by_right(&connection) {
                    Some(self.gns_global.utils().allocate_message(
                        *gns_connection,
                        msg_send_flags_to_gns(flags),
                        &msg,
                    ))
                } else {
                    debug!(?connection, "Dropping message for untracked connection");
                    None
                }
            })
            .chain(msgs_to_broadcast.into_iter()
                .map(|(msg, flags)| {
                    let flags = msg_send_flags_to_gns(flags);
                    let gns_global = self.gns_global.clone();
                    clients.left_values()
                        .map(move |gns_connection| gns_global.utils().allocate_message(
                            *gns_connection,
                            flags,
                            &msg,
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
            clients.with_upgraded(|clients| {
                if let Some((connection, _)) = clients.remove_by_right(&api_connection) {
                    info!(?api_connection, server = ?self, "Closing connection");
                    socket.close_connection(connection, 0, "", false);
                    self.api.disconnect(api_connection, DisconnectReason::ClosedLocally);
                }
            })
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

struct SubsystemData {
    gns_global: Arc<GnsGlobal>,
    next_client_id: AtomicU64,
    clients: Vec<GnsClient>,
    msg_create_client_recv: ump::Server<CreateClientReq, GnsClientHandle, NetworkingError>,
    next_server_id: AtomicU64,
    servers: Vec<GnsServer>,
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
        while let Some((req, reply_ctx))
                = data.msg_create_client_recv.try_pop().unwrap_or(None) {
            match GnsClient::new(
                data.gns_global.clone(),
                GnsClientId(data.next_client_id.fetch_add(1, Ordering::SeqCst).try_into().unwrap()),
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
                data.gns_global.clone(),
                GnsServerId(data.next_server_id.fetch_add(1, Ordering::SeqCst).try_into().unwrap()),
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

        data.gns_global.poll_callbacks();

        data.clients.retain_mut(|client| client.tick());
        data.servers.retain_mut(|server| server.tick());

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

                gns_global.utils().enable_debug_output(
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

                Ok::<_, ()>(SubsystemData {
                    gns_global,
                    next_client_id: AtomicU64::new(1),
                    clients: Vec::new(),
                    msg_create_client_recv,
                    next_server_id: AtomicU64::new(1),
                    servers: Vec::new(),
                    msg_create_server_recv,
                })
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

// TODO: These tests can't be ran due to issues with multiple local GNS client sockets
//   See: https://github.com/hussein-aitlahcen/gns-rs/issues/12
//   Some of them could theoretically be ran in sequence, as long as none of them are ran in
//   parallel to each other.
#[cfg(test)]
mod tests {
    use crate::net::driver::tests::{
        ClientServerStackFactory,
        test_net_driver_client_server_core,
        test_net_driver_client_server_send,
        test_net_driver_client_server_send_to,
        test_net_driver_client_server_broadcast,
    };

    use super::*;

    #[derive(Default)]
    struct GnsStackFactory;

    impl ClientServerStackFactory for GnsStackFactory {
        type ClientDriver = GnsClientDriver;
        type ClientDriverFactory = Arc<GnsSubsystem>;
        type ServerDriver = GnsServerDriver;
        type ServerDriverFactory = Arc<GnsSubsystem>;

        #[inline]
        fn client_factory(&self) -> Self::ClientDriverFactory {
            GnsSubsystem::get()
        }

        #[inline]
        fn server_factory(&self) -> Self::ServerDriverFactory {
            GnsSubsystem::get()
        }
    }
    
    test_net_driver_client_server_core! {
        name: gns,
        stack_factory: GnsStackFactory,
        timeout: 2.0,
    }
    test_net_driver_client_server_send! {
        name: gns,
        stack_factory: GnsStackFactory,
        timeout: 2.0,
    }
    test_net_driver_client_server_send_to! {
        name: gns,
        stack_factory: GnsStackFactory,
        timeout: 2.0,
    }
    test_net_driver_client_server_broadcast! {
        name: gns,
        stack_factory: GnsStackFactory,
        timeout: 2.0,
    }
}