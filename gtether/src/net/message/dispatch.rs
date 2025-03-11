use ahash::{HashMap, HashMapExt};
use educe::Educe;
use parking_lot::Mutex;
use std::error::Error;
use std::num::NonZeroU64;
use std::sync::Arc;
use tracing::debug;

use crate::net::message::client::ClientMessageHandler;
use crate::net::message::server::ServerMessageHandler;
use crate::net::message::{Message, MessageDecodeError, MessageDispatchError, MessageHeader, MessageRecv, MessageRepliable, MessageReplyContext, MessageReplyFuture, MessageSend};
use crate::net::server::Connection;

pub trait MessageDispatchSide {
    type HandleFn;

    fn reply_fn<M>(reply_ctx: Arc<MessageReplyContext<M>>) -> Self::HandleFn
    where
        M: MessageRecv + Send + 'static;
}

pub enum MessageDispatchClient {}

impl MessageDispatchSide for MessageDispatchClient {
    type HandleFn = Box<dyn (Fn(
        MessageHeader,
        &[u8],
    ) -> Result<bool, MessageDispatchError>) + Send + Sync + 'static>;

    fn reply_fn<M>(reply_ctx: Arc<MessageReplyContext<M>>) -> Self::HandleFn
    where
        M: MessageRecv + Send + 'static,
    {
        Box::new(move |msg_header, msg_body| {
            let msg_body = M::decode(msg_body)
                .map_err(|err| MessageDecodeError {
                    details: "Could not deserialize message body".to_owned(),
                    source: Some(Box::new(err)),
                })?;
            reply_ctx.accept(Message::with_header(msg_header, msg_body));
            Ok(true)
        })
    }
}

pub enum MessageDispatchServer {}

impl MessageDispatchSide for MessageDispatchServer {
    type HandleFn = Box<dyn (Fn(
        MessageHeader,
        Connection,
        &[u8],
    ) -> Result<bool, MessageDispatchError>) + Send + Sync + 'static>;

    fn reply_fn<M>(reply_ctx: Arc<MessageReplyContext<M>>) -> Self::HandleFn
    where
        M: MessageRecv + Send + 'static,
    {
        Box::new(move |msg_header, _, msg_body| {
            let msg_body = M::decode(msg_body)
                .map_err(|err| MessageDecodeError {
                    details: "Could not deserialize message body".to_owned(),
                    source: Some(Box::new(err)),
                })?;
            reply_ctx.accept(Message::with_header(msg_header, msg_body));
            Ok(true)
        })
    }
}

#[derive(Educe)]
#[educe(Debug)]
struct MessageDispatchEntry<S: MessageDispatchSide> {
    #[educe(Debug(ignore))]
    handle_fn: S::HandleFn,
}

impl<S: MessageDispatchSide> MessageDispatchEntry<S> {
    fn reply<M>() -> (Self, MessageReplyFuture<M>)
    where
        M: MessageRecv + Send + 'static,
    {
        let reply_ctx = Arc::new(MessageReplyContext::new());

        let entry = Self {
            handle_fn: S::reply_fn(reply_ctx.clone()),
        };

        let fut = MessageReplyFuture {
            ctx: reply_ctx,
        };

        (entry, fut)
    }
}

impl MessageDispatchEntry<MessageDispatchClient> {
    fn new<M, E, H>(handler: H) -> Self
    where
        M: MessageRecv,
        E: Error + Send + Sync + 'static,
        H: ClientMessageHandler<M, E>,
    {
        Self {
            handle_fn: Box::new(move |msg_header, msg_body| {
                if handler.is_valid() {
                    let msg_body = M::decode(msg_body)
                        .map_err(|err| MessageDecodeError {
                            details: "Could not deserialize message body".to_owned(),
                            source: Some(Box::new(err)),
                        })?;
                    let msg = Message::with_header(msg_header, msg_body);
                    handler.handle(msg)
                        .map_err(|err| MessageDispatchError::DispatchFailed(Box::new(err)))?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }),
        }
    }

    fn handle(
        &self,
        msg_header: MessageHeader,
        msg_body: &[u8],
    ) -> Result<bool, MessageDispatchError> {
        (self.handle_fn)(msg_header, msg_body)
    }
}

impl MessageDispatchEntry<MessageDispatchServer> {
    fn new<M, E, H>(handler: H) -> Self
    where
        M: MessageRecv,
        E: Error + Send + Sync + 'static,
        H: ServerMessageHandler<M, E>,
    {
        Self {
            handle_fn: Box::new(move |msg_header, connection, msg_body| {
                if handler.is_valid() {
                    let msg_body = M::decode(msg_body)
                        .map_err(|err| MessageDecodeError {
                            details: "Could not deserialize message body".to_owned(),
                            source: Some(Box::new(err)),
                        })?;
                    let msg = Message::with_header(msg_header, msg_body);
                    handler.handle(connection, msg)
                        .map_err(|err| MessageDispatchError::DispatchFailed(Box::new(err)))?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
        }
    }

    fn handle(
        &self,
        msg_header: MessageHeader,
        connection: Connection,
        msg_body: &[u8],
    ) -> Result<bool, MessageDispatchError> {
        (self.handle_fn)(msg_header, connection, msg_body)
    }
}

#[derive(Debug)]
pub struct MessageDispatch<S: MessageDispatchSide> {
    handlers: Mutex<HashMap<String, MessageDispatchEntry<S>>>,
    reply_handlers: Mutex<HashMap<NonZeroU64, MessageDispatchEntry<S>>>,
}

impl<S: MessageDispatchSide> MessageDispatch<S> {
    #[inline]
    pub fn new() -> Self {
        Self {
            handlers: Mutex::new(HashMap::new()),
            reply_handlers: Mutex::new(HashMap::new()),
        }
    }

    pub fn register_reply<M>(
        &self,
        msg: &Message<M>,
    ) -> MessageReplyFuture<M::Reply>
    where
        M: MessageSend + MessageRepliable,
        M::Reply: MessageRecv + Send + 'static,
    {
        let reply_num = msg.header().msg_num
            .expect("Message should have msg_num set");
        let (entry, fut) = MessageDispatchEntry::<S>::reply();
        self.reply_handlers.lock().insert(reply_num, entry);
        fut
    }
}

impl MessageDispatch<MessageDispatchClient> {
    pub fn insert_handler<M, E>(
        &self,
        handler: impl ClientMessageHandler<M, E>,
    )
    where
        M: MessageRecv,
        E: Error + Send + Sync + 'static,
    {
        self.handlers.lock().insert(
            M::KEY.to_owned(),
            MessageDispatchEntry::<MessageDispatchClient>::new(handler),
        );
    }

    pub fn dispatch(
        &self,
        msg: &[u8],
    ) -> Result<(), MessageDispatchError> {
        let (msg_header, msg_body) = MessageHeader::decode(msg)?;
        debug!(side="client", ?msg_header, "Dispatching message");

        if let Some(reply_num) = msg_header.reply_num {
            let mut reply_handlers = self.reply_handlers.lock();
            if let Some(entry) = reply_handlers.remove(&reply_num) {
                return entry.handle(msg_header, msg_body).map(|_| ());
            }
        }

        let mut handlers = self.handlers.lock();
        if let Some(entry) = handlers.get(&msg_header.key) {
            let key = msg_header.key.clone();
            if !entry.handle(msg_header, msg_body)? {
                handlers.remove(&key);
            }
            return Ok(())
        }

        Err(MessageDispatchError::NoHandler { key: msg_header.key })
    }
}

impl MessageDispatch<MessageDispatchServer> {
    pub fn insert_handler<M, E>(
        &self,
        handler: impl ServerMessageHandler<M, E>,
    )
    where
        M: MessageRecv,
        E: Error + Send + Sync + 'static,
    {
        self.handlers.lock().insert(
            M::KEY.to_owned(),
            MessageDispatchEntry::<MessageDispatchServer>::new(handler),
        );
    }

    pub fn dispatch(
        &self,
        connection: Connection,
        msg: &[u8],
    ) -> Result<(), MessageDispatchError> {
        let (msg_header, msg_body) = MessageHeader::decode(msg)?;
        debug!(side="server", ?msg_header, "Dispatching message");

        if let Some(reply_num) = msg_header.reply_num {
            let mut reply_handlers = self.reply_handlers.lock();
            if let Some(entry) = reply_handlers.remove(&reply_num) {
                return entry.handle(msg_header, connection, msg_body).map(|_| ());
            }
        }

        let mut handlers = self.handlers.lock();
        if let Some(entry) = handlers.get(&msg_header.key) {
            let key = msg_header.key.clone();
            if !entry.handle(msg_header, connection, msg_body)? {
                handlers.remove(&key);
            }
            return Ok(())
        }

        Err(MessageDispatchError::NoHandler { key: msg_header.key })
    }
}