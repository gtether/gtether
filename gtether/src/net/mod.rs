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

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use ahash::{HashMap, HashMapExt};

pub mod client;
pub mod gns;
pub mod message;
pub mod server;

/// Errors that can occur while building a networking stack.
#[derive(Debug)]
pub enum NetworkingBuildError {
    /// A required option was not specified.
    MissingOption { name: String },

    /// The `msg_type` specified for a [MessageHandler](message::MessageHandler) was invalid.
    InvalidMsgType {
        msg_type: String,
        reason: String,
    },

    /// There was some sort of error while initializing the stack.
    InitError { source: Option<Box<dyn Error>> },
}

impl Display for NetworkingBuildError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingOption { name } =>
                write!(f, "Missing required option: '{name}'"),
            Self::InvalidMsgType { msg_type, reason } =>
                write!(f, "Invalid handler msg_type '{msg_type}': {reason}"),
            Self::InitError { .. } => write!(f, "Error occurred while initializing"),
        }
    }
}

impl Error for NetworkingBuildError {
    #[inline]
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InitError { source } => source.as_ref().map(|v| &**v),
            _ => None,
        }
    }
}

impl NetworkingBuildError {
    #[inline]
    pub(in crate::net) fn missing_option(name: impl Into<String>) -> Self {
        Self::MissingOption { name: name.into() }
    }
}

/// Builder pattern for networking stacks.
///
/// The particular type of networking stack that is build depends on the generic parameter.
/// Currently, the options are:
///  * [ClientBuilder](client::ClientBuilder) - builds a
///    [ClientNetworking](client::ClientNetworking) stack
///  * [ServerBuilder](server::ServerBuilder) - builds a
///    [ServerNetworking](server::ServerNetworking) stack
///
/// It is recommended to create a NetworkingBuilder from the relevant `builder()` methods;
/// [ClientNetworking::builder()](client::ClientNetworking::builder) for client-side, and
/// [ServerNetworking::builder()](server::ServerNetworking::builder) for server-side.
///
/// # Examples
///
/// Basic client networking stack using [GNS](gns)
/// ```
/// use gtether::net::client::ClientNetworking;
/// use gtether::net::gns::GnsSubsystem;
/// # use gtether::net::NetworkingBuildError;
///
/// let net = ClientNetworking::builder()
///     .raw_factory(GnsSubsystem::get())
///     .build();
/// #
/// # Ok::<(), NetworkingBuildError>(())
/// ```
///
/// Basic server networking stack using [GNS](gns)
/// ```
/// use gtether::net::server::ServerNetworking;
/// use gtether::net::gns::GnsSubsystem;
/// # use gtether::net::NetworkingBuildError;
///
/// let net = ServerNetworking::builder()
///     .raw_factory(GnsSubsystem::get())
///     .port(9001)
///     .build();
/// #
/// # Ok::<(), NetworkingBuildError>(())
/// ```
///
/// Server networking stack with a different network interface bound.
/// ```
/// use std::net::Ipv4Addr;
/// use gtether::net::server::ServerNetworking;
/// use gtether::net::gns::GnsSubsystem;
/// # use gtether::net::NetworkingBuildError;
///
/// let net = ServerNetworking::builder()
///     .raw_factory(GnsSubsystem::get())
///     .ip_addr(Ipv4Addr::new(192, 168, 0, 42))
///     .port(9001)
///     .build();
/// #
/// # Ok::<(), NetworkingBuildError>(())
/// ```
///
pub struct NetworkingBuilder<T: Default> {
    msg_dispatch_handlers: HashMap<String, Box<dyn message::MessageHandler>>,
    extra: T,
}

impl<T: Default> NetworkingBuilder<T> {
    /// Create a new [NetworkingBuilder].
    ///
    /// It is recommended to use [ClientNetworking::builder()](client::ClientNetworking::builder) or
    /// [ServerNetworking::builder()](server::ServerNetworking::builder) instead.
    #[inline]
    pub fn new() -> Self {
        Self {
            msg_dispatch_handlers: HashMap::new(),
            extra: T::default(),
        }
    }

    /// Add a [message handler](message::MessageHandler) for a specified `msg_type`.
    ///
    /// Specifying this option multiple times with the same `msg_type` will override the previously
    /// set handler with the same `msg_type`.
    #[inline]
    pub fn msg_dispatch_handler(
        mut self,
        msg_type: impl Into<String>,
        handler: impl message::MessageHandler,
    ) -> Self {
        self.msg_dispatch_handlers.insert(msg_type.into(), Box::new(handler));
        self
    }

    fn msg_dispatch(&mut self) -> Result<Arc<message::MessageDispatch>, NetworkingBuildError> {
        let handlers = self.msg_dispatch_handlers.drain()
            .map(|(msg_type, handler)| {
                if msg_type.len() > message::MsgTypeLenType::MAX as usize {
                    Err(NetworkingBuildError::InvalidMsgType {
                        msg_type: msg_type.clone(),
                        reason: format!("Too large ({} > {})", msg_type.len(), message::MsgTypeLenType::MAX),
                    })
                } else {
                    Ok((msg_type, handler))
                }
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        Ok(Arc::new(message::MessageDispatch::new(handlers)))
    }
}

