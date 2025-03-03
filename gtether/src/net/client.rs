//! Client-side networking APIs.

use async_trait::async_trait;
use flagset::FlagSet;
use smol::future;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::net::message::{Message, MessageDispatchError, MessageFlags, MessageRecv, MessageRepliable, MessageReplyFuture, MessageSend};
use crate::net::{NetworkingBuildError, NetworkingBuilder};
use crate::net::message::client::ClientMessageHandler;
use crate::net::message::dispatch::{MessageDispatch, MessageDispatchClient};

/// Client-side raw API.
///
/// This API is implemented by the networking stack, and a reference is given to
/// [RawClientFactories](RawClientFactory) in order to provide an interface for raw implementations
/// to access the networking stack's logic.
pub trait RawClientApi: Send + Sync + 'static {
    /// Dispatch raw message data to the networking stack for processing.
    fn dispatch_message(&self, msg: &[u8]) -> Result<(), MessageDispatchError>;
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

struct Api {
    msg_dispatch: MessageDispatch<MessageDispatchClient>,
}

impl Api {
    fn new() -> Self {
        Self {
            msg_dispatch: MessageDispatch::new(),
        }
    }
}

impl RawClientApi for Api {
    fn dispatch_message(&self, msg: &[u8]) -> Result<(), MessageDispatchError> {
        self.msg_dispatch.dispatch(msg)
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
    next_msg_num: AtomicU64,
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
            next_msg_num: AtomicU64::new(1),
        }
    }

    /// Connect to a given [SocketAddr].
    ///
    /// If this [ClientNetworking] is already connected, closes the existing connection first.
    pub async fn connect(&self, socket_addr: SocketAddr) -> Result<(), ClientNetworkingError> {
        let mut inner = self.inner.write().await;
        if let Some(client) = inner.take() {
            client.close().await;
        }
        *inner = Some(self.raw_factory.connect(self.api.clone(), socket_addr).await?);
        self.next_msg_num.store(1, Ordering::SeqCst);
        Ok(())
    }

    /// Convenience wrapper for a sync version of [Self::connect()].
    #[inline]
    pub fn connect_sync(&self, socket_addr: SocketAddr) -> Result<(), ClientNetworkingError> {
        future::block_on(self.connect(socket_addr))
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
            msg.set_msg_num(self.next_msg_num.fetch_add(1, Ordering::SeqCst).try_into().unwrap());
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
            msg.set_msg_num(self.next_msg_num.fetch_add(1, Ordering::SeqCst).try_into().unwrap());
            let bytes = msg.encode()
                .map_err(|err| ClientNetworkingError::MalformedMessage(Box::new(err)))?;
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