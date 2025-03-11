use crossbeam::queue::ArrayQueue;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use crate::net::message::{Message, MessageBody};
use crate::net::message::client::ClientMessageHandler;
use crate::net::message::server::ServerMessageHandler;
use crate::net::server::Connection;

/// Error that occurs when the [MessageQueue] is full.
#[derive(Debug)]
pub struct MessageQueueFull {}

impl Display for MessageQueueFull {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Queue full")
    }
}

impl Error for MessageQueueFull {}

/// A queue for delegating networking [Message] handling to other threads.
///
/// This queue uses a fixed-capacity buffer, with the size determined on construction. If a
/// [Message] is attempted to be pushed onto the queue when it is full, it will result in a
/// [MessageDispatchError].
///
/// # Message Handling
///
/// [ClientMessageHandler] is implemented on `Arc<MessageQueue<Message<M>>>`, and
/// [ServerMessageHandler] is implemented on `Arc<MessageQueue<(Connection, Message<M>)>>`. This
/// means a MessageQueue reference may be used as a message handler if it uses the right generic
/// type.
pub struct MessageQueue<M>
where
    M: Send + Sync + 'static,
{
    inner: ArrayQueue<M>,
}

impl<M> MessageQueue<M>
where
    M: Send + Sync + 'static,
{
    /// Creates a new bounded queue with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if the capacity is zero.
    ///
    /// # Examples
    /// ```
    /// use gtether::net::message::queue::MessageQueue;
    ///
    /// let q = MessageQueue::<()>::new(100);
    /// ```
    #[inline]
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: ArrayQueue::new(cap),
        })
    }

    /// Attempts to pop a message from the queue.
    ///
    /// If the queue is empty, `None` is returned.
    ///
    /// # Examples
    /// ```
    /// # use gtether::net::message::queue::MessageQueueFull;
    /// use gtether::net::message::client::ClientMessageHandler;
    /// use gtether::net::message::queue::MessageQueue;
    /// use gtether::net::message::{Message, MessageBody};
    ///
    /// #[derive(Debug, PartialEq, Eq)]
    /// struct MyMessage(u64);
    /// impl MessageBody for MyMessage {
    ///     const KEY: &'static str = "my-message";
    /// }
    ///
    /// let q = MessageQueue::new(100);
    /// q.handle(Message::new(MyMessage(42)))?;
    ///
    /// assert_eq!(q.pop(), Some(Message::new(MyMessage(42))));
    /// assert_eq!(q.pop(), None);
    /// #
    /// # Ok::<_, MessageQueueFull>(())
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
    /// use gtether::net::message::queue::MessageQueue;
    ///
    /// let q = MessageQueue::<()>::new(100);
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
    /// # use gtether::net::message::queue::MessageQueueFull;
    /// use gtether::net::message::client::ClientMessageHandler;
    /// use gtether::net::message::queue::MessageQueue;
    /// use gtether::net::message::{Message, MessageBody};
    ///
    /// #[derive(Debug, PartialEq, Eq)]
    /// struct MyMessage(u64);
    /// impl MessageBody for MyMessage {
    ///     const KEY: &'static str = "my-message";
    /// }
    ///
    /// let q = MessageQueue::new(100);
    ///
    /// assert!(q.is_empty());
    /// q.handle(Message::new(MyMessage(42)))?;
    /// assert!(!q.is_empty());
    /// #
    /// # Ok::<_, MessageQueueFull>(())
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns `true` if the queue is full.
    ///
    /// # Examples
    /// ```
    /// # use gtether::net::message::queue::MessageQueueFull;
    /// use gtether::net::message::client::ClientMessageHandler;
    /// use gtether::net::message::queue::MessageQueue;
    /// use gtether::net::message::{Message, MessageBody};
    ///
    /// #[derive(Debug, PartialEq, Eq)]
    /// struct MyMessage(u64);
    /// impl MessageBody for MyMessage {
    ///     const KEY: &'static str = "my-message";
    /// }
    ///
    /// let q = MessageQueue::new(1);
    ///
    /// assert!(!q.is_full());
    /// q.handle(Message::new(MyMessage(42)))?;
    /// assert!(q.is_full());
    /// #
    /// # Ok::<_, MessageQueueFull>(())
    /// ```
    #[inline]
    pub fn is_full(&self) -> bool {
        self.inner.is_full()
    }

    /// Returns the number of elements in the queue.
    ///
    /// # Examples
    /// ```
    /// # use gtether::net::message::queue::MessageQueueFull;
    /// use gtether::net::message::client::ClientMessageHandler;
    /// use gtether::net::message::queue::MessageQueue;
    /// use gtether::net::message::{Message, MessageBody};
    ///
    /// #[derive(Debug, PartialEq, Eq)]
    /// struct MyMessage(u64);
    /// impl MessageBody for MyMessage {
    ///     const KEY: &'static str = "my-message";
    /// }
    ///
    /// let q = MessageQueue::new(100);
    /// assert_eq!(q.len(), 0);
    ///
    /// q.handle(Message::new(MyMessage(1)))?;
    /// assert_eq!(q.len(), 1);
    ///
    /// q.handle(Message::new(MyMessage(2)))?;
    /// assert_eq!(q.len(), 2);
    /// #
    /// # Ok::<_, MessageQueueFull>(())
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
    /// # use gtether::net::message::queue::MessageQueueFull;
    /// use gtether::net::message::client::ClientMessageHandler;
    /// use gtether::net::message::queue::MessageQueue;
    /// use gtether::net::message::{Message, MessageBody};
    ///
    /// #[derive(Debug, PartialEq, Eq)]
    /// struct MyMessage(u64);
    /// impl MessageBody for MyMessage {
    ///     const KEY: &'static str = "my-message";
    /// }
    ///
    /// let q = MessageQueue::new(100);
    /// q.handle(Message::new(MyMessage(1)))?;
    /// q.handle(Message::new(MyMessage(2)))?;
    /// q.handle(Message::new(MyMessage(4)))?;
    ///
    /// let mut iterator = q.iter();
    /// assert_eq!(iterator.next(), Some(Message::new(MyMessage(1))));
    /// assert_eq!(iterator.next(), Some(Message::new(MyMessage(2))));
    /// assert_eq!(iterator.next(), Some(Message::new(MyMessage(4))));
    /// assert_eq!(iterator.next(), None);
    /// #
    /// # Ok::<_, MessageQueueFull>(())
    /// ```
    #[inline]
    pub fn iter(&self) -> MessageQueueIter<'_, M> {
        MessageQueueIter { queue: self }
    }
}

impl<M> ClientMessageHandler<M, MessageQueueFull> for MessageQueue<Message<M>>
where
    M: MessageBody + Send + Sync + 'static,
{
    #[inline]
    fn handle(&self, msg: Message<M>) -> Result<(), MessageQueueFull> {
        self.inner.push(msg)
            .map_err(|_| MessageQueueFull {})
    }
}

impl<M> ServerMessageHandler<M, MessageQueueFull> for MessageQueue<(Connection, Message<M>)>
where
    M: MessageBody + Send + Sync + 'static,
{
    #[inline]
    fn handle(&self, connection: Connection, msg: Message<M>) -> Result<(), MessageQueueFull> {
        self.inner.push((connection, msg))
            .map_err(|_| MessageQueueFull {})
    }
}

impl<'a, M> IntoIterator for &'a MessageQueue<M>
where
    M: Send + Sync + 'static,
{
    type Item = M;
    type IntoIter = MessageQueueIter<'a, M>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator for messages in [MessageQueue].
pub struct MessageQueueIter<'a, M>
where
    M: Send + Sync + 'static,
{
    queue: &'a MessageQueue<M>,
}

impl<'a, M> Iterator for MessageQueueIter<'a, M>
where
    M: Send + Sync + 'static,
{
    type Item = M;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.queue.pop()
    }
}