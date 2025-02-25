//! Network messaging utilities.
//!
//! This module contains some utilities for handling network messages, including processing and
//! dispatching of said messages.
//!
//! Processing happens through [MessageHandler].
//!
//! Dispatching happens through [MessageDispatch].

use ahash::HashMap;
use crossbeam::queue::ArrayQueue;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::io::Read;
use std::sync::Arc;

/// Size type used to determine the length of the `msg_type` header in raw messages.
///
/// See [MessageDispatch::dispatch()].
pub type MsgTypeLenType = u16;

/// Errors that can occur when dispatching messages.
#[derive(Debug)]
pub enum MessageDispatchError {
    /// A message could not be parsed from the raw byte data.
    MalformedData {
        /// Details why the message could not be parsed.
        details: String,
        /// Optional source error.
        source: Option<Box<dyn Error>>,
    },

    /// There was no handler registered with the given `msg_type`.
    NoHandler {
        /// `msg_type` that is missing a handler.
        msg_type: String,
    },

    /// The message handler failed to dispatch the message.
    DispatchFailed {
        /// Reason why the message could not be dispatched.
        reason: String,
        /// Optional source error.
        source: Option<Box<dyn Error>>,
    },
}

impl MessageDispatchError {
    /// Helper method for generating a [DispatchFailed](Self::DispatchFailed) error.
    ///
    /// The `source` will be `None`.
    ///
    /// # Examples
    /// ```should_panic
    /// use gtether::net::message::MessageDispatchError;
    ///
    /// Err(MessageDispatchError::dispatch_failed("Something went wrong"))?;
    /// #
    /// # Ok::<_, MessageDispatchError>(())
    /// ```
    #[inline]
    pub fn dispatch_failed(reason: impl Into<String>) -> Self {
        Self::DispatchFailed {
            reason: reason.into(),
            source: None,
        }
    }
}

impl Display for MessageDispatchError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedData { details, .. } =>
                write!(f, "Could not parse malformed message data; {details}"),
            Self::NoHandler { msg_type } =>
                write!(f, "No handler was identified for msg_type: '{msg_type}'"),
            Self::DispatchFailed { reason, .. } =>
                write!(f, "Could not dispatch message: {reason}"),
        }
    }
}

impl Error for MessageDispatchError {
    #[inline]
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MalformedData { source, .. } => source.as_ref(),
            Self::DispatchFailed { source, .. } => source.as_ref(),
            _ => None,
        }.map(|v| &**v)
    }
}

/// API for handling messages.
///
/// # Implementation
///
/// Implementations need to be able to handle a raw data input, and should report any errors through
/// [MessageDispatchError].
///
/// Any work done in the handler should be light, as it has the potential to block further
/// networking processing. If heavy work needs to be done, it should be delegated to a separate
/// thread. For example, the [MessageQueue] implementation of MessageHandler puts all messages in a
/// queue, where they can be polled from another thread.
pub trait MessageHandler: Send + Sync + 'static {
    /// Process and handle a raw data input.
    ///
    /// This method should both process the raw data, and then handle any dispatching or direct
    /// handling of the processed message.
    fn handle(&self, reader: &mut dyn Read) -> Result<(), MessageDispatchError>;
}

/// A queue for delegating networking message handling to other threads.
///
/// This queue uses a fixed-capacity buffer, with the size determined on construction. If a message
/// is attempted to be pushed onto the queue when it is full, it will result in a
/// [MessageDispatchError].
///
/// # Message Handling
///
/// [MessageHandler] is implemented on `Arc<MessageQueue>`, so a MessageQueue reference may be used
/// as a MessageHandler.
///
/// A deserialization function is given to the queue on construction, and it will use this function
/// to deserialize raw data given to [MessageHandler::handle()].
pub struct MessageQueue<M, F>
where
    M: Send + Sync + 'static,
    F: (Fn(&mut dyn Read) -> Result<M, MessageDispatchError>) + Send + Sync + 'static,
{
    inner: ArrayQueue<M>,
    deserialize_fn: F,
}

impl<M, F> MessageQueue<M, F>
where
    M: Send + Sync + 'static,
    F: (Fn(&mut dyn Read) -> Result<M, MessageDispatchError>) + Send + Sync + 'static,
{
    /// Creates a new bounded queue with the given capacity and deserialization function.
    ///
    /// # Panics
    ///
    /// Panics if the capacity is zero.
    ///
    /// # Examples
    /// ```
    /// use gtether::net::message::MessageQueue;
    ///
    /// let q = MessageQueue::new(100, |_| Ok(()));
    /// ```
    #[inline]
    pub fn new(cap: usize, deserialize_fn: F) -> Arc<Self> {
        Arc::new(Self {
            inner: ArrayQueue::new(cap),
            deserialize_fn,
        })
    }

    /// Attempts to pop a message from the queue.
    ///
    /// If the queue is empty, `None` is returned.
    ///
    /// # Examples
    /// ```
    /// # use gtether::net::message::MessageDispatchError;
    /// use gtether::net::message::{MessageHandler, MessageQueue};
    ///
    /// let q = MessageQueue::new(100, |_| Ok(()));
    /// q.handle(&mut std::io::empty())?;
    ///
    /// assert_eq!(q.pop(), Some(()));
    /// assert_eq!(q.pop(), None);
    /// #
    /// # Ok::<_, MessageDispatchError>(())
    /// ```
    ///
    #[inline]
    pub fn pop(&self) -> Option<M> {
        self.inner.pop()
    }

    /// Returns the capacity of the queue.
    ///
    /// # Examples
    /// ```
    /// use gtether::net::message::MessageQueue;
    ///
    /// let q = MessageQueue::new(100, |_| Ok(()));
    /// assert_eq!(q.capacity(), 100);
    /// ```
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Returns `true` if the queue is empty.
    ///
    /// # Examples
    /// ```
    /// # use gtether::net::message::MessageDispatchError;
    /// use gtether::net::message::{MessageHandler, MessageQueue};
    ///
    /// let q = MessageQueue::new(100, |_| Ok(()));
    ///
    /// assert!(q.is_empty());
    /// q.handle(&mut std::io::empty())?;
    /// assert!(!q.is_empty());
    /// #
    /// # Ok::<_, MessageDispatchError>(())
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns `true` if the queue is full.
    ///
    /// # Examples
    /// ```
    /// # use gtether::net::message::MessageDispatchError;
    /// use gtether::net::message::{MessageHandler, MessageQueue};
    ///
    /// let q = MessageQueue::new(1, |_| Ok(()));
    ///
    /// assert!(!q.is_full());
    /// q.handle(&mut std::io::empty())?;
    /// assert!(q.is_full());
    /// #
    /// # Ok::<_, MessageDispatchError>(())
    /// ```
    #[inline]
    pub fn is_full(&self) -> bool {
        self.inner.is_full()
    }

    /// Returns the number of elements in the queue.
    ///
    /// # Examples
    /// ```
    /// # use gtether::net::message::MessageDispatchError;
    /// use gtether::net::message::{MessageHandler, MessageQueue};
    ///
    /// let q = MessageQueue::new(100, |_| Ok(()));
    /// assert_eq!(q.len(), 0);
    ///
    /// q.handle(&mut std::io::empty())?;
    /// assert_eq!(q.len(), 1);
    ///
    /// q.handle(&mut std::io::empty())?;
    /// assert_eq!(q.len(), 2);
    /// #
    /// # Ok::<_, MessageDispatchError>(())
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns an iterator over any messages in the queue.
    ///
    /// The iterator may yield more messages after yielding `None`, if more messages are added to
    /// the queue.
    ///
    /// # Examples
    /// ```
    /// # use gtether::net::message::MessageDispatchError;
    /// use gtether::net::message::{MessageHandler, MessageQueue};
    ///
    /// let q = MessageQueue::new(100, |_| Ok(()));
    /// q.handle(&mut std::io::empty())?;
    /// q.handle(&mut std::io::empty())?;
    ///
    /// let mut iterator = q.iter();
    /// assert_eq!(iterator.next(), Some(()));
    /// assert_eq!(iterator.next(), Some(()));
    /// assert_eq!(iterator.next(), None);
    /// #
    /// # Ok::<_, MessageDispatchError>(())
    /// ```
    #[inline]
    pub fn iter(&self) -> MessageQueueIter<'_, M, F> {
        MessageQueueIter { queue: self }
    }
}

impl<M, F> MessageHandler for Arc<MessageQueue<M, F>>
where
    M: Send + Sync + 'static,
    F: (Fn(&mut dyn Read) -> Result<M, MessageDispatchError>) + Send + Sync + 'static,
{
    fn handle(&self, reader: &mut dyn Read) -> Result<(), MessageDispatchError> {
        let msg = (self.deserialize_fn)(reader)?;
        self.inner.push(msg)
            .map_err(|_| MessageDispatchError::DispatchFailed {
                reason: "Queue full".to_owned(),
                source: None,
            })
    }
}

impl<'a, M, F> IntoIterator for &'a MessageQueue<M, F>
where
    M: Send + Sync + 'static,
    F: (Fn(&mut dyn Read) -> Result<M, MessageDispatchError>) + Send + Sync + 'static,
{
    type Item = M;
    type IntoIter = MessageQueueIter<'a, M, F>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator for messages in [MessageQueue].
pub struct MessageQueueIter<'a, M, F>
where
    M: Send + Sync + 'static,
    F: (Fn(&mut dyn Read) -> Result<M, MessageDispatchError>) + Send + Sync + 'static,
{
    queue: &'a MessageQueue<M, F>,
}

impl<'a, M, F> Iterator for MessageQueueIter<'a, M, F>
where
    M: Send + Sync + 'static,
    F: (Fn(&mut dyn Read) -> Result<M, MessageDispatchError>) + Send + Sync + 'static,
{
    type Item = M;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.queue.pop()
    }
}

/// Message dispatcher for networking.
///
/// This struct is responsible for dispatching network messages based on `msg_type`. When
/// dispatching a raw message, it will look for any handlers registered for the found `msg_type`,
/// and send the rest of the raw message to that handler.
///
/// Construction of a MessageDispatch is handled internally, but handlers can be registered while
/// building a networking stack, via [NetworkingBuilder::msg_dispatch_handler()][build].
///
/// [build]: crate::net::NetworkingBuilder::msg_dispatch_handler
pub struct MessageDispatch {
    handlers: HashMap<String, Box<dyn MessageHandler>>,
}

impl Debug for MessageDispatch {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        struct HandlersDebug<'a> {
            handlers: &'a HashMap<String, Box<dyn MessageHandler>>,
        }

        impl<'a> Debug for HandlersDebug<'a> {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.debug_map()
                    .entries(self.handlers.iter()
                        .map(|(k, _)| (k.clone(), "..".to_owned())))
                    .finish()
            }
        }

        let handlers_debug = HandlersDebug { handlers: &self.handlers };
        f.debug_struct("MessageDispatch")
            .field("handlers", &handlers_debug)
            .finish()
    }
}

impl MessageDispatch {
    #[inline]
    pub(in crate::net) fn new(handlers: HashMap<String, Box<dyn MessageHandler>>) -> Self {
        Self {
            handlers,
        }
    }

    /// Dispatch a raw message.
    ///
    /// A raw message is expected to have a header containing `msg_type`. This header consists of a
    /// size type matching [MsgTypeLenType], followed by that many utf-8 encoded string bytes. For
    /// example, if [MsgTypeLenType] is `u16`, the first two bytes would be the length of
    /// `msg_type`. If that length is 4, the next 4 bytes would be the utf-8 encoded `msg_type`.
    ///
    /// Once decoded, the `msg_type` is matched against any registered [handlers](MessageHandler),
    /// and if any match, the remaining raw message after the header is sent to that handler.
    ///
    /// # Errors
    ///
    /// Can error for several reasons, including:
    ///  * If the raw message is malformed
    ///  * If there is no handler registered with the found `msg_type`
    ///  * If the handler returns an error
    pub fn dispatch<R: Read>(&self, mut reader: R) -> Result<(), MessageDispatchError> {
        let msg_type = {
            let mut str_size_buf = [0; MsgTypeLenType::BITS as usize / 8];
            reader.read_exact(&mut str_size_buf)
                .map_err(|err| MessageDispatchError::MalformedData {
                    details: "Could not read msg_type metadata".to_owned(),
                    source: Some(Box::new(err)),
                })?;
            let str_size = MsgTypeLenType::from_be_bytes(str_size_buf);

            let mut msg_type = String::with_capacity(str_size as usize);
            reader.by_ref().take(str_size as u64).read_to_string(&mut msg_type)
                .map_err(|err| MessageDispatchError::MalformedData {
                    details: "Could not read msg_type string".to_owned(),
                    source: Some(Box::new(err)),
                })?;
            msg_type
        };

        if let Some(handler) = self.handlers.get(&msg_type) {
            handler.handle(&mut reader)
        } else {
            Err(MessageDispatchError::NoHandler { msg_type })
        }
    }
}