//! Server-side networking APIs.

use async_trait::async_trait;
use smol::future;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crate::net::message::MessageDispatch;
use crate::net::{NetworkingBuildError, NetworkingBuilder};

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
        msg_dispatch: Arc<MessageDispatch>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawServer>, ServerNetworkingError>;
}

#[async_trait]
impl RawServerFactory for Box<dyn RawServerFactory> {
    #[inline]
    async fn listen(
        &self,
        msg_dispatch: Arc<MessageDispatch>,
        socket_addr: SocketAddr,
    ) -> Result<Arc<dyn RawServer>, ServerNetworkingError> {
        (**self).listen(msg_dispatch, socket_addr).await
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
    /// Create a builder for [ServerNetworking].
    ///
    /// This is the recommended way to create a new [ServerNetworking] stack.
    #[inline]
    pub fn builder() -> NetworkingBuilder<ServerBuilder> {
        NetworkingBuilder::new()
    }

    fn new(
        raw_factory: impl RawServerFactory,
        msg_dispatch: Arc<MessageDispatch>,
        socket_addr: SocketAddr,
    ) -> Result<Self, ServerNetworkingError> {
        let inner = future::block_on(raw_factory.listen(
            msg_dispatch,
            socket_addr,
        ))?;

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

impl From<ServerNetworkingError> for NetworkingBuildError {
    #[inline]
    fn from(value: ServerNetworkingError) -> Self {
        Self::InitError { source: Some(Box::new(value)) }
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
    pub fn build(mut self) -> Result<ServerNetworking, NetworkingBuildError> {
        let msg_dispatch = self.msg_dispatch()?;
        let raw_factory = self.extra.raw_factory
            .ok_or(NetworkingBuildError::missing_option("raw_factory"))?;

        let ip_addr = self.extra.ip_addr
            .unwrap_or(Ipv4Addr::LOCALHOST.into());
        let port = self.extra.port
            .ok_or(NetworkingBuildError::missing_option("port"))?;
        let socket_addr = SocketAddr::new(ip_addr, port);

        Ok(ServerNetworking::new(raw_factory, msg_dispatch, socket_addr)?)
    }
}