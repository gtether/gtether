use std::error::Error;
use std::sync::{Arc, Weak};

use crate::net::message::{Message, MessageBody};

/// Interface for handling client-side messages.
///
/// Handles a single [Message], and optionally returns an error.
///
/// Message handlers may be executed from networking stack internal threads. Because of this, any
/// work done in the handler should be light, as it has the potential to block further networking
/// processing. If heavy work needs to be done, it should be delegated to a separate thread. For
/// example, a [MessageQueue](super::queue::MessageQueue) can be used to put all messages in a
/// queue, where they can be polled and processed from another thread.
pub trait ClientMessageHandler<M, E>: Send + Sync + 'static
where
    M: MessageBody,
    E: Error,
{
    /// Process and handle a [Message].
    fn handle(&self, msg: Message<M>) -> Result<(), E>;

    /// Whether this handler is still valid.
    ///
    /// If it is not valid, it will be removed and no longer used to handle messages.
    #[inline]
    fn is_valid(&self) -> bool { true }
}

impl<M, E, F> ClientMessageHandler<M, E> for F
where
    M: MessageBody,
    E: Error,
    F: (Fn(Message<M>) -> Result<(), E>) + Send + Sync + 'static,
{
    #[inline]
    fn handle(&self, msg: Message<M>) -> Result<(), E> {
        self(msg)
    }
}

impl<M, E, H> ClientMessageHandler<M, E> for Arc<H>
where
    M: MessageBody,
    E: Error,
    H: ClientMessageHandler<M, E>,
{
    #[inline]
    fn handle(&self, msg: Message<M>) -> Result<(), E> {
        (**self).handle(msg)
    }
}

impl<M, E, H> ClientMessageHandler<M, E> for Weak<H>
where
    M: MessageBody,
    E: Error,
    H: ClientMessageHandler<M, E>,
{
    #[inline]
    fn handle(&self, msg: Message<M>) -> Result<(), E> {
        if let Some(handler) = self.upgrade() {
            handler.handle(msg)
        } else {
            Ok(())
        }
    }

    #[inline]
    fn is_valid(&self) -> bool {
        if let Some(handler) = self.upgrade() {
            handler.is_valid()
        } else {
            false
        }
    }
}
