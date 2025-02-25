//! Client-side networking APIs.

use async_trait::async_trait;
use smol::future;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::net::message::MessageDispatch;
use crate::net::{NetworkingBuildError, NetworkingBuilder};

/// Raw client representing a single connection.
///
/// A RawClient is used to represent a single connection from the client's side. If the client
/// closes the connection and opens a new one, a new instance of RawClient should be created.
#[async_trait]
pub trait RawClient: Send + Sync + 'static {
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
        msg_dispatch: Arc<MessageDispatch>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawClient>, ClientNetworkingError>;
}

#[async_trait]
impl RawClientFactory for Box<dyn RawClientFactory> {
    #[inline]
    async fn connect(
        &self,
        msg_dispatch: Arc<MessageDispatch>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawClient>, ClientNetworkingError> {
        (**self).connect(msg_dispatch, socket_addr).await
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
        _msg_dispatch: Arc<MessageDispatch>,
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

    /// An operation was attempted on a client connection that has already been closed.
    Closed,
}

impl Display for ClientNetworkingError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InternalError(msg) => write!(f, "Internal Error: {msg}"),
            Self::InvalidConnection(socket_addr) =>
                write!(f, "Invalid connection: {socket_addr}"),
            Self::Closed => write!(f, "Client has been closed"),
        }
    }
}

impl Error for ClientNetworkingError {}

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

/// Manager for client networking stack.
///
/// An instance of ClientNetworking manages an internal [RawClientFactory] and any
/// [RawClients](RawClient) it can create. Only one [RawClient] can be managed at a time.
pub struct ClientNetworking {
    raw_factory: Box<dyn RawClientFactory>,
    msg_dispatch: Arc<MessageDispatch>,
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
        msg_dispatch: Arc<MessageDispatch>,
    ) -> Self {
        Self {
            raw_factory,
            msg_dispatch,
            inner: smol::lock::RwLock::new(None),
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
        *inner = Some(self.raw_factory.connect(self.msg_dispatch.clone(), socket_addr).await?);
        Ok(())
    }

    /// Convenience wrapper for a sync version of [Self::connect()].
    #[inline]
    pub fn connect_sync(&self, socket_addr: SocketAddr) -> Result<(), ClientNetworkingError> {
        future::block_on(self.connect(socket_addr))
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
    pub fn build(mut self) -> Result<ClientNetworking, NetworkingBuildError> {
        let msg_dispatch = self.msg_dispatch()?;
        let raw_factory = self.extra.raw_factory
            .ok_or(NetworkingBuildError::missing_option("raw_factory"))?;

        Ok(ClientNetworking::new(raw_factory, msg_dispatch))
    }
}