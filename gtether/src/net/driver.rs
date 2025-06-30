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

use async_trait::async_trait;
use flagset::FlagSet;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::net::message::MessageFlags;
use crate::net::{Connection, NetworkingApi, NetworkingError};

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

/// Re-usable [NetDriver] test infrastructure.
///
/// This module contains helper structs and functions that can be used to test [NetDriver]
/// implementations and ensure they follow required specification.
///
/// While you can use these helpers directly to create your own tests, it is recommended to instead
/// use the provided macros to generate tests for your implementation. See the following:
///  * [`test_net_driver_client_server_core!`]
///  * [`test_net_driver_client_server_send!`]
///  * [`test_net_driver_client_server_send_to!`]
///  * [`test_net_driver_client_server_broadcast!`]
#[cfg(test)]
pub mod tests {
    use ahash::HashMap;
    use bitcode::{Decode, Encode};
    use educe::Educe;
    use parking_lot::{Mutex, RwLock};
    use smol::future;
    use smol::future::FutureExt;
    use std::convert::Infallible;
    use std::fmt;
    use std::fmt::{Debug, Display, Formatter};
    use std::future::Future;
    use std::net::Ipv4Addr;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};
    use tracing::debug;

    use crate::event::{Event, EventHandler};
    use crate::net::message::{Message, MessageBody};
    use crate::net::{Networking, NetworkingConnectEvent, NetworkingDisconnectEvent};

    use super::*;

    pub use gtether_derive::{
        test_net_driver_client_server_broadcast,
        test_net_driver_client_server_core,
        test_net_driver_client_server_send,
        test_net_driver_client_server_send_to,
    };

    /// Prelude containing all *Ext traits from this module.
    pub mod prelude {
        pub use super::{
            ClientServerStackFactoryExt,
            ConnectionPairSliceExt,
            NetTestBroadcastSliceExt,
            NetTestConnectSliceExt,
            NetTestSliceExt,
            TestConnectionSendSliceExt,
            TestConnectionSendToSliceExt,
            TestConnectionSliceExt,
        };
    }

    struct Timeout {
        time: Duration,
        deadline: Instant,
    }

    impl Timeout {
        fn new(time: Duration) -> Self {
            let deadline = Instant::now() + time;
            Self {
                time,
                deadline,
            }
        }

        fn run<T>(
            &self,
            fut: impl Future<Output = T>,
            context: impl AsRef<str>,
            extra: Option<fmt::Arguments>,
        ) -> T {
            let timeout_fn = async move {
                smol::Timer::at(self.deadline).await;
                match extra {
                    Some(extra) =>
                        panic!("Timeout ({:?}) reached while: {}; {}", self.time, context.as_ref(), extra),
                    None =>
                        panic!("Timeout ({:?}) reached while: {}", self.time, context.as_ref()),
                }
            };
            future::block_on(fut.or(timeout_fn))
        }
    }

    /// Simple [Message] that contains a [String].
    #[derive(Encode, Decode, MessageBody, Debug)]
    #[message_flag(Reliable)]
    pub struct TestMessage {
        pub value: String,
    }

    impl TestMessage {
        #[inline]
        pub fn new(value: impl Into<String>) -> Self {
            Self {
                value: value.into(),
            }
        }
    }

    /// Simple repliable [Message] that contains a [String].
    #[derive(Encode, Decode, MessageBody, Debug)]
    #[message_flag(Reliable)]
    #[message_reply(TestMessage)]
    pub struct TestMessageRepliable {
        pub value: String
    }

    impl TestMessageRepliable {
        #[inline]
        pub fn new(value: impl Into<String>) -> Self {
            Self {
                value: value.into(),
            }
        }
    }

    async fn execute_futures(futures: impl Iterator<Item=impl Future>) {
        let ex = smol::LocalExecutor::new();
        let tasks = futures
            .map(|fut| ex.spawn(fut))
            .collect::<Vec<_>>();
        ex.run(async {
            for task in tasks {
                task.await;
            }
        }).await;
    }

    /// Asserts that a given [NetworkingError] is equivalent to another.
    #[inline]
    pub fn assert_networking_error_eq(actual: NetworkingError, expected: &NetworkingError) {
        match expected {
            NetworkingError::InvalidAddress(expected_socket_addr) =>
                assert_matches!(actual, NetworkingError::InvalidAddress(socket_addr) if socket_addr == *expected_socket_addr),
            NetworkingError::InvalidConnection(expected_connection) =>
                assert_matches!(actual, NetworkingError::InvalidConnection(connection) if connection == *expected_connection),
            NetworkingError::Closed =>
                assert_matches!(actual, NetworkingError::Closed),
            NetworkingError::Cancelled =>
                assert_matches!(actual, NetworkingError::Cancelled),
            other => panic!("Unsupported expected error type: {other:?}"),
        }
    }

    /// ID used to identify [NetTest] instances.
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
    pub struct NetTestId(u64);

    static NEXT_NET_TEST_ID: AtomicU64 = AtomicU64::new(1);

    impl NetTestId {
        fn new() -> Self {
            Self(NEXT_NET_TEST_ID.fetch_add(1, Ordering::SeqCst))
        }
    }

    impl Display for NetTestId {
        #[inline]
        fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
            write!(f, "net-test-{}", self.0)
        }
    }

    fn fmt_record_data(
        data: &RwLock<HashMap<Connection, HashMap<String, usize>>>,
        f: &mut Formatter<'_>,
    ) -> fmt::Result {
        let data = data.read();
        f.debug_map()
            .entries(data.iter().map(|(connection, stats)| {
                (connection.to_string(), stats)
            }))
            .finish()
    }

    #[derive(Educe)]
    #[educe(Debug)]
    struct Records {
        #[educe(Debug(method(std::fmt::Display::fmt)))]
        id: NetTestId,
        #[educe(Debug(method(fmt_record_data)))]
        inner: RwLock<HashMap<Connection, HashMap<String, usize>>>,
        #[educe(Debug(ignore))]
        wakers: Mutex<Vec<Waker>>,
    }

    impl Records {
        fn new(id: NetTestId) -> Self {
            Self {
                id,
                inner: RwLock::default(),
                wakers: Mutex::default(),
            }
        }

        fn record_message(&self, connection: Connection, msg: String) {
            debug!(id = %self.id, from = %connection, ?msg, "Recording message");
            let mut wakers = self.wakers.lock();
            let mut records = self.inner.write();
            let values: &mut HashMap<String, usize> = records.entry(connection).or_default();
            let value_count = values.entry(msg).or_default();
            *value_count += 1;
            let waker_count = wakers.len();
            for waker in wakers.drain(0..waker_count) {
                waker.wake();
            }
        }

        fn message_count_for_connection(
            &self,
            connection: Connection,
            value: impl AsRef<str>,
        ) -> usize {
            let records = self.inner.read();
            records.get(&connection)
                .map(|values| {
                    values.get(value.as_ref()).map(|v| *v).unwrap_or(0)
                })
                .unwrap_or(0)
        }

        fn wait_for_message_count_for_connection(
            self: &Arc<Self>,
            connection: Connection,
            value: impl AsRef<str>,
            count: usize,
        ) -> RecordCountFuture {
            let msg = value.as_ref().to_string();
            RecordCountFuture {
                count_type: RecordCountFutureType::ForConnection { connection, msg },
                threshold: count,
                records: self.clone(),
            }
        }

        fn message_count(&self, value: impl AsRef<str>) -> usize {
            let records = self.inner.read();
            records.values().map(|values| {
                values.get(value.as_ref()).map(|v| *v).unwrap_or(0)
            }).sum()
        }

        fn wait_for_message_count(
            self: &Arc<Self>,
            value: impl AsRef<str>,
            count: usize,
        ) -> RecordCountFuture {
            let msg = value.as_ref().to_string();
            RecordCountFuture {
                count_type: RecordCountFutureType::ForMessage { msg },
                threshold: count,
                records: self.clone(),
            }
        }

        fn total_message_count(&self) -> usize {
            let records = self.inner.read();
            records.values().map(|values| {
                values.values().sum::<usize>()
            }).sum()
        }

        fn wait_for_total_message_count(
            self: &Arc<Self>,
            count: usize,
        ) -> RecordCountFuture {
            RecordCountFuture {
                count_type: RecordCountFutureType::Total,
                threshold: count,
                records: self.clone(),
            }
        }

        fn clear(&self) {
            self.inner.write().clear();
        }
    }

    enum RecordCountFutureType {
        ForConnection {
            connection: Connection,
            msg: String,
        },
        ForMessage {
            msg: String,
        },
        Total,
    }

    struct RecordCountFuture {
        count_type: RecordCountFutureType,
        threshold: usize,
        records: Arc<Records>,
    }

    impl Future for RecordCountFuture {
        type Output = usize;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut wakers = self.records.wakers.lock();

            let count = match &self.count_type {
                RecordCountFutureType::ForConnection { connection, msg } =>
                    self.records.message_count_for_connection(*connection, msg),
                RecordCountFutureType::ForMessage { msg } =>
                    self.records.message_count(msg),
                RecordCountFutureType::Total =>
                    self.records.total_message_count(),
            };

            if count >= self.threshold {
                Poll::Ready(count)
            } else {
                wakers.push(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    struct NetworkingClosedFuture<D: NetDriver> {
        net: Arc<Networking<D>>,
        connection: Connection,
        #[allow(unused)]
        event_handler: Arc<dyn EventHandler<NetworkingDisconnectEvent>>,
        waker: Arc<Mutex<Waker>>,
    }

    impl<D: NetDriver> Future for NetworkingClosedFuture<D> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut waker = self.waker.lock();
            if self.net.connection_info(&self.connection).is_none() {
                Poll::Ready(())
            } else {
                *waker = cx.waker().clone();
                Poll::Pending
            }
        }
    }

    /// Wrapper around [Networking] that enables enhanced testing capabilities.
    ///
    /// Provides access to the wrapped [Networking] instance, but also provides methods to assert
    /// on various conditions or actions.
    #[derive(Educe)]
    #[educe(Debug)]
    pub struct NetTest<D: NetDriver> {
        #[educe(Debug(method(std::fmt::Display::fmt)))]
        id: NetTestId,
        #[educe(Debug(ignore))]
        net: Arc<Networking<D>>,
        records: Arc<Records>,
    }

    impl<D: NetDriver> NetTest<D> {
        pub fn new(driver_factory: impl NetDriverFactory<D>) -> Self {
            let id = NetTestId::new();
            let net = Arc::new(Networking::new(driver_factory));
            let records = Arc::new(Records::new(id));

            let handler_records = records.clone();
            net.insert_msg_handler(move |connection, msg: Message<TestMessage>| {
                let value = msg.body().value.clone();
                handler_records.record_message(connection, value);
                Ok::<_, Infallible>(())
            });

            Self {
                id,
                net,
                records,
            }
        }

        #[inline]
        pub fn net(&self) -> &Arc<Networking<D>> {
            &self.net
        }

        /// Waits for the given [Connection] to be closed.
        ///
        /// If the [Connection] is already closed (or never opened), yields immediately.
        ///
        /// Does not actively close the connection if it is open; only waits for it to be closed.
        pub async fn wait_for_connection_closed(&self, connection: Connection) {
            let waker = Arc::new(Mutex::new(Waker::noop().clone()));
            let event_waker = waker.clone();
            let event_handler = Arc::new(move |_: &mut Event<NetworkingDisconnectEvent>| {
                event_waker.lock().wake_by_ref();
            });
            self.net().event_bus().register(Arc::downgrade(&event_handler))
                .expect("NetworkingDisconnectEvent type should be valid");
            NetworkingClosedFuture {
                net: self.net.clone(),
                connection,
                event_handler: event_handler as _,
                waker,
            }.await;
        }

        /// Clear all message records.
        pub fn clear_records(&self) {
            self.records.clear();
        }

        /// Asserts that the total amount of messages that have been received by this [NetTest]
        /// equals the given `count`.
        pub fn assert_total_message_count(&self, count: usize) {
            assert_eq!(self.records.total_message_count(), count);
        }

        /// Waits for the total amount of messages that have been received by this [NetTest] to
        /// reach the given `count`.
        ///
        /// If the message count already meets or exceeds the given `count`, yields immediately.
        pub async fn wait_for_total_message_count(&self, count: usize) {
            self.records.wait_for_total_message_count(count).await;
        }

        /// Asserts that the amount of messages received that contained the given `msg` value equals
        /// the given `count`.
        pub fn assert_message_count(&self, msg: impl AsRef<str>, count: usize) {
            assert_eq!(self.records.message_count(msg), count);
        }

        /// Waits for the amount of messages received that contain the given `msg` to reach the
        /// given `count`.
        ///
        /// If the message count already meets or exceeds the given `count`, yields immediately.
        pub async fn wait_for_message_count(
            &self,
            msg: impl AsRef<str>,
            count: usize,
        ) {
            self.records.wait_for_message_count(msg, count).await;
        }

        /// Asserts that the amount of messages received for `connection` that contain the given
        /// `msg` value equals the given `count`.
        pub fn assert_connection_message_count(
            &self,
            connection: Connection,
            msg: impl AsRef<str>,
            count: usize,
        ) {
            assert_eq!(self.records.message_count_for_connection(connection, msg), count);
        }

        /// Waits for the amount of messages received for `connection` that contain the given `msg`
        /// value to reach the given `count`.
        ///
        /// If the message count already meets or exceeds the given `count`, yields immediately.
        pub async fn wait_for_connection_message_count(
            &self,
            connection: Connection,
            msg: impl AsRef<str>,
            count: usize,
        ) {
            self.records.wait_for_message_count_for_connection(connection, msg, count).await;
        }
    }

    /// Extension trait for slices of [NetTest].
    ///
    /// Implements many of the [NetTest] methods on slices, effectively executing those methods for
    /// each [NetTest] in the slice.
    #[async_trait(?Send)]
    pub trait NetTestSliceExt {
        /// Clear message records for all [NetTests](NetTest).
        fn clear_records(&self);

        /// Asserts the total message count for each [NetTest].
        ///
        /// See [NetTest::assert_total_message_count()].
        fn assert_total_message_count_for_each(&self, count: usize);

        /// Waits for the total amount of messages for each [NetTest].
        ///
        /// See [NetTest::wait_for_total_message_count()].
        async fn wait_for_total_message_count_for_each(&self, count: usize);

        /// Asserts the per-message count for each [NetTest].
        ///
        /// See [NetTest::assert_message_count()].
        fn assert_message_count_for_each(&self, msg: impl AsRef<str>, count: usize);

        /// Waits for the per-message count for each [NetTest].
        ///
        /// See [NetTest::wait_for_message_count()].
        async fn wait_for_message_count_for_each(&self, msg: impl AsRef<str>, count: usize);

        /// Asserts the per-connection, per-message count for each [NetTest].
        ///
        /// See [NetTest::assert_connection_message_count()].
        fn assert_connection_message_count_for_each(
            &self,
            connection: Connection,
            msg: impl AsRef<str>,
            count: usize,
        );

        /// Waits for the per-connection, per-message count for each [NetTest].
        ///
        /// See [NetTest::wait_for_connection_message_count()].
        async fn wait_for_connection_message_count_for_each(
            &self,
            connection: Connection,
            msg: impl AsRef<str>,
            count: usize,
        );
    }

    #[async_trait(?Send)]
    impl<D: NetDriver> NetTestSliceExt for [NetTest<D>] {
        fn clear_records(&self) {
            for net in self {
                net.clear_records();
            }
        }

        fn assert_total_message_count_for_each(&self, count: usize) {
            for net in self {
                net.assert_total_message_count(count);
            }
        }

        async fn wait_for_total_message_count_for_each(&self, count: usize) {
            execute_futures(self.iter().map(|net| {
                net.wait_for_total_message_count(count)
            })).await;
        }

        fn assert_message_count_for_each(&self, msg: impl AsRef<str>, count: usize) {
            for net in self {
                net.assert_message_count(msg.as_ref(), count);
            }
        }

        async fn wait_for_message_count_for_each(&self, msg: impl AsRef<str>, count: usize) {
            execute_futures(self.iter().map(|net| {
                net.wait_for_message_count(msg.as_ref(), count)
            })).await;
        }

        fn assert_connection_message_count_for_each(
            &self,
            connection: Connection,
            msg: impl AsRef<str>,
            count: usize,
        ) {
            for net in self {
                net.assert_connection_message_count(connection, msg.as_ref(), count);
            }
        }

        async fn wait_for_connection_message_count_for_each(
            &self,
            connection: Connection,
            msg: impl AsRef<str>,
            count: usize,
        ) {
            execute_futures(self.iter().map(|net| {
                net.wait_for_connection_message_count(connection, msg.as_ref(), count)
            })).await;
        }
    }

    /// Get an iterator for all valid socket ports to use for testing.
    ///
    /// This is configured via the environment variable `GTETHER_TEST_NET_DRIVER_PORTS`, using the
    /// format "<start>:<end>" (inclusive start, exclusive end). For example:
    /// ```text
    /// export GTETHER_TEST_NET_DRIVER_PORTS="9001:10000"
    /// ```
    ///
    /// If that environment variable is not set, or cannot be parsed, the default port range is
    /// `59000:60000`.
    pub fn testing_ports_from_env() -> impl Iterator<Item=u16> {
        let var_str = std::env::var("GTETHER_TEST_NET_DRIVER_PORTS")
            .unwrap_or("59000:60000".to_owned());
        let (start, end) = var_str.split_once(':')
            .unwrap_or(("59000", "60000"));
        let start = start.parse::<u16>().unwrap_or(59000);
        let end = end.parse::<u16>().unwrap_or(60000);
        start..end
    }

    impl<D: NetDriverListen> NetTest<D> {
        /// Asserts that a [NetTest] using a [NetDriverListen] driver can bind to a socket.
        ///
        /// Uses [testing_ports_from_env()] to iterate through and find a free port to bind to. If
        /// the chosen port is in use, will attempt to use the next port, and so on, until all ports
        /// from [testing_ports_from_env()] have been attempted. If no ports can be used, will
        /// panic.
        ///
        /// Returns the [SocketAddr] that was successfully listened on.
        pub async fn assert_listen(&self) -> SocketAddr {
            for port in testing_ports_from_env() {
                let socket_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);

                match self.net.listen(socket_addr).await {
                    Ok(_) => return socket_addr,
                    Err(NetworkingError::InvalidAddress(_)) => continue,
                    result => result.expect("Listen should succeed"),
                }
            }
            panic!("No port available to bind/listen on")
        }

        /// Asserts that a [NetTest] using a [NetDriverListen] driver errors in the expected way
        /// when attempting to bind to a socket.
        pub async fn assert_listen_error(
            &self,
            socket_addr: SocketAddr,
            expected_error: NetworkingError,
        ) {
            let error = self.net.listen(socket_addr).await
                .expect_err("Listen should fail");
            assert_networking_error_eq(error, &expected_error);
        }
    }

    impl<S: NetDriverConnect> NetTest<S> {
        /// Asserts that a [NetTest] using a [NetDriverConnect] driver can connect to a server.
        ///
        /// The given `dst` [NetTest] is used to determine the [Connection] ID from the server's
        /// side once connected. The given `socket_addr` should be the same address that `dst` is
        /// listening on. If it does not match, this method may wait indefinitely waiting for a
        /// connection that will not occur.
        ///
        /// Returns a [ConnectionPair] describing the successful connection.
        pub async fn assert_connect<'a, D: NetDriver>(
            &'a self,
            dst: &'a NetTest<D>,
            socket_addr: SocketAddr,
        ) -> ConnectionPair<'a, S, D> {
            let server_connection = Arc::new(Mutex::new(None));
            let event_server_connection = server_connection.clone();
            let fut = dst.net().event_bus().register_once(move |event: &mut Event<NetworkingConnectEvent>| {
                *event_server_connection.lock() = Some(event.connection());
            }).expect("NetworkingConnectEvent type should be valid");

            let src_to_dst = self.net.connect(socket_addr).await
                .expect("src should connect")
                .into_connection();
            fut.await.expect("NetworkingConnectEvent should fire successfully");

            let dst_to_src = server_connection.lock().take()
                .expect("server_connection should be set");

            ConnectionPair {
                src: TestConnection {
                    net: self,
                    connection: src_to_dst,
                },
                dst: TestConnection {
                    net: dst,
                    connection: dst_to_src,
                },
            }
        }
    }

    /// Extension trait for slices of [NetTest] that use a [NetDriverConnect] driver.
    ///
    /// Implements many of the [NetTest] methods on slices, effectively executing those methods for
    /// each [NetTest] in the slice.
    #[async_trait(?Send)]
    pub trait NetTestConnectSliceExt<S: NetDriverConnect> {
        /// Asserts that each client [NetTest] can connect to the given server [NetTest].
        ///
        /// See [NetTest::assert_connect()].
        async fn assert_connect<'a, D: NetDriver>(
            &'a self,
            dst: &'a NetTest<D>,
            socket_addr: SocketAddr,
        ) -> Vec<ConnectionPair<'a, S, D>>;

        /// Asserts that each client [NetTest] errors in the expected way when connecting.
        async fn assert_connect_error(
            &self,
            socket_addr: SocketAddr,
            expected_error: NetworkingError,
        );
    }

    #[async_trait(?Send)]
    impl<S: NetDriverConnect> NetTestConnectSliceExt<S> for [NetTest<S>] {
        async fn assert_connect<'a, D: NetDriver>(
            &'a self,
            dst: &'a NetTest<D>,
            socket_addr: SocketAddr,
        ) -> Vec<ConnectionPair<'a, S, D>> {
            let mut srcs_out = Vec::with_capacity(self.len());
            for src in self {
                // NOTE: These cannot run in parallel, otherwise the logic that determines dst_to_src
                //  will break
                let pair = src.assert_connect(dst, socket_addr).await;
                srcs_out.push(pair);
            }
            srcs_out
        }

        async fn assert_connect_error(
            &self,
            socket_addr: SocketAddr,
            expected_error: NetworkingError,
        ) {
            for src in self {
                let error = src.net().connect(socket_addr).await
                    .expect_err("Connect should fail");
                assert_networking_error_eq(error, &expected_error);
            }
        }
    }

    impl<D: NetDriverSend> NetTest<D> {
        /// Installs a message reply handler using a [NetDriverSend] driver.
        pub fn init_test_message_repliable_send_handler(
            &self,
            reply_fn: impl (Fn(String) -> String) + Send + Sync + 'static,
        ) {
            let handler_net = self.net.clone();
            let handler_records = self.records.clone();
            self.net.insert_msg_handler(move |connection, msg: Message<TestMessageRepliable>| {
                let value = msg.body().value.clone();
                handler_records.record_message(connection, value.clone());

                let reply = msg.reply(TestMessage {
                    value: reply_fn(value),
                });
                handler_net.send(reply)
            });
        }
    }

    impl<D: NetDriverSendTo> NetTest<D> {
        /// Installs a message reply handler using a [NetDriverSendTo] driver.
        pub fn init_test_message_repliable_send_to_handler(
            &self,
            reply_fn: impl (Fn(String) -> String) + Send + Sync + 'static,
        ) {
            let handler_net = self.net.clone();
            let handler_records = self.records.clone();
            self.net.insert_msg_handler(move |connection, msg: Message<TestMessageRepliable>| {
                let value = msg.body().value.clone();
                handler_records.record_message(connection, value.clone());

                let reply = msg.reply(TestMessage {
                    value: reply_fn(value),
                });
                handler_net.send_to(connection, reply)
            });
        }
    }

    impl<D: NetDriverBroadcast> NetTest<D> {
        /// Asserts that a [NetTest] using a [NetDriverBroadcast] driver can broadcast a message.
        pub fn assert_broadcast(&self, msg: impl Into<String>) {
            let msg = TestMessage::new(msg.into());
            self.net.broadcast(msg)
                .expect("Message should broadcast");
        }

        /// Asserts that a [NetTest] using a [NetDriverBroadcast] driver errors in the expected way
        /// when attempting to broadcast a message.
        pub fn assert_broadcast_error(&self, msg: impl Into<String>, expected_error: &NetworkingError) {
            let msg = TestMessage::new(msg.into());
            let error = self.net.broadcast(msg)
                .expect_err("Message should NOT broadcast");
            assert_networking_error_eq(error, expected_error);
        }
    }

    /// Extension trait for slices of [NetTest] that use a [NetDriverBroadcast] driver.
    ///
    /// Implements many of the [NetTest] methods on slices, effectively executing those methods for
    /// each [NetTest] in the slice.
    pub trait NetTestBroadcastSliceExt {
        /// Asserts that each [NetTest] can broadcast a message.
        ///
        /// See [NetTest::assert_broadcast()].
        fn assert_broadcast_all(&self, msg: impl Into<String>);

        /// Asserts that each [NetTest] errors in the expected way when broadcasting a message.
        ///
        /// See [NetTest::assert_broadcast_error()].
        fn assert_broadcast_error_all(&self, msg: impl Into<String>, expected_error: &NetworkingError);
    }

    impl<D: NetDriverBroadcast> NetTestBroadcastSliceExt for [NetTest<D>] {
        fn assert_broadcast_all(&self, msg: impl Into<String>) {
            let msg = msg.into();
            for net in self {
                net.assert_broadcast(&msg);
            }
        }

        fn assert_broadcast_error_all(&self, msg: impl Into<String>, expected_error: &NetworkingError) {
            let msg = msg.into();
            for net in self {
                net.assert_broadcast_error(&msg, expected_error);
            }
        }
    }

    /// Struct representing an actively connected [NetTest] and it's [Connection].
    /// 
    /// The internal [NetTest] is held by reference, meaning this struct is cheaply cloneable.
    pub struct TestConnection<'a, D: NetDriver> {
        pub net: &'a NetTest<D>,
        pub connection: Connection,
    }

    impl<'a, D: NetDriver> Debug for TestConnection<'a, D> {
        fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
            f.debug_struct("TestConnection")
                .field("net", &self.net)
                .field("connection", &self.connection)
                .field("closed", &self.net.net.connection_info(&self.connection).is_none())
                .finish()
        }
    }

    impl<'a, D: NetDriver> Clone for TestConnection<'a, D> {
        fn clone(&self) -> Self {
            Self {
                net: self.net,
                connection: self.connection,
            }
        }
    }

    impl<'a, D: NetDriver> TestConnection<'a, D> {
        /// Assert that this connection has been closed.
        #[inline]
        pub fn assert_closed(&self) {
            assert!(self.net.net.connection_info(&self.connection).is_none());
        }

        /// Wait for this connection to close.
        ///
        /// Does not actively close the connection if it is open; only waits for it to be closed.
        #[inline]
        pub async fn wait_for_closed(&self) {
            self.net.wait_for_connection_closed(self.connection).await;
        }
    }

    /// Extension trait for slices of [TestConnection].
    ///
    /// Implements many of the [TestConnection] methods on slices, effectively executing those
    /// methods for each [TestConnection] in the slice.
    #[async_trait(?Send)]
    pub trait TestConnectionSliceExt {
        /// Assert that each connection has been closed.
        fn assert_all_closed(&self);

        /// Wait for each connection to close.
        ///
        /// Does not actively close connections if they are open; only waits for them to be closed.
        async fn wait_for_all_closed(&self);
    }

    #[async_trait(?Send)]
    impl<'a, D: NetDriver> TestConnectionSliceExt for [TestConnection<'a, D>] {
        #[inline]
        fn assert_all_closed(&self) {
            for connection in self {
                connection.assert_closed();
            }
        }

        #[inline]
        async fn wait_for_all_closed(&self) {
            execute_futures(self.iter().map(TestConnection::wait_for_closed)).await;
        }
    }

    impl<'a, D: NetDriverSend> TestConnection<'a, D> {
        /// Asserts that a [NetTest] can send a message.
        pub fn assert_send(&self, msg: impl Into<String>) {
            let msg = TestMessage::new(msg.into());
            self.net.net.send(msg)
                .expect("Message should send");
        }

        /// Asserts that a [NetTest] errors in the expected way when attempting to send a message.
        pub fn assert_send_error(&self, msg: impl Into<String>, expected_error: &NetworkingError) {
            let msg = TestMessage::new(msg.into());
            let error = self.net.net.send(msg)
                .expect_err("Message should NOT send");
            assert_networking_error_eq(error, expected_error);
        }

        /// Asserts that a [NetTest] can send a repliable message and receive a response.
        pub async fn assert_send_recv(
            &self,
            msg: impl Into<String>,
            expected_reply: impl Into<String>,
        ) {
            let msg = TestMessageRepliable::new(msg.into());
            let reply = self.net.net.send_recv(msg)
                .expect("Message should send")
                .await;
            let msg_reply = reply.into_body().value;
            let expected_reply = expected_reply.into();
            assert_eq!(msg_reply, expected_reply);
            // Replies bypass normal handlers, so manually record
            self.net.records.record_message(self.connection, msg_reply);
        }
    }

    /// Extension trait for slices of [TestConnection] that use a [NetDriverSend] driver.
    ///
    /// Implements many of the [TestConnection] methods on slices, effectively executing those
    /// methods for each [TestConnection] in the slice.
    #[async_trait(?Send)]
    pub trait TestConnectionSendSliceExt {
        /// Asserts that each [NetTest] can send a message.
        fn assert_send_all(&self, msg: impl Into<String>);

        /// Asserts that each [NetTest] errors in the expected way when attempting to send a
        /// message.
        fn assert_send_error_all(&self, msg: impl Into<String>, expected_error: &NetworkingError);

        /// Asserts that each [NetTest] can send a repliable message and receive a response.
        async fn assert_send_recv_all(
            &self,
            msg: impl Into<String>,
            expected_reply: impl Into<String>,
        );
    }

    #[async_trait(?Send)]
    impl <'a, D: NetDriverSend> TestConnectionSendSliceExt for [TestConnection<'a, D>] {
        fn assert_send_all(&self, msg: impl Into<String>) {
            let msg = msg.into();
            for connection in self {
                connection.assert_send(&msg);
            }
        }

        fn assert_send_error_all(&self, msg: impl Into<String>, expected_error: &NetworkingError) {
            let msg = msg.into();
            for connection in self {
                connection.assert_send_error(&msg, expected_error);
            }
        }

        async fn assert_send_recv_all(
            &self,
            msg: impl Into<String>,
            expected_reply: impl Into<String>,
        ) {
            let msg = msg.into();
            let expected_reply = expected_reply.into();
            execute_futures(self.iter().map(|cn| {
                cn.assert_send_recv(&msg, &expected_reply)
            })).await;
        }
    }

    impl<'a, D: NetDriverSendTo> TestConnection<'a, D> {
        /// Asserts that a [NetTest] can send a message to a specific [Connection].
        ///
        /// The [Connection] used is the one associated with this [TestConnection].
        pub fn assert_send_to(&self, msg: impl Into<String>) {
            let msg = TestMessage::new(msg.into());
            self.net.net.send_to(self.connection, msg)
                .expect("Message should send");
        }

        /// Asserts that a [NetTest] errors in the expected way when attempting to send a message
        /// to a [Connection].
        ///
        /// The [Connection] used is the one associated with this [TestConnection].
        pub fn assert_send_to_error(&self, msg: impl Into<String>, expected_error: &NetworkingError) {
            let msg = TestMessage::new(msg.into());
            let error = self.net.net.send_to(self.connection, msg)
                .expect_err("Message should NOT send");
            assert_networking_error_eq(error, expected_error);
        }

        /// Asserts that a [NetTest] can send a repliable message to a specific [Connection] and
        /// receive a response.
        ///
        /// The [Connection] used is the one associated with this [TestConnection].
        pub async fn assert_send_recv_to(
            &self,
            msg: impl Into<String>,
            expected_reply: impl Into<String>,
        ) {
            let msg = TestMessageRepliable::new(msg.into());
            let reply = self.net.net.send_recv_to(self.connection, msg)
                .expect("Message should send")
                .await;
            let msg_reply = reply.into_body().value;
            let expected_reply = expected_reply.into();
            assert_eq!(msg_reply, expected_reply);
            // Replies bypass normal handlers, so manually record
            self.net.records.record_message(self.connection, msg_reply);
        }
    }

    /// Extension trait for slices of [TestConnection] that use a [NetDriverSendTo] driver.
    ///
    /// Implements many of the [TestConnection] methods on slices, effectively executing those
    /// methods for each [TestConnection] in the slice.
    #[async_trait(?Send)]
    pub trait TestConnectionSendToSliceExt {
        /// Asserts that each [NetTest] can send messages to a specific [Connection].
        ///
        /// See [TestConnection::assert_send_to()].
        fn assert_send_to_all(&self, msg: impl Into<String>);

        /// Asserts that each [NetTest] errors in the expected way when attempting to send a message
        /// to a [Connection].
        ///
        /// See [TestConnection::assert_send_to_error_all()].
        fn assert_send_to_error_all(&self, msg: impl Into<String>, expected_error: &NetworkingError);

        /// Asserts that each [NetTest] can send a repliable message to a specific [Connection] and
        /// receive a response.
        ///
        /// See [TestConnection::assert_send_recv_to_all()].
        async fn assert_send_recv_to_all(
            &self,
            msg: impl Into<String>,
            expected_reply: impl Into<String>,
        );
    }

    #[async_trait(?Send)]
    impl <'a, D: NetDriverSendTo> TestConnectionSendToSliceExt for [TestConnection<'a, D>] {
        fn assert_send_to_all(&self, msg: impl Into<String>) {
            let msg = msg.into();
            for connection in self {
                connection.assert_send_to(&msg);
            }
        }

        fn assert_send_to_error_all(&self, msg: impl Into<String>, expected_error: &NetworkingError) {
            let msg = msg.into();
            for connection in self {
                connection.assert_send_to_error(&msg, expected_error);
            }
        }

        async fn assert_send_recv_to_all(
            &self,
            msg: impl Into<String>,
            expected_reply: impl Into<String>,
        ) {
            let msg = msg.into();
            let expected_reply = expected_reply.into();
            execute_futures(self.iter().map(|cn| {
                cn.assert_send_recv_to(&msg, &expected_reply)
            })).await;
        }
    }

    /// A pair of [TestConnections](TestConnection) representing the two ends of an active
    /// connection.
    #[derive(Clone, Educe)]
    #[educe(Debug)]
    pub struct ConnectionPair<'a, S: NetDriver, D: NetDriver> {
        pub src: TestConnection<'a, S>,
        pub dst: TestConnection<'a, D>,
    }

    impl<'a, S: NetDriver, D: NetDriver> ConnectionPair<'a, S, D> {
        /// Close the `src` side and wait for the `dst` to acknowledge the closure.
        pub async fn close_src_and_wait(&self) {
            self.src.net.net.close().await;
            self.src.assert_closed();
            self.dst.wait_for_closed().await;
        }

        /// Close the `dst` side and wait for the `src` to acknowledge the closure.
        pub async fn close_dst_and_wait(&self) {
            self.dst.net.net.close().await;
            self.dst.assert_closed();
            self.src.wait_for_closed().await;
        }

        /// Flip the representation of this [ConnectionPair], such that the new pair uses the
        /// original `src` as `dst`, and the original `dst` as `src`. 
        pub fn flip(&self) -> ConnectionPair<'a, D, S> {
            ConnectionPair {
                src: self.dst.clone(),
                dst: self.src.clone(),
            }
        }
    }

    /// Extension trait for slices of [ConnectionPair].
    /// 
    /// Implements many of the [ConnectionPair] methods on slices, effectively executing those
    /// methods for each [ConnectionPair] in the slice.
    #[async_trait(?Send)]
    pub trait ConnectionPairSliceExt<'a, S: NetDriver, D: NetDriver> {
        /// Collect only all the `src` [TestConnections](TestConnection).
        fn srcs(&self) -> Vec<TestConnection<'a, S>>;
        
        /// Close each `src` side and wait for each `dst` to acknowledge the closure.
        async fn close_all_srcs_and_wait(&self);
        
        /// Collect only all the `dst` [TestConnections](TestConnection).
        fn dsts(&self) -> Vec<TestConnection<'a, D>>;

        /// Close each `dst` side and wait for each `src` to acknowledge the closure.
        async fn close_all_dsts_and_wait(&self);
        
        /// Flip the representation of each [ConnectionPair], such that the new pair uses the
        /// original `src` as `dst`, and the original `dst` as `src`. 
        fn flip(&self) -> Vec<ConnectionPair<'a, D, S>>;
    }

    #[async_trait(?Send)]
    impl<'a, S: NetDriver, D: NetDriver> ConnectionPairSliceExt<'a, S, D> for [ConnectionPair<'a, S, D>] {
        #[inline]
        fn srcs(&self) -> Vec<TestConnection<'a, S>> {
            self.iter()
                .map(|entry| entry.src.clone())
                .collect()
        }

        #[inline]
        async fn close_all_srcs_and_wait(&self) {
            execute_futures(self.iter().map(ConnectionPair::close_src_and_wait)).await;
        }

        #[inline]
        fn dsts(&self) -> Vec<TestConnection<'a, D>> {
            self.iter()
                .map(|entry| entry.dst.clone())
                .collect()
        }

        #[inline]
        async fn close_all_dsts_and_wait(&self) {
            execute_futures(self.iter().map(ConnectionPair::close_dst_and_wait)).await;
        }

        #[inline]
        fn flip(&self) -> Vec<ConnectionPair<'a, D, S>> {
            self.iter()
                .map(ConnectionPair::flip)
                .collect()
        }
    }

    /// Factory trait for creating an entire client/server collection of [Networking] stacks.
    pub trait ClientServerStackFactory: Default
    {
        /// The type of the client [NetDriver].
        type ClientDriver: NetDriverConnect;
        
        /// The type of the client [NetDriverFactory].
        type ClientDriverFactory: NetDriverFactory<Self::ClientDriver>;
        
        /// The type of the server [NetDriver].
        type ServerDriver: NetDriverListen;
        
        /// The type of the server [NetDriverFactory].
        type ServerDriverFactory: NetDriverFactory<Self::ServerDriver>;

        /// Create a client [NetDriverFactory].
        fn client_factory(&self) -> Self::ClientDriverFactory;
        
        /// Create a server [NetDriverFactory].
        fn server_factory(&self) -> Self::ServerDriverFactory;
    }

    /// Extension trait for additional functionality applied to [ClientServerStackFactory]
    /// implementations.
    pub trait ClientServerStackFactoryExt: ClientServerStackFactory {
        /// Create a client [NetTest].
        #[inline]
        fn create_client(&self) -> NetTest<Self::ClientDriver> {
            NetTest::new(self.client_factory())
        }

        /// Create a server [NetTest].
        #[inline]
        fn create_server(&self) -> NetTest<Self::ServerDriver> {
            NetTest::new(self.server_factory())
        }

        /// Create `CLIENT_COUNT` client [NetTests](NetTest), and one server [NetTest].
        /// 
        /// These [NetTests](NetTest) are _NOT_ connected to each other by default.
        fn create_stacks<const CLIENT_COUNT: usize>(
            &self,
        ) -> ([NetTest<Self::ClientDriver>; CLIENT_COUNT], NetTest<Self::ServerDriver>) {
            let clients: [NetTest<Self::ClientDriver>; CLIENT_COUNT] = core::array::from_fn(|_| {
                NetTest::new(self.client_factory())
            });

            let server = NetTest::new(self.server_factory());

            (clients, server)
        }
    }

    impl<F: ClientServerStackFactory> ClientServerStackFactoryExt for F {}

    /// Pre-defined test suites.
    /// 
    /// This module contains several pre-defined suites of tests that ensure implementations
    /// maintain the expected behavior of [NetDriver]'s. Usually, these test suites are not used
    /// directly, as they do not exist in individual function form. Instead, it is recommended to
    /// use the macros found in the [super module](super), such as
    /// [`test_net_driver_client_server_core!`].
    pub mod suites {
        use super::*;

        /// Client/Server test suite.
        /// 
        /// It is recommended to use the following macros instead of using this test suite directly:
        ///  * [`test_net_driver_client_server_core!`]
        ///  * [`test_net_driver_client_server_send!`]
        ///  * [`test_net_driver_client_server_send_to!`]
        ///  * [`test_net_driver_client_server_broadcast!`]
        #[derive(Default)]
        pub struct ClientServer<F>
        where
            F: ClientServerStackFactory,
        {
            stack_factory: F,
        }

        impl<F> ClientServer<F>
        where
            F: ClientServerStackFactory,
        {
            pub fn test_connect(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();

                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    Some(format_args!("{server:#?}")),
                );
                timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    Some(format_args!("{clients:#?}")),
                );
            }

            pub fn test_listen_already_bound(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let server = self.stack_factory.create_server();
                let server2 = self.stack_factory.create_server();

                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server 1 listening",
                    Some(format_args!("{server:#?}")),
                );
                timeout.run(
                    server2.assert_listen_error(
                        socket_addr,
                        NetworkingError::InvalidAddress(socket_addr),
                    ),
                    "Server 2 listening",
                    Some(format_args!("{server2:#?}")),
                );
            }

            pub fn test_connect_cancelled(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    Some(format_args!("{server:#?}")),
                );

                for client in &clients {
                    client.net().event_bus().register(move |event: &mut Event<NetworkingConnectEvent>| {
                        event.cancel();
                    }).expect("NetworkingConnectEvent type should be valid");
                }

                timeout.run(
                    clients.assert_connect_error(
                        socket_addr,
                        NetworkingError::Cancelled,
                    ),
                    "Clients connecting",
                    Some(format_args!("{clients:#?}")),
                );
            }

            pub fn test_connect_after_close(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    Some(format_args!("{server:#?}")),
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    Some(format_args!("{clients:#?}")),
                );

                timeout.run(
                    connections.close_all_srcs_and_wait(),
                    "Closing connections from clients",
                    Some(format_args!("{connections:#?}")),
                );
            }
        }

        impl<F> ClientServer<F>
        where
            F: ClientServerStackFactory,
            <F as ClientServerStackFactory>::ClientDriver: NetDriverSend,
        {
            pub fn test_send(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                );

                connections.srcs().assert_send_all("client->server");

                timeout.run(
                    server.wait_for_total_message_count(clients.len()),
                    "Waiting for messages",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_message_count("client->server", clients.len());
                clients.assert_total_message_count_for_each(0);
            }

            pub fn test_send_many(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<3>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                );

                connections.srcs().assert_send_all("client->server");

                timeout.run(
                    server.wait_for_total_message_count(clients.len()),
                    "Waiting for messages",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_message_count("client->server", clients.len());
                clients.assert_total_message_count_for_each(0);
            }

            pub fn test_send_closed_src(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                );

                timeout.run(
                    connections.close_all_srcs_and_wait(),
                    "Closing clients",
                    Some(format_args!("{clients:#?}")),
                );
                connections.srcs().assert_send_error_all("client->server", &NetworkingError::Closed);

                server.assert_total_message_count(0);
                clients.assert_total_message_count_for_each(0);
            }

            pub fn test_send_closed_dst(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                );

                timeout.run(
                    connections.close_all_dsts_and_wait(),
                    "Closing server",
                    Some(format_args!("{server:#?}")),
                );
                connections.srcs().assert_send_error_all("client->server", &NetworkingError::Closed);

                server.assert_total_message_count(0);
                clients.assert_total_message_count_for_each(0);
            }

            pub fn test_send_closed_partial(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<3>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                );

                timeout.run(
                    connections[0].close_src_and_wait(),
                    "Closing client 0",
                    Some(format_args!("{clients:#?}")),
                );
                connections[1..].srcs().assert_send_all("client->server");

                timeout.run(
                    server.wait_for_total_message_count(clients.len() - 1),
                    "Waiting for messages",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_message_count("client->server", clients.len() - 1);
                clients.assert_total_message_count_for_each(0);
            }
        }

        impl<F> ClientServer<F>
        where
            F: ClientServerStackFactory,
            <F as ClientServerStackFactory>::ServerDriver: NetDriverSendTo,
        {
            pub fn test_send_to(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                connections.srcs().assert_send_to_all("server->client");

                timeout.run(
                    clients.wait_for_total_message_count_for_each(1),
                    "Waiting for messages",
                    Some(format_args!("{connections:#?}")),
                );
                clients.assert_message_count_for_each("server->client", 1);
                server.assert_total_message_count(0);
            }

            pub fn test_send_to_many(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<3>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                connections.srcs().assert_send_to_all("server->client");

                timeout.run(
                    clients.wait_for_total_message_count_for_each(1),
                    "Waiting for messages",
                    Some(format_args!("{connections:#?}")),
                );
                clients.assert_message_count_for_each("server->client", 1);
                server.assert_total_message_count(0);
            }

            pub fn test_send_to_closed_src(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                timeout.run(
                    connections.close_all_srcs_and_wait(),
                    "Closing server",
                    Some(format_args!("{clients:#?}")),
                );
                connections.srcs().assert_send_to_error_all("server->client", &NetworkingError::Closed);

                clients.assert_total_message_count_for_each(0);
                server.assert_total_message_count(0);
            }

            pub fn test_send_to_closed_dst(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                timeout.run(
                    connections.close_all_dsts_and_wait(),
                    "Closing clients",
                    Some(format_args!("{clients:#?}")),
                );
                for src in connections.srcs() {
                    let connection = src.connection;
                    src.assert_send_to_error("server->client", &NetworkingError::InvalidConnection(connection));
                }

                clients.assert_total_message_count_for_each(0);
                server.assert_total_message_count(0);
            }

            pub fn test_send_to_closed_partial(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                timeout.run(
                    connections[0].close_dst_and_wait(),
                    "Closing client 0",
                    Some(format_args!("{clients:#?}")),
                );
                connections[1..].srcs().assert_send_to_all("server->client");

                timeout.run(
                    clients[1..].wait_for_total_message_count_for_each(1),
                    "Waiting for messages",
                    Some(format_args!("{connections:#?}")),
                );
                clients[0].assert_total_message_count(0);
                clients[1..].assert_message_count_for_each("server->client", 1);
                server.assert_total_message_count(0);
            }

            pub fn test_send_to_invalid_connection(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let mut connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                for connection in &mut connections {
                    connection.src.connection = Connection::INVALID;
                }

                connections.srcs().assert_send_to_error_all(
                    "server->client",
                    &NetworkingError::InvalidConnection(Connection::INVALID),
                );

                server.assert_total_message_count(0);
                clients.assert_total_message_count_for_each(0);
            }
        }

        impl<F> ClientServer<F>
        where
            F: ClientServerStackFactory,
            <F as ClientServerStackFactory>::ClientDriver: NetDriverSend,
            <F as ClientServerStackFactory>::ServerDriver: NetDriverSendTo,
        {
            pub fn test_send_recv(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                );

                server.init_test_message_repliable_send_to_handler(|value| value + "-REPLY");

                timeout.run(
                    connections.srcs().assert_send_recv_all(
                        "client->server",
                        "client->server-REPLY",
                    ),
                    "Waiting for message replies",
                    Some(format_args!("{connections:#?}")),
                );

                timeout.run(
                    server.wait_for_total_message_count(clients.len()),
                    "Waiting for server-side messages",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_message_count("client->server", clients.len());
                timeout.run(
                    clients.wait_for_total_message_count_for_each(1),
                    "Waiting for client-side messages",
                    Some(format_args!("{connections:#?}")),
                );
                clients.assert_message_count_for_each("client->server-REPLY", 1);
            }

            pub fn test_send_recv_many(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<3>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                );

                server.init_test_message_repliable_send_to_handler(|value| value + "-REPLY");

                timeout.run(
                    connections.srcs().assert_send_recv_all(
                        "client->server",
                        "client->server-REPLY",
                    ),
                    "Waiting for message replies",
                    Some(format_args!("{connections:#?}")),
                );

                timeout.run(
                    server.wait_for_total_message_count(clients.len()),
                    "Waiting for server-side messages",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_message_count("client->server", clients.len());
                timeout.run(
                    clients.wait_for_total_message_count_for_each(1),
                    "Waiting for client-side messages",
                    Some(format_args!("{connections:#?}")),
                );
                clients.assert_message_count_for_each("client->server-REPLY", 1);
            }

            pub fn test_send_recv_to(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<1>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                for client in &clients {
                    client.init_test_message_repliable_send_handler(|value| value + "-REPLY");
                }

                timeout.run(
                    connections.srcs().assert_send_recv_to_all(
                        "server->client",
                        "server->client-REPLY",
                    ),
                    "Waiting for message replies",
                    Some(format_args!("{connections:#?}")),
                );

                timeout.run(
                    clients.wait_for_total_message_count_for_each(1),
                    "Waiting for client-side messages",
                    Some(format_args!("{connections:#?}")),
                );
                clients.assert_message_count_for_each("server->client", 1);
                timeout.run(
                    server.wait_for_total_message_count(clients.len()),
                    "Waiting for server-side messages",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_message_count("server->client-REPLY", clients.len());
            }

            pub fn test_send_recv_to_many(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<3>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                for client in &clients {
                    client.init_test_message_repliable_send_handler(|value| value + "-REPLY");
                }

                timeout.run(
                    connections.srcs().assert_send_recv_to_all(
                        "server->client",
                        "server->client-REPLY",
                    ),
                    "Waiting for message replies",
                    Some(format_args!("{connections:#?}")),
                );

                timeout.run(
                    clients.wait_for_total_message_count_for_each(1),
                    "Waiting for client-side messages",
                    Some(format_args!("{connections:#?}")),
                );
                clients.assert_message_count_for_each("server->client", 1);
                timeout.run(
                    server.wait_for_total_message_count(clients.len()),
                    "Waiting for server-side messages",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_message_count("server->client-REPLY", clients.len());
            }
        }

        impl<F> ClientServer<F>
        where
            F: ClientServerStackFactory,
            <F as ClientServerStackFactory>::ServerDriver: NetDriverBroadcast,
        {
            pub fn test_broadcast(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<3>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let _connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                server.assert_broadcast("server->BROADCAST");

                timeout.run(
                    clients.wait_for_total_message_count_for_each(1),
                    "Waiting for messages",
                    Some(format_args!("{clients:#?}")),
                );
                clients.assert_message_count_for_each("server->BROADCAST", 1);
                server.assert_total_message_count(0);
            }

            pub fn test_broadcast_closed_src(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<3>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                timeout.run(
                    connections.close_all_srcs_and_wait(),
                    "Closing connections from server",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_broadcast_error("server->BROADCAST", &NetworkingError::Closed);

                clients.assert_total_message_count_for_each(0);
                server.assert_total_message_count(0);
            }

            pub fn test_broadcast_closed_dst(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<3>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                timeout.run(
                    connections.close_all_dsts_and_wait(),
                    "Closing connections from clients",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_broadcast("server->BROADCAST");

                clients.assert_total_message_count_for_each(0);
                server.assert_total_message_count(0);
            }

            pub fn test_broadcast_closed_partial(&self, timeout: Duration) {
                let timeout = Timeout::new(timeout);

                let (clients, server) = self.stack_factory.create_stacks::<3>();
                let socket_addr = timeout.run(
                    server.assert_listen(),
                    "Server listening",
                    None,
                );
                let connections = timeout.run(
                    clients.assert_connect(&server, socket_addr),
                    "Clients connecting",
                    None,
                ).flip();

                timeout.run(
                    connections[0].close_dst_and_wait(),
                    "Closing connection 0",
                    Some(format_args!("{connections:#?}")),
                );
                server.assert_broadcast("server->BROADCAST");

                timeout.run(
                    clients[1..].wait_for_total_message_count_for_each(1),
                    "Waiting for messages",
                    Some(format_args!("{connections:#?}")),
                );
                clients[0].assert_total_message_count(0);
                clients[1..].assert_message_count_for_each("server->BROADCAST", 1);
                server.assert_total_message_count(0);
            }
        }
    }
}