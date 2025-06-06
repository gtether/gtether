use ahash::{HashMap, HashMapExt};
use educe::Educe;
use parking_lot::Mutex;
use std::error::Error;
use std::num::NonZeroU64;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::debug;

use crate::net::message::MessageHandler;
use crate::net::message::{Message, MessageDecodeError, MessageDispatchError, MessageHeader, MessageRecv, MessageRepliable, MessageReplyContext, MessageReplyFuture, MessageSend};
use crate::net::Connection;

#[derive(Educe)]
#[educe(Debug)]
struct MessageDispatchEntry {
    #[educe(Debug(ignore))]
    handle_fn: Box<dyn (Fn(
        MessageHeader,
        Connection,
        &[u8],
    ) -> Result<bool, MessageDispatchError>) + Send + Sync + 'static>,
}

impl MessageDispatchEntry {
    fn reply<M>() -> (Self, MessageReplyFuture<M>)
    where
        M: MessageRecv + Send + 'static,
    {
        let reply_ctx = Arc::new(MessageReplyContext::new());
        let handle_reply_ctx = reply_ctx.clone();

        let entry = Self {
            //handle_fn: S::reply_fn(reply_ctx.clone()),
            handle_fn: Box::new(move |msg_header, _, msg_body| {
                let msg_body = M::decode(msg_body)
                    .map_err(|err| MessageDecodeError {
                        details: "Could not deserialize message body".to_owned(),
                        source: Some(Box::new(err)),
                    })?;
                handle_reply_ctx.accept(Message::with_header(msg_header, msg_body));
                Ok(true)
            }),
        };

        let fut = MessageReplyFuture {
            ctx: reply_ctx,
        };

        (entry, fut)
    }
}

impl MessageDispatchEntry {
    fn new<M, E, H>(handler: H) -> Self
    where
        M: MessageRecv,
        E: Error + Send + Sync + 'static,
        H: MessageHandler<M, E>,
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

// Currently only public within the `net` module, but may need to be more public in the future for
// third-party implemented drivers
pub trait InterceptMessageHandler: Send + Sync + 'static {
    fn accept(
        &self,
        connection: Connection,
        msg_header: &MessageHeader,
        msg_body: &[u8],
    ) -> Result<bool, MessageDispatchError>;

    fn register_reply(
        &self,
        msg_header: &MessageHeader,
    ) -> bool;
}

#[derive(Debug)]
pub struct MessageDispatch {
    handlers: Mutex<HashMap<String, MessageDispatchEntry>>,
    reply_handlers: Mutex<HashMap<NonZeroU64, MessageDispatchEntry>>,
    intercept_handlers: Mutex<Vec<Weak<dyn InterceptMessageHandler>>>,
    next_msg_num: AtomicU64,
}

impl MessageDispatch {
    #[inline]
    pub fn new() -> Self {
        Self {
            handlers: Mutex::new(HashMap::new()),
            reply_handlers: Mutex::new(HashMap::new()),
            intercept_handlers: Mutex::new(Vec::new()),
            next_msg_num: AtomicU64::new(1),
        }
    }

    pub fn gen_msg_num(&self) -> NonZeroU64 {
        self.next_msg_num.fetch_add(1, Ordering::SeqCst).try_into().unwrap()
    }

    pub fn register_reply<M>(
        &self,
        msg: &Message<M>,
    ) -> MessageReplyFuture<M::Reply>
    where
        M: MessageSend + MessageRepliable,
        M::Reply: MessageRecv + Send + 'static,
    {
        for handler in &*self.intercept_handlers.lock() {
            if let Some(handler) = handler.upgrade() {
                if handler.register_reply(msg.header()) {
                    break;
                }
            }
        }
        let reply_num = msg.header().msg_num
            .expect("Message should have msg_num set");
        let (entry, fut) = MessageDispatchEntry::reply();
        self.reply_handlers.lock().insert(reply_num, entry);
        fut
    }

    pub fn insert_handler<M, E>(
        &self,
        handler: impl MessageHandler<M, E>,
    )
    where
        M: MessageRecv,
        E: Error + Send + Sync + 'static,
    {
        self.handlers.lock().insert(
            M::KEY.to_owned(),
            MessageDispatchEntry::new(handler),
        );
    }

    pub fn insert_intercept_handler(
        &self,
        handler: &Arc<impl InterceptMessageHandler>,
    ) {
        let handler: Arc<dyn InterceptMessageHandler> = handler.clone();
        self.intercept_handlers.lock().push(
            Arc::downgrade(&handler)
        );
    }

    #[inline]
    pub fn dispatch(
        &self,
        connection: Connection,
        msg: &[u8],
    ) -> Result<(), MessageDispatchError> {
        let (msg_header, msg_body) = MessageHeader::decode(msg)?;
        self.dispatch_parsed(connection, msg_header, msg_body)
    }

    pub fn dispatch_parsed(
        &self,
        connection: Connection,
        msg_header: MessageHeader,
        msg_body: &[u8],
    ) -> Result<(), MessageDispatchError> {
        debug!(?msg_header, from=%connection, "Dispatching message");

        {
            let mut intercept_handlers = self.intercept_handlers.lock();

            // housekeeping, best effort to remove dropped refs
            intercept_handlers.retain(|handler| handler.strong_count() > 0);

            for handler in intercept_handlers.iter().rev() {
                if let Some(handler) = handler.upgrade() {
                    if handler.accept(connection, &msg_header, msg_body)? {
                        return Ok(())
                    }
                }
            }
        }

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