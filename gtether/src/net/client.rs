//! Client-side networking APIs.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::sync::Arc;
use async_trait::async_trait;
use smol::future;

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
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawClient>, ClientNetworkingError>;
}

#[async_trait]
impl RawClientFactory for Box<dyn RawClientFactory> {
    #[inline]
    async fn connect(&self, socket_addr: SocketAddr) -> Result<Arc<dyn RawClient>, ClientNetworkingError> {
        (**self).connect(socket_addr).await
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

/// Manager for client networking logic.
///
/// An instance of ClientNetworking manages an internal [RawClientFactory] and any
/// [RawClients](RawClient) it can create. Only one [RawClient] can be managed at a time.
pub struct ClientNetworking {
    raw_factory: Box<dyn RawClientFactory>,
    inner: smol::lock::RwLock<Option<Arc<dyn RawClient>>>,
}

impl ClientNetworking {
    /// Create a new [ClientNetworking] instance.
    ///
    /// New instances start as unconnected.
    pub fn new(raw_factory: impl RawClientFactory) -> Self {
        Self {
            raw_factory: Box::new(raw_factory),
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
        *inner = Some(self.raw_factory.connect(socket_addr).await?);
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