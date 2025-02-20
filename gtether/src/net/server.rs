//! Server-side networking APIs.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::sync::Arc;
use async_trait::async_trait;
use smol::future;

/// Raw server listening on a given socket.
///
/// A RawServer can contain and manage many client connections for a single socket.
#[async_trait]
pub trait RawServer: Send + Sync + 'static {
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
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawServer>, ServerNetworkingError>;
}

#[async_trait]
impl RawServerFactory for Box<dyn RawServerFactory> {
    #[inline]
    async fn listen(&self, socket_addr: SocketAddr) -> Result<Arc<dyn RawServer>, ServerNetworkingError> {
        (**self).listen(socket_addr).await
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

    /// An operation was attempted on a server manager that has already been closed.
    Closed,
}

impl Display for ServerNetworkingError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InternalError(msg) => write!(f, "Internal Error: {msg}"),
            Self::InvalidListen(socket_addr) =>
                write!(f, "Cannot listen on: {socket_addr}"),
            Self::Closed => write!(f, "Server has been closed"),
        }
    }
}

impl Error for ServerNetworkingError {}

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

/// Manager for server networking logic.
///
/// An instance of ServerNetworking manages an internal [RawServer]. This internal server may
/// handle many client connections.
pub struct ServerNetworking {
    inner: Arc<dyn RawServer>,
}

impl ServerNetworking {
    /// Create a new [ServerNetworking] instance.
    pub fn new(
        raw_factory: impl RawServerFactory,
        socket_addr: SocketAddr,
    ) -> Result<Self, ServerNetworkingError> {
        let inner = future::block_on(raw_factory.listen(socket_addr))?;

        Ok(Self {
            inner,
        })
    }
}

impl Drop for ServerNetworking {
    fn drop(&mut self) {
        future::block_on(self.inner.close())
    }
}