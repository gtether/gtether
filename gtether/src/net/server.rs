//! Server-side networking APIs.

use async_trait::async_trait;
use flagset::FlagSet;
use smol::future;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crate::event::{EventBus, EventBusRegistry};
use crate::net::message::{Message, MessageDispatchError, MessageFlags, MessageRecv, MessageRepliable, MessageReplyFuture, MessageSend};
use crate::net::{DisconnectReason, NetworkingBuildError, NetworkingBuilder};
use crate::net::message::dispatch::{MessageDispatch, MessageDispatchServer};
use crate::net::message::server::ServerMessageHandler;

/// An opaque ID type representing a single client-server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Connection(u32);

impl Display for Connection {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "connection-{}", self.0)
    }
}

/// Event describing when a client connects to a [ServerNetworking].
#[derive(Debug)]
pub struct ServerNetworkingConnectEvent {
    connection: Connection,
}

impl ServerNetworkingConnectEvent {
    #[inline]
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
        }
    }

    /// The [Connection] of the client that has connected.
    #[inline]
    pub fn connection(&self) -> Connection {
        self.connection
    }
}

/// Event describing when a client disconnects from a [ServerNetworking].
#[derive(Debug)]
pub struct ServerNetworkingDisconnectEvent {
    connection: Connection,
    reason: DisconnectReason,
}

impl ServerNetworkingDisconnectEvent {
    #[inline]
    pub fn new(connection: Connection, reason: DisconnectReason) -> Self {
        Self {
            connection,
            reason,
        }
    }

    /// The [Connection] of the client that disconnected.
    #[inline]
    pub fn connection(&self) -> Connection {
        self.connection
    }

    /// Reason for the disconnected occurring.
    #[inline]
    pub fn reason(&self) -> DisconnectReason {
        self.reason
    }
}

/// Server-side raw API.
///
/// This API is implemented by the networking stack, and a reference is given to
/// [RawServerFactories](RawServerFactory) in order to provide an interface for raw implementations
/// to access the networking stack's logic.
pub trait RawServerApi: Send + Sync + 'static {
    /// Generate and initialize a new [Connection].
    fn init_connection(&self) -> Connection;

    /// Dispatch raw message data to the networking stack for processing.
    fn dispatch_message(
        &self,
        connection: Connection,
        msg: &[u8],
    ) -> Result<(), MessageDispatchError>;

    /// Access to the [EventBus] to fire server-networking events.
    fn event_bus(&self) -> &EventBus;
}

/// Raw server listening on a given socket.
///
/// A RawServer can contain and manage many client connections for a single socket.
#[async_trait]
pub trait RawServer: Send + Sync + 'static {
    fn send(
        &self,
        connection: Connection,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), ServerNetworkingError>;

    fn broadcast(
        &self,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), ServerNetworkingError>;

    /// Close the server and all connections it has.
    ///
    /// After being closed, this server can no longer be used. This method should be idempotent, and
    /// calling it multiple times should result in successive calls being NoOps.
    async fn close(&self);
}

/// Factory for [RawServers](RawServer).
///
/// This factory generates server-side connection managers represented by [RawServer].
///
/// Note that `RawServerFactory` is implemented for `Box<dyn RawServerFactory>`, so that boxes can
/// be used as a concrete factory type.
#[async_trait]
pub trait RawServerFactory: Send + Sync + 'static {
    /// Listen on a given [SocketAddr].
    ///
    /// If the socket is free and can be bound, yields a new [RawServer].
    async fn listen(
        &self,
        api: Arc<dyn RawServerApi>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawServer>, ServerNetworkingError>;
}

#[async_trait]
impl RawServerFactory for Box<dyn RawServerFactory> {
    #[inline]
    async fn listen(
        &self,
        api: Arc<dyn RawServerApi>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawServer>, ServerNetworkingError> {
        (**self).listen(api, socket_addr).await
    }
}

/// Error that can occur during server networking logic.
#[derive(Debug)]
pub enum ServerNetworkingError {
    /// An error internal to the system happened.
    ///
    /// Internal errors are generally non-recoverable, and represent something going wrong with the
    /// internal logic of the implementation.
    InternalError(String),

    /// The socket could not be bound for listening.
    InvalidListen(SocketAddr),

    /// The specified connection was invalid or not found.
    InvalidConnection(Connection),

    /// A message failed to send because it could not be serialized.
    MalformedMessage(Box<dyn Error + Send + Sync>),

    /// An operation was attempted on a server manager that has already been closed.
    Closed,
}

impl Display for ServerNetworkingError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InternalError(msg) => write!(f, "Internal Error: {msg}"),
            Self::InvalidListen(socket_addr) =>
                write!(f, "Cannot listen on: {socket_addr}"),
            Self::InvalidConnection(connection) =>
                write!(f, "Invalid connection: {connection}"),
            Self::MalformedMessage(err) =>
                write!(f, "Malformed message; {err}"),
            Self::Closed => write!(f, "Server has been closed"),
        }
    }
}

impl Error for ServerNetworkingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MalformedMessage(err) => Some(err.deref()),
            _ => None,
        }
    }
}

impl ServerNetworkingError {
    /// Helper for creating an [InternalError](Self::InternalError) from a string.
    #[inline]
    pub fn internal_error(msg: impl Into<String>) -> Self {
        Self::InternalError(msg.into())
    }
}

impl From<ump::Error<ServerNetworkingError>> for ServerNetworkingError {
    #[inline]
    fn from(value: ump::Error<Self>) -> Self {
        match value {
            ump::Error::App(err) => err,
            ump::Error::ClientsDisappeared | ump::Error::ServerDisappeared =>
                Self::Closed,
            ump::Error::NoReply =>
                Self::internal_error("No response from internal system"),
        }
    }
}

struct Api {
    next_connect_id: AtomicU32,
    msg_dispatch: MessageDispatch<MessageDispatchServer>,
    event_bus: EventBus,
}

impl Api {
    fn new() -> Self {
        let event_bus = EventBus::builder()
            .event_type::<ServerNetworkingConnectEvent>()
            .event_type::<ServerNetworkingDisconnectEvent>()
            .build();

        Self {
            next_connect_id: AtomicU32::new(0),
            msg_dispatch: MessageDispatch::new(),
            event_bus,
        }
    }
}

impl RawServerApi for Api {
    #[inline]
    fn init_connection(&self) -> Connection {
        Connection(self.next_connect_id.fetch_add(1, Ordering::SeqCst))
    }

    #[inline]
    fn dispatch_message(
        &self,
        connection: Connection,
        msg: &[u8],
    ) -> Result<(), MessageDispatchError> {
        self.msg_dispatch.dispatch(connection, msg)
    }

    #[inline]
    fn event_bus(&self) -> &EventBus {
        &self.event_bus
    }
}

/// Manager for server networking logic.
///
/// An instance of ServerNetworking manages an internal [RawServer]. This internal server may
/// handle many client connections.
pub struct ServerNetworking {
    api: Arc<Api>,
    inner: Arc<dyn RawServer>,
    next_msg_num: AtomicU64,
}

impl ServerNetworking {
    /// Create a builder for [ServerNetworking].
    ///
    /// This is the recommended way to create a new [ServerNetworking] stack.
    #[inline]
    pub fn builder() -> NetworkingBuilder<ServerBuilder> {
        NetworkingBuilder::new()
    }

    fn new(
        raw_factory: impl RawServerFactory,
        socket_addr: SocketAddr,
    ) -> Result<Self, ServerNetworkingError> {
        let api = Arc::new(Api::new());

        let inner = future::block_on(raw_factory.listen(
            api.clone(),
            socket_addr,
        ))?;

        Ok(Self {
            api,
            inner,
            next_msg_num: AtomicU64::new(1),
        })
    }

    /// [EventBusRegistry] for [ServerNetworking] events.
    ///
    /// These include e.g.:
    ///  * [ServerNetworkingConnectEvent]
    ///  * [ServerNetworkingDisconnectEvent]
    #[inline]
    pub fn event_bus(&self) -> &EventBusRegistry {
        self.api.event_bus.registry()
    }

    /// Insert a message handler into this [ServerNetworking] stack.
    ///
    /// The handler is associated with the message type's [key](message::MessageBody::KEY), and
    /// inserting multiple handlers with the same key will override the old one.
    pub fn insert_msg_handler<M, E>(
        &self,
        handler: impl ServerMessageHandler<M, E>
    )
    where
        M: MessageRecv,
        E: Error + Send + Sync + 'static,
    {
        self.api.msg_dispatch.insert_handler(handler)
    }

    /// Send a [Message] from this server to a connected client.
    ///
    /// Does not block, and will yield as soon as the message is passed off to the networking stack.
    ///
    /// # Errors
    ///
    /// Errors if the provided [Message] could not be serialized, or if the provided [Connection] is
    /// invalid.
    pub fn send<M>(
        &self,
        connection: Connection,
        msg: impl Into<Message<M>>,
    ) -> Result<(), ServerNetworkingError>
    where
        M: MessageSend,
    {
        let mut msg = msg.into();
        msg.set_msg_num(self.next_msg_num.fetch_add(1, Ordering::SeqCst).try_into().unwrap());
        let bytes = msg.encode()
            .map_err(|err| ServerNetworkingError::MalformedMessage(Box::new(err)))?;
        self.inner.send(connection, bytes, M::flags())
    }

    pub fn broadcast<M>(
        &self,
        msg: impl Into<Message<M>>,
    ) -> Result<(), ServerNetworkingError>
    where
        M: MessageSend,
    {
        let mut msg = msg.into();
        msg.set_msg_num(self.next_msg_num.fetch_add(1, Ordering::SeqCst).try_into().unwrap());
        let bytes = msg.encode()
            .map_err(|err| ServerNetworkingError::MalformedMessage(Box::new(err)))?;
        self.inner.broadcast(bytes, M::flags())
    }

    /// Send a [Message] from this server to a connected client, and wait for a response.
    ///
    /// Does not block. Will yield a [MessageReplyFuture] that can be used to wait for a response
    /// either synchronously or asynchronously.
    ///
    /// # Errors
    ///
    /// Errors if the provided [Message] could not be serialized, or if the provided [Connection] is
    /// invalid.
    pub fn send_recv<M>(
        &self,
        connection: Connection,
        msg: impl Into<Message<M>>,
    ) -> Result<MessageReplyFuture<M::Reply>, ServerNetworkingError>
    where
        M: MessageSend + MessageRepliable,
        M::Reply: MessageRecv + Send + 'static,
    {
        let mut msg = msg.into();
        msg.set_msg_num(self.next_msg_num.fetch_add(1, Ordering::SeqCst).try_into().unwrap());
        let bytes = msg.encode()
            .map_err(|err| ServerNetworkingError::MalformedMessage(Box::new(err)))?;
        let fut = self.api.msg_dispatch.register_reply(&msg);
        self.inner.send(connection, bytes, M::flags())?;
        Ok(fut)
    }
}

impl Drop for ServerNetworking {
    fn drop(&mut self) {
        future::block_on(self.inner.close())
    }
}

impl From<ServerNetworkingError> for NetworkingBuildError {
    #[inline]
    fn from(value: ServerNetworkingError) -> Self {
        Self::InitError(Box::new(value))
    }
}

/// Builder sub-pattern for [ServerNetworking] stack.
///
/// This builder cannot be used directly, and is only used as a generic type in [NetworkingBuilder].
/// To get a builder using this, see [ServerNetworking::builder()].
#[derive(Default)]
pub struct ServerBuilder {
    raw_factory: Option<Box<dyn RawServerFactory>>,
    ip_addr: Option<IpAddr>,
    port: Option<u16>,
}

impl NetworkingBuilder<ServerBuilder> {
    /// Set the [RawServerFactory] implementation for creating the [RawServer].
    ///
    /// This option is required.
    #[inline]
    pub fn raw_factory(mut self, raw_factory: impl RawServerFactory) -> Self {
        self.extra.raw_factory = Some(Box::new(raw_factory));
        self
    }

    /// Set the IP address that this server listens on.
    ///
    /// Default is [Ipv4Addr::LOCALHOST].
    #[inline]
    pub fn ip_addr(mut self, ip_addr: impl Into<IpAddr>) -> Self {
        self.extra.ip_addr = Some(ip_addr.into());
        self
    }

    /// Set the port that this server listens on.
    ///
    /// This option is required.
    #[inline]
    pub fn port(mut self, port: u16) -> Self {
        self.extra.port = Some(port);
        self
    }

    /// Set the socket address that this server listens on.
    ///
    /// This is effectively the same as:
    /// ```no_run
    /// # use std::net::{Ipv4Addr, SocketAddr};
    /// # use gtether::net::server::ServerNetworking;
    /// # let socket = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9001);
    /// ServerNetworking::builder()
    ///     .ip_addr(socket.ip())
    ///     .port(socket.port())
    /// # ;
    /// ```
    #[inline]
    pub fn socket_addr(mut self, socket_addr: impl Into<SocketAddr>) -> Self {
        let socket_addr = socket_addr.into();
        self.extra.ip_addr = Some(socket_addr.ip());
        self.extra.port = Some(socket_addr.port());
        self
    }

    /// Build this [ServerNetworking] stack.
    ///
    /// # Errors
    ///
    /// Errors when there is a required option that is missing, or if the networking stack fails to
    /// initialize.
    pub fn build(self) -> Result<Arc<ServerNetworking>, NetworkingBuildError> {
        let raw_factory = self.extra.raw_factory
            .ok_or(NetworkingBuildError::missing_option("raw_factory"))?;

        let ip_addr = self.extra.ip_addr
            .unwrap_or(Ipv4Addr::LOCALHOST.into());
        let port = self.extra.port
            .ok_or(NetworkingBuildError::missing_option("port"))?;
        let socket_addr = SocketAddr::new(ip_addr, port);

        Ok(Arc::new(ServerNetworking::new(raw_factory, socket_addr)?))
    }
}