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

use async_trait::async_trait;
use bimap::BiHashMap;
use educe::Educe;
use flagset::FlagSet;
use gns::sys::{k_nSteamNetworkingSend_Reliable, ESteamNetworkingConnectionState as NetConnectState};
use gns::{GnsConnection, GnsGlobal, GnsSocket, GnsUtils, IsClient, IsServer};
use image::EncodableLayout;
use itertools::Either;
use ouroboros::self_referencing;
use parking_lot::RwLock;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::net::client::{ClientNetworkingError, RawClient, RawClientApi, RawClientFactory};
use crate::net::message::MessageFlags;
use crate::net::server::{Connection, RawServer, RawServerApi, RawServerFactory, ServerNetworkingError};
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
    api: Arc<dyn RawClientApi>,
    socket: Option<GnsSocket<'gu, 'gu, IsClient>>,
    msg_recv: ump::Server<ClientMessage, (), ClientNetworkingError>,
}

impl<'gu> GnsClient<'gu> {
    fn new(
        gns_global: &'gu GnsGlobal,
        gns_utils: &'gu GnsUtils,
        api: Arc<dyn RawClientApi>,
        socket_addr: SocketAddr,
    ) -> Result<(Self, Arc<dyn RawClient>), ClientNetworkingError> {
        let socket = GnsSocket::new(gns_global, gns_utils)
            .ok_or(ClientNetworkingError::internal_error("Failed to create socket"))?
            .connect(socket_addr.ip(), socket_addr.port())
            .map_err(|_| ClientNetworkingError::InvalidConnection(socket_addr))?;

        let (msg_recv, msg_send) = ump::channel();

        let client = Self {
            api,
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

        socket.poll_messages::<100>(|message| {
            match self.api.dispatch_message(&mut message.payload()) {
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

struct GnsClientHandle {
    msg_send: ump::Client<ClientMessage, (), ClientNetworkingError>,
}

#[async_trait]
impl RawClient for GnsClientHandle {
    #[inline]
    fn send(
        &self,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), ClientNetworkingError> {
        self.msg_send.req_async(ClientMessage::Send { msg, flags })
            // We're firing and forgetting, so we don't care about the WaitReply<>
            .map(|_| ())
            .map_err(ClientNetworkingError::from)
    }

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
    Send {
        connection: Connection,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    },
    Close,
}

type ClientsMap = BiHashMap<GnsConnection, Connection, ahash::RandomState, ahash::RandomState>;

struct GnsServer<'gu> {
    api: Arc<dyn RawServerApi>,
    socket: Option<GnsSocket<'gu, 'gu, IsServer>>,
    clients: Arc<RwLock<ClientsMap>>,
    msg_recv: ump::Server<ServerMessage, (), ServerNetworkingError>,
}

impl<'gu> GnsServer<'gu> {
    fn new(
        gns_global: &'gu GnsGlobal,
        gns_utils: &'gu GnsUtils,
        api: Arc<dyn RawServerApi>,
        socket_addr: SocketAddr,
    ) -> Result<(Self, Arc<dyn RawServer>), ServerNetworkingError> {
        let socket = GnsSocket::new(gns_global, gns_utils)
            .ok_or(ServerNetworkingError::internal_error("Failed to create socket"))?
            .listen(socket_addr.ip(), socket_addr.port())
            .map_err(|_| ServerNetworkingError::InvalidListen(socket_addr))?;

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

        let server_handle = Arc::new(GnsServerHandle {
            msg_send,
            clients,
        });

        Ok((server, server_handle))
    }

    fn close(&mut self) {
        // Take and drop the socket
        if let Some(socket) = self.socket.take() {
            let mut clients = self.clients.write();
            clients.retain(|connection, _| {
                socket.close_connection(*connection, 0, "", false);
                false
            });
            socket.poll_callbacks();
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
                ServerMessage::Send { connection, msg, flags } => {
                    msgs_to_send.push((connection, msg, flags))
                },
                ServerMessage::Close => {
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
                            let api_connection = self.api.init_connection();
                            let mut clients = self.clients.write();
                            clients.insert(connection, api_connection);
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
                    let mut clients = self.clients.write();
                    clients.remove_by_left(&connection);
                    socket.close_connection(connection, 0, "", false);
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

        true
    }
}

struct GnsServerHandle {
    msg_send: ump::Client<ServerMessage, (), ServerNetworkingError>,
    clients: Arc<RwLock<ClientsMap>>,
}

#[async_trait]
impl RawServer for GnsServerHandle {
    fn send(
        &self,
        connection: Connection,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), ServerNetworkingError> {
        let clients = self.clients.read();
        if clients.contains_right(&connection) {
            self.msg_send.req_async(ServerMessage::Send { connection, msg, flags })
                .map(|_| ())
                .map_err(ServerNetworkingError::from)
        } else {
            Err(ServerNetworkingError::InvalidConnection(connection))
        }
    }

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

#[derive(Educe)]
#[educe(Debug)]
struct CreateClientReq {
    socket_addr: SocketAddr,
    #[educe(Debug(ignore))]
    api: Arc<dyn RawClientApi>,
}

#[derive(Educe)]
#[educe(Debug)]
struct CreateServerReq {
    socket_addr: SocketAddr,
    #[educe(Debug(ignore))]
    api: Arc<dyn RawServerApi>,
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
    msg_create_client_recv: ump::Server<CreateClientReq, Arc<dyn RawClient>, ClientNetworkingError>,
    msg_create_server_recv: ump::Server<CreateServerReq, Arc<dyn RawServer>, ServerNetworkingError>,
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
    msg_create_client_send: ump::Client<CreateClientReq, Arc<dyn RawClient>, ClientNetworkingError>,
    msg_create_server_send: ump::Client<CreateServerReq, Arc<dyn RawServer>, ServerNetworkingError>,
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
    async fn connect(
        &self,
        api: Arc<dyn RawClientApi>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawClient>, ClientNetworkingError> {
        let req = CreateClientReq {
            socket_addr,
            api,
        };
        self.msg_create_client_send.areq(req).await
            .map_err(|err| match err {
                ump::Error::App(err) => err,
                _ => ClientNetworkingError::internal_error("No response from subsystem"),
            })
    }
}

#[async_trait]
impl RawServerFactory for Arc<GnsSubsystem> {
    async fn listen(
        &self,
        api: Arc<dyn RawServerApi>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawServer>, ServerNetworkingError> {
        let req = CreateServerReq {
            socket_addr,
            api,
        };
        self.msg_create_server_send.areq(req).await
            .map_err(|err| match err {
                ump::Error::App(err) => err,
                _ => ServerNetworkingError::internal_error("No response from subsystem"),
            })
    }
}