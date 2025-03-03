use std::error::Error;

use crate::net::message::{Message, MessageBody};
use crate::net::server::Connection;

/// Interface for handling server-side messages.
///
/// Handles a single [Message], and optionally returns an error. The [Connection] that the message
/// was sent from is also given, to distinguish which client it came from.
///
/// Message handlers may be executed from networking stack internal threads. Because of this, any
/// work done in the handler should be light, as it has the potential to block further networking
/// processing. If heavy work needs to be done, it should be delegated to a separate thread. For
/// example, a [MessageQueue](super::queue::MessageQueue) can be used to put all messages in a
/// queue, where they can be polled and processed from another thread.
pub trait ServerMessageHandler<M, E: Error>: Send + Sync + 'static
where
    M: MessageBody,
    E: Error,
{
    /// Process and handle a [Message].
    ///
    /// The [Connection] represents which client sent the message.
    fn handle(
        &self,
        connection: Connection,
        msg: Message<M>,
    ) -> Result<(), E>;
}

impl<M, E, F> ServerMessageHandler<M, E> for F
where
    M: MessageBody,
    E: Error,
    F: (Fn(Connection, Message<M>) -> Result<(), E>) + Send + Sync + 'static,
{
    #[inline]
    fn handle(
        &self,
        connection: Connection,
        msg: Message<M>,
    ) -> Result<(), E> {
        self(connection, msg)
    }
}

