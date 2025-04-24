//! Networking logic.
//!
//! This module contains logic related to game networking. Rather than directly using a third-party
//! library for networking, this engine provides its own API for game networking, and hooks into
//! a third-party library underneath. This allows the underlying library to be swapped without
//! needing to restructure any code.
//!
//! gTether provides an implementation of the API using
//! [Game Networking Sockets](https://github.com/ValveSoftware/GameNetworkingSockets), but further
//! implementations can be easily made by implementing the provided interfaces.
//!
//! # Stack layers
//!
//! The networking logic in this module primarily concerns the host layers of the
//! [OSI model](https://en.wikipedia.org/wiki/OSI_model) (layers 4-7). The breakout of these layers
//! can be seen in the following:
//!
//! | Layer                      | Relevant Structs |
//! | -------------------------- | ---------------- |
//! | 7 - Application            | [Message]        |
//! | 5/6 - Session/Presentation | [Networking]     |
//! | 4/5 - Transport/Session    | [NetDriver]      |
//!
//! ## Application layer
//!
//! The application layer is represented by [Message] structs, which contain an engine-managed
//! [MessageHeader](message::MessageHeader) and a user-defined [MessageBody](message::MessageBody).
//! Users can specify any amount of custom MessageBodies, as long as they register
//! [handlers](MessageHandler) for receiving said messages with the [Networking] stack.
//!
//! ## Session/Presentation layers
//!
//! The user-side session and presentation layers are primarily managed by the [Networking] stack
//! struct. This struct handles the presentation of application [Messages](Message) to the session
//! layer via [`send()`](Networking::send) and [`send_to()`](Networking::send_to) methods. It also
//! handles user-side session management, and is responsible for creation new
//! [Connections](Connection).
//!
//! ## Transport/Session layers
//!
//! The engine does not do transport layer handling itself. Instead, it delegates that to other
//! implementations, which it interacts with [NetDriver] traits. These traits describe what
//! capabilities a custom "driver" implementation can handle, and are used to expose the relevant
//! methods in the [Networking] struct.
//!
//! In order for [NetDriver] implementations to interact with the upper layers, a [NetworkingApi]
//! struct is usually given to the implementation when created via [`NetDriverFactory::create()`].
//! This NetworkingApi struct provides methods that allow interaction with the [Networking] struct,
//! such as [`NetworkingApi::dispatch_message()`] for passing along raw message data.
//!
//! In addition, [NetDriver] implementations will likely need to manage sessions to an extent, in
//! order to notify the [NetworkingApi] when incoming connections are requested as well as track
//! where incoming and outgoing messages need to be directed to.
//!
//! Note that standard engine users do not need to interact with [NetworkingApi] directly, unless
//! you are implementing a custom [NetDriver].

use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use arrayvec::ArrayVec;
use driver::NetDriverFactory;
use parking_lot::{Mutex, RwLock};
use smol::future;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::event::{EventBus, EventBusRegistry, EventCancellable};
use crate::net::driver::{
    NetDriver,
    NetDriverBroadcast,
    NetDriverCloseConnection,
    NetDriverConnect,
    NetDriverListen,
    NetDriverSend,
    NetDriverSendTo,
};
use crate::net::message::dispatch::{InterceptMessageHandler, MessageDispatch};
use crate::net::message::{
    Message,
    MessageDispatchError,
    MessageHandler,
    MessageHeader,
    MessageRecv,
    MessageRepliable,
    MessageReplyFuture,
    MessageSend,
};

pub mod driver;
pub mod gns;
pub mod message;

/// An opaque ID type representing a single endpoint-to-endpoint connection.
///
/// Connections are created and managed by the [Networking] stack. This ID type can only be used to
/// identify, and does not contain detailed information about the connection. Instead, detailed
/// information can be found by querying the Networking stack via [`Networking::connection_info()`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Connection(Option<NonZeroU32>);

impl Connection {
    /// Invalid Connection ID.
    ///
    /// Intended for use in e.g. limited-scope testing where real Connection IDs are not required
    /// and/or not feasible to create.
    pub const INVALID: Self = Connection(None);
}

impl Display for Connection {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(id) => write!(f, "connection-{id}"),
            None => write!(f, "connection-INVALID"),
        }
    }
}

/// Detailed information about a given [Connection].
///
/// Can be retrieved via querying the [Networking] stack using [`Networking::connection_info()`].
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    connection: Connection,
    remote_addr: IpAddr,
}

impl ConnectionInfo {
    /// The [Connection] associated with this ConnectionInfo.
    #[inline]
    pub fn connection(&self) -> Connection {
        self.connection
    }

    /// The remote address that is connected.
    #[inline]
    pub fn remote_addr(&self) -> IpAddr {
        self.remote_addr
    }
}

/// Event describing when a [Connection] is about to be created.
///
/// This event is cancellable, which will prevent the connection from occurring.
///
/// Note that this event is fired _before_ the connection actually occurs.
///
/// ```
/// # use gtether::net::Networking;
/// # use gtether::net::driver::NoNetDriver;
/// # use gtether::event::InvalidTypeIdError;
/// use gtether::event::Event;
/// use gtether::net::NetworkingConnectEvent;
/// #
/// # let networking = Networking::new(NoNetDriver::new());
///
/// let deny_list = vec![
///     /* IP addresses we don't want to connect to */
/// ];
///
/// networking.event_bus().register(move |event: &mut Event<NetworkingConnectEvent>| {
///     if deny_list.contains(&event.connection_info().remote_addr()) {
///         event.cancel();
///     }
/// })?;
/// #
/// # Ok::<_, InvalidTypeIdError>(())
/// ```
#[derive(Debug)]
pub struct NetworkingConnectEvent {
    connection: Connection,
    connection_info: ConnectionInfo,
}
impl EventCancellable for NetworkingConnectEvent {}

impl NetworkingConnectEvent {
    #[inline]
    pub fn new(connection: Connection, connection_info: ConnectionInfo) -> Self {
        Self {
            connection,
            connection_info,
        }
    }

    /// The [Connection] that has connected.
    #[inline]
    pub fn connection(&self) -> Connection {
        self.connection
    }

    /// Detailed information about the attempted connection.
    #[inline]
    pub fn connection_info(&self) -> &ConnectionInfo {
        &self.connection_info
    }
}

/// Reason for a disconnect to occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisconnectReason {
    /// The connection was terminated locally by the user.
    ClosedLocally,
    /// The connection was terminated by the connected peer.
    ClosedByPeer,
    /// The connection was unexpectedly dropped.
    Unexpected,
}

impl Display for DisconnectReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClosedLocally => write!(f, "Connection closed gracefully"),
            Self::ClosedByPeer => write!(f, "Connection closed by peer"),
            Self::Unexpected => write!(f, "Unexpected disconnect occurred"),
        }
    }
}

/// Event describing when a [Connection] disconnects.
///
/// Note that this event is fired _after_ the connection is cleaned up.
///
/// ```
/// # use gtether::net::Networking;
/// # use gtether::net::driver::NoNetDriver;
/// # use gtether::event::InvalidTypeIdError;
/// use gtether::event::Event;
/// use gtether::net::NetworkingDisconnectEvent;
/// #
/// # let networking = Networking::new(NoNetDriver::new());
///
/// networking.event_bus().register(move |event: &mut Event<NetworkingDisconnectEvent>| {
///     println!(
///         "Connection disconnected: {:?}, reason: {}",
///         event.connection_info(),
///         event.reason(),
///     );
/// })?;
/// #
/// # Ok::<_, InvalidTypeIdError>(())
/// ```
#[derive(Debug)]
pub struct NetworkingDisconnectEvent {
    connection: Connection,
    connection_info: ConnectionInfo,
    reason: DisconnectReason,
}

impl NetworkingDisconnectEvent {
    #[inline]
    pub fn new(
        connection: Connection,
        connection_info: ConnectionInfo,
        reason: DisconnectReason,
    ) -> Self {
        Self {
            connection,
            connection_info,
            reason,
        }
    }

    /// The [Connection] that disconnected.
    #[inline]
    pub fn connection(&self) -> Connection {
        self.connection
    }

    /// Detailed information about the connection.
    #[inline]
    pub fn connection_info(&self) -> &ConnectionInfo {
        &self.connection_info
    }

    /// Reason for the disconnected occurring.
    #[inline]
    pub fn reason(&self) -> DisconnectReason {
        self.reason
    }
}

/// Errors that can occur when working with a [Networking] stack.
#[derive(Debug, thiserror::Error)]
pub enum NetworkingError {
    #[error("Internal Error: {0}")]
    InternalError(String),
    #[error("Invalid socket address: {0}")]
    InvalidAddress(SocketAddr),
    #[error("Invalid connection: {0}")]
    InvalidConnection(Connection),
    #[error("The operation was cancelled by the user.")]
    Cancelled,
    #[error("Malformed message; {0}")]
    MalformedMessage(#[source] Box<dyn Error + Send + Sync>),
    #[error("Socket has been closed.")]
    Closed,
}

impl NetworkingError {
    /// Helper for creating an [InternalError](Self::InternalError) from a string.
    #[inline]
    pub fn internal_error(msg: impl Into<String>) -> Self {
        Self::InternalError(msg.into())
    }
}

impl From<ump::Error<NetworkingError>> for NetworkingError {
    #[inline]
    fn from(value: ump::Error<NetworkingError>) -> Self {
        match value {
            ump::Error::App(err) => err,
            ump::Error::ClientsDisappeared | ump::Error::ServerDisappeared => Self::Closed,
            ump::Error::NoReply => Self::internal_error("No response from internal system"),
        }
    }
}

/// API for interacting with the engine [Networking] layer.
///
/// This API is implemented by the engine's [Networking] stack layer, and a reference is given to
/// [NetDrivers](NetDriver) in order to provide an interface for drivers to access the engine's
/// networking logic.
///
/// This API is _not_ intended to be directly accessed by users.
pub struct NetworkingApi {
    event_bus: EventBus,
    msg_dispatch: Arc<MessageDispatch>,
    next_connect_id: AtomicU32,
    connection_infos: RwLock<HashMap<Connection, ConnectionInfo>>,
}

impl NetworkingApi {
    fn new() -> Arc<Self> {
        let event_bus = EventBus::builder()
            .event_type::<NetworkingConnectEvent>()
            .event_type::<NetworkingDisconnectEvent>()
            .build();
        let msg_dispatch = Arc::new(MessageDispatch::new());
        let next_connect_id = AtomicU32::new(1);
        let connection_infos = RwLock::new(HashMap::new());

        Arc::new(Self {
            event_bus,
            msg_dispatch,
            next_connect_id,
            connection_infos,
        })
    }

    /// Attempt to initiate a [Connection].
    ///
    /// Will fire a [NetworkingConnectEvent]. If that event is cancelled, this will return `None`.
    /// If the event is _not_ cancelled, this will return the created [Connection].
    ///
    /// This method does _NOT_ actually execute any connection logic, but only initializes the
    /// relevant [ConnectionInfo] for a [Networking] stack. This method should always be called by
    /// the [NetDriver] when a connection is being created, either outgoing or incoming. If it
    /// yields a [Connection], it is the driver's responsibility to actually create and manage that
    /// connection.
    #[inline]
    pub fn try_init_connect(&self, remote_addr: IpAddr) -> Option<Connection> {
        let connection = Connection(Some(self.next_connect_id.fetch_add(1, Ordering::SeqCst)
            .try_into().unwrap()));
        let connection_info = ConnectionInfo {
            connection,
            remote_addr,
        };

        let event = self.event_bus.fire(NetworkingConnectEvent::new(
            connection,
            connection_info.clone(),
        ));

        if !event.is_cancelled() {
            self.connection_infos.write().insert(connection, connection_info);
            Some(connection)
        } else {
            None
        }
    }

    /// Signal that a [Connection] has disconnected.
    ///
    /// Will fire a [NetworkingDisconnectEvent].
    ///
    /// The [NetDriver] should call this method when any connection terminates, either expected or
    /// unexpected. Calling this method multiple times for the same connection is idempotent, and
    /// only the first will generate an event, though this should still be avoided if possible.
    #[inline]
    pub fn disconnect(&self, connection: Connection, reason: DisconnectReason) {
        if let Some(connection_info) = self.connection_infos.write().remove(&connection) {
            self.event_bus.fire(NetworkingDisconnectEvent::new(connection, connection_info, reason));
        } else {
            warn!(?connection, ?reason, "Attempted to disconnect an already disconnected connection");
        }
    }

    /// Dispatch an incoming message.
    ///
    /// The [NetDriver] should call this method when it receives an incoming message. The
    /// [Networking] stack will then parse and handle the raw message data. The `connection`
    /// parameter should be the remote connection that sent the message.
    #[inline]
    pub fn dispatch_message(
        &self,
        connection: Connection,
        msg: &[u8],
    ) -> Result<(), MessageDispatchError> {
        self.msg_dispatch.dispatch(connection, msg)
    }
}

/// Networking stack for managing OSI host layers 5 (session) and 6 (presentation).
///
/// This structure maintains user-side networking sessions and connections. It also manages
/// the presentation layer, by accepting layer 7 (application) [Messages](message::Message) to
/// present downwards to layer 4 (transport) and below.
///
/// See [module-level](super::net) documentation for more details.
pub struct Networking<D: NetDriver> {
    driver: Arc<D>,
    api: Arc<NetworkingApi>,
}

impl<D: NetDriver> Networking<D> {
    /// Create a new Networking stack using a particular [NetDriver] implementation.
    pub fn new(driver_factory: impl NetDriverFactory<D>) -> Self {
        let api = NetworkingApi::new();
        let driver = driver_factory.create(api.clone());

        Self {
            driver,
            api,
        }
    }

    /// Get the [EventBusRegistry] to register event handlers.
    ///
    /// Available events:
    ///  * [NetworkingConnectEvent]
    ///  * [NetworkingDisconnectEvent]
    #[inline]
    pub fn event_bus(&self) -> &EventBusRegistry {
        self.api.event_bus.registry()
    }

    /// Insert a message handler into this [Networking] stack.
    ///
    /// The handler is associated with the message type's [key](message::MessageBody::KEY), and
    /// inserting multiple handlers with the same key will override the old one.
    #[inline]
    pub fn insert_msg_handler<M, E>(
        &self,
        handler: impl MessageHandler<M, E>,
    )
    where
        M: MessageRecv,
        E: Error + Send + Sync + 'static,
    {
        self.api.msg_dispatch.insert_handler(handler);
    }

    /// Retrieve detailed [ConnectionInfo] for a particular [Connection].
    #[inline]
    pub fn connection_info(&self, connection: &Connection) -> Option<ConnectionInfo> {
        self.api.connection_infos.read().get(connection).cloned()
    }

    /// Close the [Networking] stack.
    ///
    /// Closes the stack if it is open (e.g. has been [connected](Networking::connect) or
    /// [listened](Networking::listen)). It is likely that any future operations will yield
    /// [NetworkingError::Closed] until the stack is reset.
    ///
    /// Note that the [Networking] stack will automatically close itself when dropped, so it is
    /// not necessary to explicitly call this before dropping the stack.
    #[inline]
    pub async fn close(&self) {
        self.driver.close().await;
    }

    /// Convenience wrapper for a sync version of [Self::close()].
    #[inline]
    pub fn close_sync(&self) {
        future::block_on(self.close());
    }
}

impl<D: NetDriver> Drop for Networking<D> {
    #[inline]
    fn drop(&mut self) {
        self.close_sync();
    }
}

// TODO: does this need to be configurable?
const CONNECT_BUFFER_SIZE: usize = 64;

struct ConnectBuffer {
    connection: Connection,
    buffer: Mutex<ArrayVec<(MessageHeader, Box<[u8]>), CONNECT_BUFFER_SIZE>>,
    active_replies: Mutex<HashSet<NonZeroU64>>,
    msg_dispatch: Arc<MessageDispatch>,
}

impl ConnectBuffer {
    fn new(connection: Connection, msg_dispatch: Arc<MessageDispatch>) -> Self {
        Self {
            connection,
            buffer: Mutex::new(ArrayVec::new()),
            active_replies: Mutex::new(HashSet::new()),
            msg_dispatch,
        }
    }
}

impl InterceptMessageHandler for ConnectBuffer {
    fn accept(
        &self,
        connection: Connection,
        msg_header: &MessageHeader,
        msg_body: &[u8],
    ) -> Result<bool, MessageDispatchError> {
        if connection == self.connection {
            if let Some(reply_num) = msg_header.reply_num() {
                if self.active_replies.lock().remove(&reply_num) {
                    // Let the message through
                    return Ok(false)
                }
            }

            let entry = (
                msg_header.clone(),
                Box::from(msg_body),
            );
            match self.buffer.lock().try_push(entry) {
                Ok(_) => Ok(true),
                Err(_) => Err(MessageDispatchError::DispatchFailed(Box::from(
                    "ConnectBuffer is full".to_owned()
                ))),
            }
        } else {
            // Message not intended for this connection, let it through
            Ok(false)
        }
    }

    fn register_reply(&self, msg_header: &MessageHeader) -> bool {
        let msg_num = msg_header.msg_num()
            .expect("msg_num should already be set");
        self.active_replies.lock().insert(msg_num);
        true
    }
}

impl Drop for ConnectBuffer {
    fn drop(&mut self) {
        let mut buffer = self.buffer.lock();
        debug!(msg_count = buffer.len(), "Dispatching buffered ConnectContext messages");
        let range = 0..buffer.len();
        for (msg_header, msg_body) in buffer.drain(range) {
            match self.msg_dispatch.dispatch_parsed(self.connection, msg_header, &*msg_body) {
                Ok(_) => {},
                Err(error) =>
                    warn!(?error, "Failed to dispatch buffered ConnectContext message"),
            }
        }
    }
}

/// Context used while actively initializing a connection to an endpoint.
///
/// This context does not provide any active functionality itself, but while it exists the
/// [Networking] stack that created it will intercept and hold all incoming messages for the
/// initializing connection. This allows users to install any required message handlers or
/// coordinate handshakes before any incoming messages are dealt with.
///
/// When the context is dropped, all held messages will then be dispatched through the [Networking]
/// stack for regular processing.
///
/// There is a limit to how many messages can be held by the context. Currently, that limit is `64`,
/// but is subject to change. Any messages that arrive after that buffer is full will trigger a
/// [MessageDispatchError], and will likely be dropped.
pub struct ClientConnectContext(#[allow(unused)] Arc<ConnectBuffer>);

impl<D: NetDriverConnect> Networking<D> {
    /// Attempt to connect to the remote `socket_addr`.
    ///
    /// How many endpoints can be connected to is up to the particular driver. For example, a
    /// client/server architecture may only allow a client to connect to one server at a time,
    /// whereas a peer-to-peer architecture may allow clients to connect to many other clients at
    /// once. See the driver's documentation for more details.
    ///
    /// # Errors
    ///
    /// [NetworkingError::InvalidAddress] if the `socket_addr` could not be
    /// connected to.
    ///
    /// [NetworkingError::Cancelled] if the connection was cancelled by event handling.
    pub async fn connect(
        &self,
        socket_addr: SocketAddr,
    ) -> Result<ClientConnectContext, NetworkingError> {
        if let Some(connection) = self.api.try_init_connect(socket_addr.ip()) {
            let connect_buffer = Arc::new(ConnectBuffer::new(
                connection,
                self.api.msg_dispatch.clone(),
            ));
            self.api.msg_dispatch.insert_intercept_handler(&connect_buffer);
            let connect_ctx = ClientConnectContext(connect_buffer);
            self.driver.connect(connection, socket_addr).await?;
            Ok(connect_ctx)
        } else {
            Err(NetworkingError::Cancelled)
        }
    }

    /// Convenience wrapper for a sync version of [Self::connect()].
    #[inline]
    pub fn connect_sync(
        &self,
        socket_addr: SocketAddr,
    ) -> Result<ClientConnectContext, NetworkingError> {
        future::block_on(self.connect(socket_addr))
    }
}

impl<D: NetDriverCloseConnection> Networking<D> {
    /// Close the specified [Connection].
    ///
    /// This method is idempotent, and specifying a non-existing [Connection] ID will do nothing.
    pub async fn close_connection(
        &self,
        connection: Connection,
    ) {
        self.driver.close_connection(connection).await
    }
}

impl<D: NetDriverListen> Networking<D> {
    /// Bind to and listen on a socket.
    ///
    /// # Errors
    ///
    /// [NetworkingError::InvalidAddress] if the `socket_addr` could not be bound.
    pub async fn listen(
        &self,
        socket_addr: SocketAddr,
    ) -> Result<(), NetworkingError> {
        self.driver.listen(socket_addr).await
    }
}

impl<D: NetDriverSend> Networking<D> {
    /// Send a [Message].
    ///
    /// Messages are dispatched immediately. This method does not block or otherwise wait.
    ///
    /// # Errors
    ///
    /// [NetworkingError::MalformedMessage] if the [Message] couldn't be encoded
    /// into bytes.
    ///
    /// [NetworkingError::Closed] if the connection is closed.
    pub fn send<M>(
        &self,
        msg: impl Into<Message<M>>,
    ) -> Result<(), NetworkingError>
    where
        M: MessageSend,
    {
        let mut msg = msg.into();
        msg.set_msg_num(self.api.msg_dispatch.gen_msg_num());
        let bytes = msg.encode()?;
        self.driver.send(bytes, M::flags())
    }

    /// Send a [repliable](MessageRepliable) [Message] and get a [reply future](MessageReplyFuture).
    ///
    /// Messages are dispatched immediately. This method does not block or otherwise wait, however
    /// the returned [reply future](MessageReplyFuture) can be waited on.
    ///
    /// # Errors
    ///
    /// [NetworkingError::MalformedMessage] if the [Message] couldn't be encoded
    /// into bytes.
    ///
    /// [NetworkingError::Closed] if the connection is closed.
    pub fn send_recv<M>(
        &self,
        msg: impl Into<Message<M>>,
    ) -> Result<MessageReplyFuture<M::Reply>, NetworkingError>
    where
        M: MessageSend + MessageRepliable,
        M::Reply: MessageRecv + Send + 'static,
    {
        let mut msg = msg.into();
        msg.set_msg_num(self.api.msg_dispatch.gen_msg_num());
        let bytes = msg.encode()?;
        let fut = self.api.msg_dispatch.register_reply(&msg);
        self.driver.send(bytes, M::flags())?;
        Ok(fut)
    }
}

impl<D: NetDriverSendTo> Networking<D> {
    /// Send a [Message] to a specific [Connection].
    ///
    /// Messages are dispatched immediately. This method does not block or otherwise wait.
    ///
    /// # Errors
    ///
    /// [NetworkingError::InvalidConnection] if the [Connection] does not exist.
    ///
    /// [NetworkingError::MalformedMessage] if the [Message] couldn't be encoded
    /// into bytes.
    ///
    /// [NetworkingError::Closed] if the driver is closed (e.g. no longer listening on a socket in
    /// the case of a server).
    pub fn send_to<M>(
        &self,
        connection: Connection,
        msg: impl Into<Message<M>>,
    ) -> Result<(), NetworkingError>
    where
        M: MessageSend,
    {
        let mut msg = msg.into();
        msg.set_msg_num(self.api.msg_dispatch.gen_msg_num());
        let bytes = msg.encode()?;
        self.driver.send_to(connection, bytes, M::flags())
    }

    /// Send a [repliable](MessageRepliable) [Message] to a specific [Connection] and get a
    /// [reply future](MessageReplyFuture).
    ///
    /// Messages are dispatched immediately. This method does not block or otherwise wait, however
    /// the returned [reply future](MessageReplyFuture) can be waited on.
    ///
    /// # Errors
    ///
    /// [NetworkingError::InvalidConnection] if the [Connection] does not exist.
    ///
    /// [NetworkingError::MalformedMessage] if the [Message] couldn't be encoded
    /// into bytes.
    ///
    /// [NetworkingError::Closed] if the driver is closed (e.g. no longer listening on a socket in
    /// the case of a server).
    pub fn send_recv_to<M>(
        &self,
        connection: Connection,
        msg: impl Into<Message<M>>,
    ) -> Result<MessageReplyFuture<M::Reply>, NetworkingError>
    where
        M: MessageSend + MessageRepliable,
        M::Reply: MessageRecv + Send + 'static,
    {
        let mut msg = msg.into();
        msg.set_msg_num(self.api.msg_dispatch.gen_msg_num());
        let bytes = msg.encode()?;
        let fut = self.api.msg_dispatch.register_reply(&msg);
        self.driver.send_to(connection, bytes, M::flags())?;
        Ok(fut)
    }
}

impl<D: NetDriverBroadcast> Networking<D> {
    /// Broadcast a [Message] to all [Connections](Connection).
    ///
    /// Messages are dispatched immediately. This method does not block or otherwise wait.
    ///
    /// # Errors
    ///
    /// [NetworkingError::MalformedMessage] if the [Message] couldn't be encoded
    /// into bytes.
    ///
    /// [NetworkingError::Closed] if the driver is closed (e.g. no longer listening on a socket in
    /// the case of a server).
    pub fn broadcast<M>(
        &self,
        msg: impl Into<Message<M>>,
    ) -> Result<(), NetworkingError>
    where
        M: MessageSend,
    {
        let mut msg = msg.into();
        msg.set_msg_num(self.api.msg_dispatch.gen_msg_num());
        let bytes = msg.encode()?;
        self.driver.broadcast(bytes, M::flags())
    }
}