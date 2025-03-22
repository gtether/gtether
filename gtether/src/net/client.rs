//! Client-side networking APIs.

use async_trait::async_trait;
use flagset::FlagSet;
use smol::future;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::ops::Deref;
use std::sync::{Arc, Weak};
use ahash::{HashSet, HashSetExt};
use arrayvec::ArrayVec;
use parking_lot::Mutex;
use tracing::warn;

use crate::event::{EventBus, EventBusRegistry, EventCancellable};
use crate::net::message::{Message, MessageDispatchError, MessageEncodeError, MessageFlags, MessageHeader, MessageRecv, MessageRepliable, MessageReplyFuture, MessageSend};
use crate::net::{DisconnectReason, NetworkingBuildError, NetworkingBuilder};
use crate::net::message::client::ClientMessageHandler;
use crate::net::message::dispatch::{MessageDispatch, MessageDispatchClient};
use crate::NonExhaustive;

const CONNECT_BUFFER_SIZE: usize = 64;

/// Event describing when a [ClientNetworking] connects to an endpoint.
///
/// This event is cancellable, which will prevent the connection from occurring.
///
/// Note that this event is fired _before_ the connection actually occurs.
#[derive(Debug)]
pub struct ClientNetworkingConnectEvent {
    _ne: NonExhaustive,
}
impl EventCancellable for ClientNetworkingConnectEvent {}

impl ClientNetworkingConnectEvent {
    #[inline]
    pub fn new() -> Self {
        Self {
            _ne: NonExhaustive(())
        }
    }
}

/// Event describing when a [ClientNetworking] disconnects from an endpoint.
#[derive(Debug)]
pub struct ClientNetworkingDisconnectEvent {
    /// Reason for the disconnect occurring.
    reason: DisconnectReason,
}

impl ClientNetworkingDisconnectEvent {
    #[inline]
    pub fn new(reason: DisconnectReason) -> Self {
        Self {
            reason,
        }
    }

    #[inline]
    pub fn reason(&self) -> DisconnectReason {
        self.reason
    }
}

/// Client-side raw API.
///
/// This API is implemented by the networking stack, and a reference is given to
/// [RawClientFactories](RawClientFactory) in order to provide an interface for raw implementations
/// to access the networking stack's logic.
pub trait RawClientApi: Send + Sync + 'static {
    /// Dispatch raw message data to the networking stack for processing.
    fn dispatch_message(&self, msg: &[u8]) -> Result<(), MessageDispatchError>;

    /// Access to the [EventBus] to fire client-networking events.
    fn event_bus(&self) -> &EventBus;
}

/// Raw client representing a single connection.
///
/// A RawClient is used to represent a single connection from the client's side. If the client
/// closes the connection and opens a new one, a new instance of RawClient should be created.
#[async_trait]
pub trait RawClient: Send + Sync + 'static {
    fn send(
        &self,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), ClientNetworkingError>;

    /// Close the connection.
    ///
    /// After being closed, this connection can no longer be used. This method should be idempotent,
    /// and calling it multiple times should result in successive calls being NoOps.
    async fn close(&self);
}

/// Factory for [RawClients](RawClient).
///
/// This factory generates client-side connections represented by [RawClient].
///
/// Note that `RawClientFactory` is implemented for `Box<dyn RawClientFactory>`, so that boxes can
/// be used as a concrete factory type.
#[async_trait]
pub trait RawClientFactory: Send + Sync + 'static {
    /// Connect to a given [SocketAddr].
    ///
    /// If the connection is successful, yields a new [RawClient].
    async fn connect(
        &self,
        api: Arc<dyn RawClientApi>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawClient>, ClientNetworkingError>;
}

#[async_trait]
impl RawClientFactory for Box<dyn RawClientFactory> {
    #[inline]
    async fn connect(
        &self,
        api: Arc<dyn RawClientApi>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawClient>, ClientNetworkingError> {
        (**self).connect(api, socket_addr).await
    }
}

/// Simplistic [RawClientFactory] that always errors with [ClientNetworkingError::Closed].
///
/// This can be used for a [ClientNetworking] stack that isn't actively used; i.e. for a
/// singleplayer game.
#[derive(Debug, Default)]
pub struct ClosedClientFactory {}

impl ClosedClientFactory {
    /// Create a new [ClosedClientFactory].
    #[inline]
    pub fn new() -> Self { Self::default() }
}

#[async_trait]
impl RawClientFactory for ClosedClientFactory {
    #[inline]
    async fn connect(
        &self,
        _api: Arc<dyn RawClientApi>,
        _socket_addr: SocketAddr
    ) -> Result<Arc<dyn RawClient>, ClientNetworkingError> {
        Err(ClientNetworkingError::Closed)
    }
}

/// Error that can occur during client networking logic.
#[derive(Debug)]
pub enum ClientNetworkingError {
    /// An error internal to the system happened.
    ///
    /// Internal errors are generally non-recoverable, and represent something going wrong with the
    /// internal logic of the implementation.
    InternalError(String),

    /// The attempted connection was invalid.
    InvalidConnection(SocketAddr),

    /// The operation was cancelled by the user.
    Cancelled,

    /// A message failed to send because it could not be serialized.
    MalformedMessage(Box<dyn Error + Send + Sync>),

    /// An operation was attempted on a client connection that has already been closed.
    Closed,
}

impl Display for ClientNetworkingError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InternalError(msg) => write!(f, "Internal Error: {msg}"),
            Self::InvalidConnection(socket_addr) =>
                write!(f, "Invalid connection: {socket_addr}"),
            Self::Cancelled =>
                write!(f, "The operation was cancelled by the user"),
            Self::MalformedMessage(err) =>
                write!(f, "Malformed message; {err}"),
            Self::Closed => write!(f, "Client has been closed"),
        }
    }
}

impl Error for ClientNetworkingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MalformedMessage(err) => Some(err.deref()),
            _ => None,
        }
    }
}

impl ClientNetworkingError {
    /// Helper for creating an [InternalError](Self::InternalError) from a string.
    #[inline]
    pub fn internal_error(msg: impl Into<String>) -> Self {
        Self::InternalError(msg.into())
    }
}

impl From<ump::Error<ClientNetworkingError>> for ClientNetworkingError {
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

impl From<MessageEncodeError> for ClientNetworkingError {
    #[inline]
    fn from(value: MessageEncodeError) -> Self {
        Self::MalformedMessage(Box::new(value))
    }
}

struct ConnectBuffer {
    buffer: Mutex<ArrayVec<Box<[u8]>, CONNECT_BUFFER_SIZE>>,
    active_replies: Mutex<HashSet<NonZeroU64>>,
}

impl ConnectBuffer {
    fn new() -> Self {
        Self {
            buffer: Mutex::new(ArrayVec::new()),
            active_replies: Mutex::new(HashSet::new()),
        }
    }

    fn try_push(&self, msg: &[u8]) -> Result<(), MessageDispatchError> {
        match self.buffer.lock().try_push(Box::from(msg)) {
            Ok(_) => Ok(()),
            Err(_) => Err(MessageDispatchError::DispatchFailed(Box::from(
                "ConnectBuffer is full".to_owned()
            ))),
        }
    }

    fn register_reply<M>(&self, msg: &Message<M>)
    where
        M: MessageSend + MessageRepliable,
        M::Reply: MessageRecv,
    {
        let msg_num = msg.header().msg_num()
            .expect("msg_num should already be set");
        self.active_replies.lock().insert(msg_num);
    }

    fn check_reply(&self, msg: &[u8]) -> Result<bool, MessageDispatchError> {
        let (msg_header, _) = MessageHeader::decode(msg)?;
        if let Some(reply_num) = msg_header.reply_num() {
            Ok(self.active_replies.lock().remove(&reply_num))
        } else {
            Ok(false)
        }
    }

    fn drain(&self, msg_dispatch: &Arc<MessageDispatch<MessageDispatchClient>>) {
        let mut buffer = self.buffer.lock();
        let range = 0..buffer.len();
        for msg in buffer.drain(range) {
            match msg_dispatch.dispatch(&*msg) {
                Ok(_) => {},
                Err(error) =>
                    warn!(?error, "Failed to dispatch buffered ConnectContext message"),
            }
        }
    }
}

/// Context representing a connection that is being established.
///
/// This context is used while a connection is actively being established to hold messages while
/// handlers are being installed in the [ClientNetworking] stack. This ensures that no messages
/// might be lost during the initial connection.
///
/// Additionally, if communication is needed to finish establishing the connection,
/// [ClientConnectContext::send_recv()] can be used to send messages during this period.
///
/// Once this context is dropped, any buffered messages will be dispatched, and [ClientNetworking]
/// will start operating as normal.
pub struct ClientConnectContext {
    msg_dispatch: Arc<MessageDispatch<MessageDispatchClient>>,
    connect_buffer: Arc<ConnectBuffer>,
    client: Option<Arc<dyn RawClient>>,
}

impl ClientConnectContext {
    fn set_client(&mut self, client: Arc<dyn RawClient>) {
        self.client = Some(client);
    }

    /// Send a [Message] and wait for a response.
    ///
    /// This is functionally the same as [ClientNetworking::send_recv()], but can be used while a
    /// connection is being established.
    pub fn send_recv<M>(
        &self,
        msg: impl Into<Message<M>>,
    ) -> Result<MessageReplyFuture<M::Reply>, ClientNetworkingError>
    where
        M: MessageSend + MessageRepliable,
        M::Reply: MessageRecv + Send + 'static,
    {
        let client = self.client.as_ref()
            .expect("'client' should already be connected and set");
        let mut msg = msg.into();
        msg.set_msg_num(self.msg_dispatch.gen_msg_num());
        let bytes = msg.encode()?;
        self.connect_buffer.register_reply(&msg);
        let fut = self.msg_dispatch.register_reply(&msg);
        client.send(bytes, M::flags())?;
        Ok(fut)
    }
}

impl Drop for ClientConnectContext {
    fn drop(&mut self) {
        self.connect_buffer.drain(&self.msg_dispatch);
    }
}

struct Api {
    msg_dispatch: Arc<MessageDispatch<MessageDispatchClient>>,
    event_bus: EventBus,
    connect_buffer: Mutex<Option<Weak<ConnectBuffer>>>,
}

impl Api {
    fn new() -> Self {
        let event_bus = EventBus::builder()
            .event_type::<ClientNetworkingConnectEvent>()
            .event_type::<ClientNetworkingDisconnectEvent>()
            .build();

        Self {
            msg_dispatch: Arc::new(MessageDispatch::new()),
            event_bus,
            connect_buffer: Mutex::new(None),
        }
    }

    fn connect_ctx(&self) -> ClientConnectContext {
        let connect_buffer = Arc::new(ConnectBuffer::new());

        *self.connect_buffer.lock() = Some(Arc::downgrade(&connect_buffer));

        ClientConnectContext {
            msg_dispatch: self.msg_dispatch.clone(),
            connect_buffer,
            client: None,
        }
    }
}

impl RawClientApi for Api {
    #[inline]
    fn dispatch_message(&self, msg: &[u8]) -> Result<(), MessageDispatchError> {
        let mut connect_buffer = self.connect_buffer.lock();
        if let Some(connect_buffer) = connect_buffer.as_ref().map(|w| w.upgrade()).flatten() {
            if connect_buffer.check_reply(msg)? {
                self.msg_dispatch.dispatch(msg)
            } else {
                connect_buffer.try_push(msg)
            }
        } else {
            *connect_buffer = None;
            self.msg_dispatch.dispatch(msg)
        }
    }

    #[inline]
    fn event_bus(&self) -> &EventBus {
        &self.event_bus
    }
}

/// Manager for client networking stack.
///
/// An instance of ClientNetworking manages an internal [RawClientFactory] and any
/// [RawClients](RawClient) it can create. Only one [RawClient] can be managed at a time.
pub struct ClientNetworking {
    raw_factory: Box<dyn RawClientFactory>,
    api: Arc<Api>,
    inner: smol::lock::RwLock<Option<Arc<dyn RawClient>>>,
}

impl ClientNetworking {
    /// Create a builder for [ClientNetworking].
    ///
    /// This is the recommended way to create a new [ClientNetworking] stack.
    #[inline]
    pub fn builder() -> NetworkingBuilder<ClientBuilder> {
        NetworkingBuilder::new()
    }

    fn new(
        raw_factory: Box<dyn RawClientFactory>,
    ) -> Self {
        let api = Arc::new(Api::new());

        Self {
            raw_factory,
            api,
            inner: smol::lock::RwLock::new(None),
        }
    }

    /// [EventBusRegistry] for [ClientNetworking] events.
    ///
    /// These include e.g.:
    ///  * [ClientNetworkingConnectEvent]
    ///  * [ClientNetworkingDisconnectEvent]
    #[inline]
    pub fn event_bus(&self) -> &EventBusRegistry {
        self.api.event_bus.registry()
    }

    /// Connect to a given [SocketAddr].
    ///
    /// If this [ClientNetworking] is already connected, closes the existing connection first.
    ///
    /// This yields a [ClientConnectContext], which is used while establishing a connection to
    /// intercept and buffer any incoming messages. This allows message handlers to be installed
    /// after the connection is made but before messages are dispatched, preventing any messages
    /// from being dropped due to missing a handler.
    pub async fn connect(
        &self,
        socket_addr: SocketAddr,
    ) -> Result<ClientConnectContext, ClientNetworkingError> {
        let mut inner = self.inner.write().await;
        if let Some(client) = inner.take() {
            client.close().await;
        }
        let event = self.api.event_bus.fire(ClientNetworkingConnectEvent::new());
        if event.is_cancelled() {
            Err(ClientNetworkingError::Cancelled)
        } else {
            let mut connect_ctx = self.api.connect_ctx();
            let client = self.raw_factory.connect(self.api.clone(), socket_addr).await?;
            connect_ctx.set_client(client.clone());
            *inner = Some(client);
            Ok(connect_ctx)
        }
    }

    /// Convenience wrapper for a sync version of [Self::connect()].
    #[inline]
    pub fn connect_sync(
        &self,
        socket_addr: SocketAddr,
    ) -> Result<ClientConnectContext, ClientNetworkingError> {
        future::block_on(self.connect(socket_addr))
    }

    /// Close the [ClientNetworking].
    ///
    /// Any future operations e.g. [Self::send()] will yield [ClientNetworkingError::Closed], until
    /// the [ClientNetworking] is reconnected via [Self::connect()].
    #[inline]
    pub async fn close(&self) {
        if let Some(client) = self.inner.write().await.take() {
            client.close().await;
        }
    }

    /// Convenience wrapper for a sync version of [Self::close()].
    #[inline]
    pub fn close_sync(&self) {
        future::block_on(self.close());
    }

    /// Insert a message handler into this [ClientNetworking] stack.
    ///
    /// The handler is associated with the message type's [key](message::MessageBody::KEY), and
    /// inserting multiple handlers with the same key will override the old one.
    pub fn insert_msg_handler<M, E>(
        &self,
        handler: impl ClientMessageHandler<M, E>
    )
    where
        M: MessageRecv,
        E: Error + Send + Sync + 'static,
    {
        self.api.msg_dispatch.insert_handler(handler);
    }

    /// Send a [Message] from this client to a connected server.
    ///
    /// Does not block, and will yield as soon as the message is passed off to the networking stack.
    ///
    /// # Errors
    ///
    /// Errors if the provided [Message] could not be serialized, or if the client connection is
    /// already closed.
    pub fn send<M>(
        &self,
        msg: impl Into<Message<M>>,
    ) -> Result<(), ClientNetworkingError>
    where
        M: MessageSend,
    {
        let inner = self.inner.read_blocking();
        if let Some(client) = &*inner {
            let mut msg = msg.into();
            msg.set_msg_num(self.api.msg_dispatch.gen_msg_num());
            let bytes = msg.encode()
                .map_err(|err| ClientNetworkingError::MalformedMessage(Box::new(err)))?;
            client.send(bytes, M::flags())
        } else {
            Err(ClientNetworkingError::Closed)
        }
    }

    /// Send a [Message] from this client to a connected server, and wait for a response.
    ///
    /// Does not block. Will yield a [MessageReplyFuture] that can be used to wait for a response
    /// either synchronously or asynchronously.
    ///
    /// # Errors
    ///
    /// Errors if the provided [Message] could not be serialized, or if the client connection is
    /// already closed.
    pub fn send_recv<M>(
        &self,
        msg: impl Into<Message<M>>,
    ) -> Result<MessageReplyFuture<M::Reply>, ClientNetworkingError>
    where
        M: MessageSend + MessageRepliable,
        M::Reply: MessageRecv + Send + 'static,
    {
        let inner = self.inner.read_blocking();
        if let Some(client) = &*inner {
            let mut msg = msg.into();
            msg.set_msg_num(self.api.msg_dispatch.gen_msg_num());
            let bytes = msg.encode()?;
            let fut = self.api.msg_dispatch.register_reply(&msg);
            client.send(bytes, M::flags())?;
            Ok(fut)
        } else {
            Err(ClientNetworkingError::Closed)
        }
    }
}

impl Drop for ClientNetworking {
    fn drop(&mut self) {
        if let Some(client) = self.inner.write_blocking().take() {
            future::block_on(client.close())
        }
    }
}

/// Builder sub-pattern for [ClientNetworking] stack.
///
/// This builder cannot be used directly, and is only used as a generic type in [NetworkingBuilder].
/// To get a builder using this, see [ClientNetworking::builder()].
#[derive(Default)]
pub struct ClientBuilder {
    raw_factory: Option<Box<dyn RawClientFactory>>,
}

impl NetworkingBuilder<ClientBuilder> {
    /// Set the [RawClientFactory] implementation for creating [RawClients](RawClient).
    ///
    /// This option is required.
    #[inline]
    pub fn raw_factory(mut self, raw_factory: impl RawClientFactory) -> Self {
        self.extra.raw_factory = Some(Box::new(raw_factory));
        self
    }

    /// Build this [ClientNetworking] stack.
    ///
    /// # Errors
    ///
    /// Errors when there is a required option that is missing, or if the networking stack fails to
    /// initialize.
    pub fn build(self) -> Result<Arc<ClientNetworking>, NetworkingBuildError> {
        let raw_factory = self.extra.raw_factory
            .ok_or(NetworkingBuildError::missing_option("raw_factory"))?;

        Ok(Arc::new(ClientNetworking::new(raw_factory)))
    }
}