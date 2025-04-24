//! OSI layer 4 (transport) driver traits.
//!
//! This module contains the traits that are required to implement for [Networking] stack transport
//! layer drivers. These drivers control the OSI layer 4 (transport) implementation that a
//! Networking stack uses, an example being an implementation for
//! [GNS](https://github.com/ValveSoftware/GameNetworkingSockets), found in the [`gns`](super::gns)
//! module. For further details on the overall Networking stack structure, see the
//! [parent module](super).
//!
//! # Implementing
//!
//! Drivers pick and choose which traits they want to implement from this module. Each trait
//! describes a particular functionality or set of functionality that that driver will support. When
//! creating a [Networking] stack, the traits that the driver implements will expose the relevant
//! functionality on the Networking stack.
//!
//! At the very basic level, every driver is required to implement [NetDriver], which simply
//! describes how to "close" a driver. What this means is up to the driver, but once closed it is
//! not expected to be used again, and most functionality should yield [NetworkingError::Closed]
//! errors.
//!
//! Further functionality can be described by the following:
//!  * [NetDriverConnect] - The driver can connect to remote endpoints.
//!  * [NetDriverListen] - The driver can bind to and listen on sockets.
//!  * [NetDriverSend] - The driver can send to a single remote endpoint. This is typically
//!    implemented on client drivers in client/server setups, where the client only has one endpoint
//!    to connect to.
//!  * [NetDriverSendTo] - The driver can send to specific remote endpoints.
//!  * [NetDriverBroadcast] - The driver can broadcast to all connected endpoints.
//!
//! See each of these traits for more details.
//!
//! In addition, creation of drivers is described by an implementation of [NetDriverFactory]. This
//! factory implementation is what will be given to a [Networking] stack during construction, so
//! there should be a valid implementation for your driver.

use std::net::SocketAddr;
use std::sync::Arc;
use async_trait::async_trait;
use flagset::FlagSet;

use crate::net::{Connection, NetworkingApi, NetworkingError};
use crate::net::message::MessageFlags;

/// Lowest level [Networking][net] driver trait.
///
/// Every [Networking][net] driver is required to implement this trait.
///
/// [net]: super::Networking
#[async_trait]
pub trait NetDriver: Send + Sync + 'static {
    /// "Close" this driver.
    ///
    /// What this means is up to the driver, but once closed most methods should no longer function,
    /// and instead yield [NetworkingError::Closed] errors.
    ///
    /// Note that the driver may be "re-opened" e.g. by successive
    /// [`connect()`](NetDriverConnect::connect)/[`listen()`](NetDriverListen::listen) calls, if the
    /// driver wishes to allow such functionality.
    async fn close(&self);
}

/// Trait describing a [Networking](super::Networking) driver that can connect to remote endpoints.
///
/// How many endpoints can be connected to is up to the particular driver. For example, a
/// client/server architecture may only allow a client to connect to one server at a time, whereas
/// a peer-to-peer architecture may allow clients to connect to many other clients at once.
#[async_trait]
pub trait NetDriverConnect: NetDriver {
    /// Attempt to connect to the remote `socket_addr`.
    ///
    /// At this point in a connection workflow, the [Networking](super::Networking) stack has
    /// already generated and initialized a [Connection] ID, but the driver is responsible for
    /// performing the actual connection logic.
    ///
    /// If an existing connection needs to be closed before a new one can be connected, it is the
    /// responsibility of the driver to handle closing the existing connection.
    async fn connect(
        &self,
        connection: Connection,
        socket_addr: SocketAddr,
    ) -> Result<(), NetworkingError>;
}

/// Trait describing a [Networking](super::Networking) driver that can close individual
/// [Connections](Connection).
///
/// For drivers that can support multiple active connections at once, such as servers in
/// client/server architectures or clients in peer-to-peer architectures, this allows specific
/// individual connections to be closed without closing all connections.
#[async_trait]
pub trait NetDriverCloseConnection: NetDriver {
    /// Close the specified [Connection].
    ///
    /// If there is no connection for the specified [Connection] ID, this method is expected to be
    /// a noop.
    async fn close_connection(
        &self,
        connection: Connection,
    );
}

/// Trait describing a [Networking](super::Networking) driver that can bind to and listen on
/// sockets.
///
/// Generally, a driver only listens on one socket at a time, but this implementation is up to the
/// driver, and drivers can choose to allow multiple listens if they wish.
#[async_trait]
pub trait NetDriverListen: NetDriver {
    /// Bind to and listen on a socket.
    ///
    /// If an existing binding needs to be closed before a new one can be opened, it is the
    /// responsibility of the driver to handle closing the existing binding.
    async fn listen(
        &self,
        socket_addr: SocketAddr,
    ) -> Result<(), NetworkingError>;
}

/// Trait describing a [Networking](super::Networking) driver that can send messages.
///
/// This trait describes sending messages to exactly one endpoint, and therefore is only appropriate
/// if the driver can only have one connection. If the driver can have multiple connections,
/// [NetDriverSendTo] is a more appropriate trait.
pub trait NetDriverSend: NetDriver {
    /// Send raw message data.
    ///
    /// This method is expected to not block or otherwise wait. If blocking behavior is needed, it
    /// should be handed off to a separate thread or other non-blocking mechanism.
    fn send(
        &self,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), NetworkingError>;
}

/// Trait describing a [Networking](super::Networking) driver that can send messages to endpoints.
///
/// This trait describes sending messages to specified endpoints. If the driver can only have a
/// single connection, [NetDriverSend] may be more appropriate.
pub trait NetDriverSendTo: NetDriver {
    /// Send raw message data to a specified connection.
    ///
    /// This method is expected to not block or otherwise wait. If blocking behavior is needed, it
    /// should be handed off to a separate thread or other non-blocking mechanism.
    fn send_to(
        &self,
        connection: Connection,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), NetworkingError>;
}

/// Trait describing a [Networking](super::Networking) driver that can broadcast messages to all
/// connected endpoints.
///
/// This trait describes the capability to send a single message to all connections. This is
/// generally applicable for any driver that can have many connections.
pub trait NetDriverBroadcast: NetDriver {
    /// Send raw message data to all connections.
    fn broadcast(
        &self,
        msg: Vec<u8>,
        flags: FlagSet<MessageFlags>,
    ) -> Result<(), NetworkingError>;
}

/// Trait describing how to create a [Networking][net] driver.
///
/// This trait is what a [Networking][net] stack is created with, instead of a driver directly. This
/// allows the [Networking][net] stack to create a [NetworkingApi] to give to this factory.
///
/// Note that this trait is automatically implemented on any function with the following signature:
/// ```text
/// Fn(Arc<NetworkingApi>) -> Arc<D> where D: NetDriver
/// ```
///
/// [net]: super::Networking
pub trait NetDriverFactory<D: NetDriver> {
    /// Create a [NetDriver] using the given [NetworkingApi] instance.
    fn create(&self, api: Arc<NetworkingApi>) -> Arc<D>;
}

impl<D, F> NetDriverFactory<D> for F
where
    D: NetDriver,
    F: Fn(Arc<NetworkingApi>) -> Arc<D>,
{
    fn create(&self, api: Arc<NetworkingApi>) -> Arc<D> {
        self(api)
    }
}

/// "No-Operation" [NetDriver].
///
/// This basic [NetDriver] implementation represents "no" networking, and can be used where
/// networking is not actively required.
///
/// [NetDriverFactory] is implemented directly on this type, so an instance of this type can be
/// used as a factory for itself.
pub struct NoNetDriver;

impl NoNetDriver {
    /// Create a new NoNetDriver.
    #[inline]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {})
    }
}

#[async_trait]
impl NetDriver for NoNetDriver {
    async fn close(&self) {
        /* noop */
    }
}

impl NetDriverFactory<NoNetDriver> for Arc<NoNetDriver> {
    #[inline]
    fn create(&self, _api: Arc<NetworkingApi>) -> Arc<NoNetDriver> {
        self.clone()
    }
}