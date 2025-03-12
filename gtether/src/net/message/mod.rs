//! Network messaging utilities.
//!
//! This module contains some utilities for handling network messages, including processing and
//! dispatching of said messages.
//!
//! # Message composition
//!
//! A [Message] is composed of a [header](MessageHeader) and a [body](MessageBody). The header is
//! an internally managed type, and contains information common to all messages, while the body is
//! a user-implemented trait that describes unique message types. The body can be further described
//! with additional traits as follows:
//!  * [MessageSend] - These messages can be sent from a networking stack, and the trait describes
//!    how the message can be encoded into raw byte data.
//!  * [MessageRecv] - These messages can be received from a networking stack, and the trait
//!    describes how the message can be decoded from raw byte data.
//!  * [MessageRepliable] - These messages have a [reply type](MessageRepliable::Reply) associated
//!    with them.
//!
//! Generally, if a message body type implements [MessageSend], it usually also implements
//! [MessageRecv] and vice versa, but if the type differs between sending and receiving, only one
//! can be implemented, as long as the encoded/decoded byte data is compatible.
//!
//! # Handling messages
//!
//! Depending on whether the message is being handled from the client-side or the server-side, there
//! are two different handlers:
//!  * [ClientMessageHandler](client::ClientMessageHandler)
//!  * [ServerMessageHandler](server::ServerMessageHandler)
//!
//! See individual documentation for more details, but the gist is that the server handler has an
//! extra parameter for [Connections](super::server::Connection).
//!
//! Regardless of the specific handler, it can be registered with its matching networking stack via:
//!  * [ClientNetworking::insert_msg_handler()](super::client::ClientNetworking::insert_msg_handler)
//!  * [ServerNetworking::insert_msg_handler()](super::server::ServerNetworking::insert_msg_handler)
//!
//! When handling a message, the handler may execute on the networking stack's own thread, so care
//! should be taken to avoid expensive computation directly in the handler. Instead, such
//! computation should be delegated to another thread, for example by using a
//! [MessageQueue](queue::MessageQueue).

use bitcode::{Decode, DecodeOwned, Encode};
use flagset::{flags, FlagSet};
use parking_lot::{Condvar, Mutex};
use std::convert::Infallible;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::num::NonZeroU64;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

pub mod client;
pub(super) mod dispatch;
pub mod queue;
pub mod server;

#[cfg(feature = "derive")]
pub use gtether_derive::MessageBody;

/// Error that can occur when encoding messages.
#[derive(Debug)]
pub struct MessageEncodeError {
    /// Details why the message could not be encoded.
    details: String,
    /// Optional source error.
    source: Box<dyn Error + Send + Sync>,
}

impl Display for MessageEncodeError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Could not serialize message data; {}", self.details)
    }
}

impl Error for MessageEncodeError {
    #[inline]
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.deref())
    }
}

/// Error that can occur when decoding messages.
#[derive(Debug)]
pub struct MessageDecodeError {
    /// Details why the message could not be decoded.
    details: String,
    /// Optional source error.
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl Display for MessageDecodeError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Could not deserialize message data; {}", self.details)
    }
}

impl Error for MessageDecodeError {
    #[inline]
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Some(source) = &self.source {
            Some(source.deref())
        } else {
            None
        }
    }
}

flags! {
    /// Flags that can change how [messages](Message) behave.
    ///
    /// Flags exist on a per-type basis, and are defined by [MessageBody::flags()].
    pub enum MessageFlags: u16 {
        /// The message should be sent/received reliably.
        Reliable,
    }
}

/// Header data for a [Message].
///
/// This type is managed internally, but can be accessed from a message in order to retrieve some
/// common data about the message.
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq)]
pub struct MessageHeader {
    key: String,
    msg_num: Option<NonZeroU64>,
    reply_num: Option<NonZeroU64>,
}

impl MessageHeader {
    /// The message key.
    ///
    /// This key is determined on a per-type basis, and is defined by [MessageBody::KEY].
    #[inline]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The message number.
    ///
    /// The number is generated by the networking stack, but is generally unique for each stack and
    /// message combination.
    ///
    /// If the networking stack has not assigned a number to the message yet (i.e. it has been
    /// created but not sent), this may yield `None`.
    #[inline]
    pub fn msg_num(&self) -> Option<NonZeroU64> {
        self.msg_num
    }

    /// The number of the message that this message is replying to.
    ///
    /// This number is optional, and may be `None`, but if it exists it is used to tie a message to
    /// the message it is responding to.
    #[inline]
    pub fn reply_num(&self) -> Option<NonZeroU64> {
        self.reply_num
    }

    fn encode(&self) -> impl Iterator<Item=u8> {
        let bytes = bitcode::encode(self);
        let len = bytes.len() as u16;
        len.to_be_bytes().into_iter()
            .chain(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<(Self, &[u8]), MessageDecodeError> {
        let header_len = if bytes.len() >= 2 {
            u16::from_be_bytes(bytes[0..2].try_into()
                .map_err(|err| MessageDecodeError {
                    details: "Could not deserialize message header length".to_owned(),
                    source: Some(Box::new(err)),
                })?)
        } else {
            return Err(MessageDecodeError {
                details: format!("Not enough bytes to parse header length ({} < 2)", bytes.len()),
                source: None,
            });
        };
        // Include 2 for the original 2 bytes
        let full_len = header_len as usize + 2;
        if bytes.len() >= full_len {
            let header = bitcode::decode(&bytes[2..full_len])
                .map_err(|err| MessageDecodeError {
                    details: "Could not deserialize message header".to_owned(),
                    source: Some(Box::new(err)),
                })?;
            Ok((header, &bytes[full_len..]))
        } else {
            Err(MessageDecodeError {
                details: format!("Not enough bytes to parse header ({} < {})", bytes.len(), full_len),
                source: None,
            })
        }
    }
}

/// Body data for a [Message].
///
/// This type is user-defined.
///
/// See [module-level documentation](super::message#message-composition) for more.
///
/// # Implementation
///
/// The easiest way to implement MessageBody is with the derive macro:
/// ```
/// use gtether::net::message::MessageBody;
///
/// #[derive(MessageBody)]
/// struct MyMessage {}
///
/// #[derive(MessageBody)]
/// #[message_flag(Reliable)]
/// struct MyReliableMessage {}
/// ```
///
/// You can however implement MessageBody manually if needed:
/// ```
/// use gtether::net::message::{MessageBody, MessageFlags};
/// use gtether::util::FlagSet;
///
/// struct MyMessage {}
/// impl MessageBody for MyMessage {
///     const KEY: &'static str = "MyMessage";
/// }
///
/// struct MyReliableMessage {}
/// impl MessageBody for MyReliableMessage {
///     const KEY: &'static str = "MyReliableMessage";
///     fn flags() -> FlagSet<MessageFlags> {
///         MessageFlags::Reliable.into()
///     }
/// }
/// ```
pub trait MessageBody {
    /// The key identifying this message type.
    ///
    /// This is used to match to a [handler](super::message#handling-messages) when dispatching
    /// messages.
    const KEY: &'static str;

    /// Any additional flags describing how a message should be treated.
    ///
    /// By default, yields the empty set of flags.
    fn flags() -> FlagSet<MessageFlags> {
        FlagSet::default()
    }
}

/// Trait describing a [Message] that can be sent.
///
/// This trait is used both as a marker and to define how a [Message] can be
/// [encoded](MessageSend::encode).
///
/// Note that for convenience this trait is automatically implemented for any type that implements
/// both [MessageBody] and bitcode's `Encode` trait.
pub trait MessageSend: MessageBody {
    /// Error type that may occur while encoding.
    type EncodeError: Error + Send + Sync + 'static;

    /// Encode this message body in raw byte data.
    ///
    /// The encode method is expected to allocate its own buffer to fill and return.
    fn encode(&self) -> Result<Vec<u8>, Self::EncodeError>;
}

impl<M: MessageBody + Encode> MessageSend for M {
    type EncodeError = Infallible;

    #[inline]
    fn encode(&self) -> Result<Vec<u8>, Self::EncodeError> {
        Ok(bitcode::encode(self))
    }
}

/// Trait describing a [Message] that can be received.
///
/// This trait is used both as a marker and to define how a [Message] can be
/// [decoded](MessageRecv::decode).
///
/// Note that for convenience this trait is automatically implemented for any type that implements
/// both [MessageBody] and bitcode's `DecodeOwned` trait.
pub trait MessageRecv: MessageBody {
    /// Error tyep that may occur while decoding.
    type DecodeError: Error + Send + Sync + 'static;

    /// Decode raw byte data into a new message body.
    fn decode(bytes: &[u8]) -> Result<Self, Self::DecodeError>
    where
        Self: Sized;
}

impl<M: MessageBody + DecodeOwned> MessageRecv for M {
    type DecodeError = bitcode::Error;

    #[inline]
    fn decode(bytes: &[u8]) -> Result<Self, Self::DecodeError>
    where
        Self: Sized,
    {
        Ok(bitcode::decode(bytes)?)
    }
}

/// Trait describing a [Message] that can be replied to.
///
/// # Implementation
///
/// The easiest way to implement MessageRepliable is with the [MessageBody] derive macro:
/// ```
/// use gtether::net::message::MessageBody;
///
/// #[derive(MessageBody)]
/// #[message_reply(MyMessageReply)]
/// struct MyMessage {}
///
/// #[derive(MessageBody)]
/// struct MyMessageReply {}
/// ```
///
/// You can however implement MessageBody manually if needed:
/// ```
/// use gtether::net::message::{MessageBody, MessageRepliable};
///
/// #[derive(MessageBody)]
/// struct MyMessage {}
/// impl MessageRepliable for MyMessage {
///     type Reply = MyMessageReply;
/// }
///
/// #[derive(MessageBody)]
/// struct MyMessageReply {}
/// ```
pub trait MessageRepliable: MessageBody {
    /// The message type of the reply.
    type Reply: MessageBody;
}

/// A full message, including both a [header](MessageHeader) and a [body](MessageBody).
#[derive(Debug, PartialEq, Eq)]
pub struct Message<M: MessageBody> {
    header: MessageHeader,
    body: M,
}

impl<M: MessageBody> Message<M> {
    /// Create a new [Message] from a given `body`.
    ///
    /// This new message will not have a number assigned to it until it is given to a networking
    /// stack.
    ///
    /// Note that you can also convert a [MessageBody] into a [Message] using `into()`/`from()`.
    #[inline]
    pub fn new(body: M) -> Self {
        let header = MessageHeader {
            key: M::KEY.to_owned(),
            msg_num: None,
            reply_num: None,
        };

        Self {
            header,
            body,
        }
    }

    #[inline]
    pub(super) fn with_header(header: MessageHeader, body: M) -> Self {
        Self {
            header,
            body,
        }
    }

    /// Get a reference to the [header](MessageHeader).
    #[inline]
    pub fn header(&self) -> &MessageHeader {
        &self.header
    }

    /// Get a reference to the [body](MessageBody).
    #[inline]
    pub fn body(&self) -> &M {
        &self.body
    }

    /// Unwrap into the [body](MessageBody).
    #[inline]
    pub fn into_body(self) -> M {
        self.body
    }

    pub(super) fn set_msg_num(&mut self, msg_num: NonZeroU64) {
        self.header.msg_num = Some(msg_num);
    }
}

impl<M: MessageSend> Message<M> {
    /// Encode the entirety of this message into raw bytes, including the header.
    ///
    /// Will allocate and yield a new buffer.
    pub fn encode(&self) -> Result<Vec<u8>, MessageEncodeError> {
        let body = self.body.encode()
            .map_err(|err| MessageEncodeError {
                details: "Could not serialize message body".to_owned(),
                source: Box::new(err)
            })?;

        Ok(self.header.encode()
            .chain(body)
            .collect::<Vec<_>>())
    }
}

impl<M: MessageRepliable> Message<M> {
    /// Create a new message as a reply to this message.
    ///
    /// The new message will have its reply number set to this message's number.
    ///
    /// # Examples
    /// ```
    /// use std::convert::Infallible;
    /// use gtether::net::message::{Message, MessageBody, MessageRepliable};
    ///
    /// struct MyMessage {}
    /// struct MyMessageReply {}
    ///
    /// impl MessageBody for MyMessage {
    ///     const KEY: &'static str = "my-message";
    /// }
    ///
    /// impl MessageBody for MyMessageReply {
    ///     const KEY: &'static str = "my-message-reply";
    /// }
    ///
    /// impl MessageRepliable for MyMessage {
    ///     type Reply = MyMessageReply;
    /// }
    ///
    /// fn msg_handler(msg: Message<MyMessage>) -> Result<(), Infallible> {
    ///     let reply = msg.reply(MyMessageReply {});
    ///     // Send the reply...
    ///     Ok(())
    /// }
    /// ```
    #[inline]
    pub fn reply(&self, body: M::Reply) -> Message<M::Reply> {
        let header = MessageHeader {
            key: M::Reply::KEY.to_owned(),
            msg_num: None,
            reply_num: self.header.msg_num,
        };

        Message {
            header,
            body,
        }
    }
}

impl<M: MessageBody> From<M> for Message<M> {
    #[inline]
    fn from(value: M) -> Self {
        Message::new(value)
    }
}

pub(in crate::net) struct MessageReplyContext<M: MessageRecv> {
    value: Mutex<Option<Message<M>>>,
    waker: Mutex<Option<Waker>>,
    cvar: Condvar,
}

impl<M: MessageRecv> MessageReplyContext<M> {
    fn new() -> Self {
        Self {
            value: Mutex::new(None),
            waker: Mutex::new(None),
            cvar: Condvar::new(),
        }
    }

    fn accept(&self, msg: Message<M>) {
        let mut value = self.value.lock();
        *value = Some(msg);
        if let Some(waker) = self.waker.lock().take() {
            waker.wake();
        }
        self.cvar.notify_one();
    }
}

/// A future indicating a reply that is yet to be received.
///
/// Yielded when sending a message and expecting a reply back. In addition to implementing the std's
/// [Future], can also be waited synchronously via [MessageReplyFuture::wait()].
pub struct MessageReplyFuture<M: MessageRecv> {
    ctx: Arc<MessageReplyContext<M>>,
}

impl<M: MessageRecv> MessageReplyFuture<M> {
    /// Block and synchronously wait for a reply.
    pub fn wait(self) -> Message<M> {
        let mut msg = self.ctx.value.lock();
        if let Some(msg) = msg.take() {
            msg
        } else {
            self.ctx.cvar.wait(&mut msg);
            msg.take().expect("'msg' should be set after cvar wakes")
        }
    }
}

impl<M: MessageRecv> Future for MessageReplyFuture<M> {
    type Output = Message<M>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(msg) = self.ctx.value.lock().take() {
            Poll::Ready(msg)
        } else {
            let mut waker = self.ctx.waker.lock();
            if let Some(waker) = waker.as_mut() {
                waker.clone_from(ctx.waker());
            } else {
                *waker = Some(ctx.waker().clone());
            }
            Poll::Pending
        }
    }
}


/// Errors that can occur when dispatching messages.
#[derive(Debug)]
pub enum MessageDispatchError {
    /// A message could not be parsed from the raw byte data.
    DecodeError(MessageDecodeError),

    /// There was no handler registered with the given `key`.
    NoHandler {
        /// `key` that is missing a handler.
        key: String,
    },

    /// The message handler failed to dispatch the message.
    DispatchFailed(Box<dyn Error + Send + Sync>),
}

impl Display for MessageDispatchError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DecodeError(error) =>
                Display::fmt(error, f),
            Self::NoHandler { key } =>
                write!(f, "No handler was identified for key: '{key}'"),
            Self::DispatchFailed(err) =>
                write!(f, "Could not dispatch message: {err}"),
        }
    }
}

impl Error for MessageDispatchError {
    #[inline]
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DecodeError(error) => error.source(),
            Self::DispatchFailed(err) => Some(err.deref()),
            _ => None,
        }
    }
}

impl From<MessageDecodeError> for MessageDispatchError {
    #[inline]
    fn from(value: MessageDecodeError) -> Self {
        Self::DecodeError(value)
    }
}