//! Event lifecycles.
//!
//! This module contains the framework for creating, driving, and hooking into
//! [event-driven buses][eb]. These event buses follow a publish-subscribe pattern, where a
//! particular code site may [publish][ebf] an event, which is then propagated to all
//! [subscribers][eh] registered to events of that type.
//!
//! These event buses are primarily synchronous, in that any subscribed event handlers are triggered
//! inline on the same thread that fires the event.
//!
//! [eb]: EventBus
//! [ebf]: EventBus::fire
//! [eh]: EventHandler
use parking_lot::RwLock;
use smol::prelude::*;
use std::any::{Any, TypeId};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};

/// Marker trait for [events][Event] that are cancellable.
pub trait EventCancellable {}

/// Instance of an event that has been published.
///
/// Each instance of Event represents a single event that has been fired/published, and will be
/// handled by possibly many [subscribers][eh].
///
/// [eh]: EventHandler
pub struct Event<T> {
    inner: T,
    cancelled: bool,
}

impl<T> Event<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            cancelled: false,
        }
    }

    /// User-derived data associated with this Event.
    #[inline]
    pub fn data(&mut self) -> &mut T { &mut self.inner }

    /// Consume this event and yield the data it contains.
    ///
    /// This is usually used after an event has been fired, to retrieve any data that
    /// [subscribers][eh] may have added to it.
    ///
    /// # Examples
    /// ```
    /// # use gtether::event::EventBus;
    /// #
    /// # fn wrapper<T>(input_data: T, event_bus: EventBus) where for<'a> T: 'a {
    /// let event = event_bus.fire(input_data);
    /// let output_data = event.into_data();
    /// // Do something with 'output_data'...
    /// # }
    /// ```
    ///
    /// [eh]: EventHandler
    #[inline]
    pub fn into_data(self) -> T { self.inner }
}

impl<T> Event<T>
where
    T: EventCancellable
{
    /// Cancel this Event.
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Whether this Event has been cancelled.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl<T> Deref for Event<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target { &self.inner }
}

impl<T> DerefMut for Event<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.inner }
}

pub type SubscriberFnOnce<T> = Box<dyn FnOnce(&mut Event<T>) + Send>;

/// User-defined event handler for specific [Events](Event).
///
/// An EventHandler is triggered when any [Events][evt] that it is listening for are fired. An
/// EventHandler is tied to a particular event type.
pub trait EventHandler<T>: Send + Sync + 'static {
    /// Handle a particular [Event] that has been fired.
    fn handle_event(&self, event: &mut Event<T>);

    /// Whether this handler is still valid.
    ///
    /// If it is not valid, it will be removed and no longer used to handle events. This is
    /// primarily used with e.g. handlers implemented for weak references. Note that EventHandler is
    /// automatically implemented for any `Weak<H: EventHandler>`, where this method will return
    /// `false` if the weak reference is no longer valid.
    #[inline]
    fn is_valid(&self) -> bool { true }
}

impl<T, F> EventHandler<T> for F
where
    F: Fn(&mut Event<T>) + Send + Sync + 'static,
{
    #[inline]
    fn handle_event(&self, event: &mut Event<T>) {
        (*self)(event)
    }
}

impl<T, H> EventHandler<T> for Arc<H>
where
    H: EventHandler<T>,
{
    #[inline]
    fn handle_event(&self, event: &mut Event<T>) {
        (**self).handle_event(event)
    }
}

impl<T, H> EventHandler<T> for Weak<H>
where
    H: EventHandler<T>,
{
    #[inline]
    fn handle_event(&self, event: &mut Event<T>) {
        if let Some(handler) = self.upgrade() {
            handler.handle_event(event)
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

struct SubscriberEntry(Box<dyn Any + Send + Sync + 'static>);

impl SubscriberEntry {
    fn new<T>(handler: Box<dyn EventHandler<T>>) -> Self
    where
        for<'a> T: 'a,
    {
        Self(Box::new(handler))
    }

    fn handle_event<T>(&self, event: &mut Event<T>) -> bool
    where
        for<'a> T: 'a,
    {
        let handler = self.0.downcast_ref::<Box<dyn EventHandler<T>>>().unwrap();
        if handler.is_valid() {
            handler.handle_event(event);
            true
        } else {
            false
        }
    }
}

/// Error types associated with [SubscriberOnceFuture].
#[derive(Debug)]
pub enum SubscriberOnceError {
    /// The [EventBus] was dropped, so the queued [SubscriberFnOnce] will never run.
    Dropped,
}

impl Display for SubscriberOnceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscriberOnceError::Dropped => f.write_str("QueuedAction was dropped before being executed"),
        }
    }
}

impl Error for SubscriberOnceError {}

impl From<ump::Error<()>> for SubscriberOnceError {
    #[inline]
    fn from(value: ump::Error<()>) -> Self {
        match value {
            ump::Error::ServerDisappeared | ump::Error::ClientsDisappeared | ump::Error::NoReply =>
                Self::Dropped,
            ump::Error::App(e) => panic!("Unexpected App error: {e:?}"),
        }
    }
}

enum SubscriberOnceFutureState {
    Init(Result<ump::WaitReply<(), ()>, SubscriberOnceError>),
    Awaiting(Pin<Box<dyn Future<Output = Result<(), ump::Error<()>>> + Send>>),
    Uninit,
}

impl SubscriberOnceFutureState {
    fn poll(self, cx: &mut Context<'_>) -> (SubscriberOnceFutureState, Poll<Result<(), SubscriberOnceError>>) {
        match self {
            Self::Init(result) => {
                match result {
                    Ok(wait_reply) =>
                        Self::Awaiting(Box::pin(wait_reply.wait_async())).poll(cx),
                    Err(err) =>
                        (Self::Uninit, Poll::Ready(Err(err))),
                }
            },
            Self::Awaiting(mut fut) => {
                match fut.poll(cx) {
                    Poll::Ready(result) =>
                        (Self::Uninit, Poll::Ready(result.map_err(Into::into))),
                    Poll::Pending => (Self::Awaiting(fut), Poll::Pending),
                }
            },
            Self::Uninit => unreachable!("Found uninit state"),
        }
    }
}

/// Handle for a queued [SubscriberFnOnce].
///
/// Can be waited on either synchronously using [SubscriberOnceFuture::wait()] or asynchronously
/// with `.await` syntax.
pub struct SubscriberOnceFuture(SubscriberOnceFutureState);

impl SubscriberOnceFuture {
    fn map_ump_error(error: ump::Error<()>) -> SubscriberOnceError {
        match error {
            ump::Error::ServerDisappeared | ump::Error::ClientsDisappeared | ump::Error::NoReply
            => SubscriberOnceError::Dropped,
            ump::Error::App(e) => panic!("Unexpected App error: {e:?}"),
        }
    }

    fn new(wait_reply: Result<ump::WaitReply<(), ()>, ump::Error<()>>) -> Self {
        Self(SubscriberOnceFutureState::Init(wait_reply.map_err(Self::map_ump_error)))
    }

    fn with_state<R>(
        &mut self,
        f: impl FnOnce(SubscriberOnceFutureState) -> (SubscriberOnceFutureState, R),
    ) -> R {
        let mut state = SubscriberOnceFutureState::Uninit;
        std::mem::swap(&mut self.0, &mut state);
        let (mut state, result) = f(state);
        std::mem::swap(&mut self.0, &mut state);
        result
    }

    /// Synchronously wait for a [SubscriberFnOnce] to complete.
    ///
    /// This method should NOT be used on the same thread that runs the [EventBus] the one-off
    /// handler was registered to, as this would cause a deadlock.
    ///
    /// # Examples
    /// ```
    /// # use gtether::event::{EventBus, SubscriberOnceError};
    /// use gtether::event::Event;
    ///
    /// # fn wrapper<T>(event_bus: EventBus) -> Result<(), SubscriberOnceError> where for<'a> T: 'a {
    /// let fut = event_bus.registry().register_once(|event: &mut Event<T>| {
    ///     // Some one-time action
    /// }).unwrap(); // We're sure we're registering the right type here
    /// fut.wait()?;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn wait(self) -> Result<(), SubscriberOnceError> {
        match self.0 {
            SubscriberOnceFutureState::Init(wait_reply) =>
                wait_reply?.wait().map_err(Self::map_ump_error),
            SubscriberOnceFutureState::Awaiting(_) =>
                panic!("Can't wait synchronously once async polling has started"),
            SubscriberOnceFutureState::Uninit =>
                unreachable!("Found uninit state"),
        }
    }
}

impl Future for SubscriberOnceFuture {
    type Output = Result<(), SubscriberOnceError>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.with_state(|state| state.poll(cx))
    }
}

struct SubscriberOncePipe {
    client: Box<dyn Any + Send + Sync>, // ump::Client<SubscriberFnOnce<T>, (), ()>,
    server: Box<dyn Any + Send + Sync>, // ump::Server<SubscriberFnOnce<T>, (), ()>,
}

impl SubscriberOncePipe {
    fn new<T>() -> Self
    where
        for<'a> T: 'a,
    {
        let (server, client) = ump::channel::<SubscriberFnOnce<T>, (), ()>();
        Self {
            client: Box::new(client),
            server: Box::new(server),
        }
    }

    fn send<T>(&self, subscriber: SubscriberFnOnce<T>) -> SubscriberOnceFuture
    where
        for<'a> T: 'a,
    {
        let client = self.client.downcast_ref::<ump::Client<SubscriberFnOnce<T>, (), ()>>()
            .unwrap();
        SubscriberOnceFuture::new(client.req_async(subscriber))
    }

    fn handle_event<T>(&self, event: &mut Event<T>)
    where
        for<'a> T: 'a,
    {
        let server = self.server.downcast_ref::<ump::Server<SubscriberFnOnce<T>, (), ()>>()
            .unwrap();
        while let Some((subscriber, ctx)) = server.try_pop().unwrap() {
            subscriber(event);
            ctx.reply(()).unwrap();
        }
    }
}

#[derive(Debug)]
pub struct InvalidTypeIdError {
    type_id: TypeId,
}

impl Display for InvalidTypeIdError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "TypeId '{:?}' is not valid for the EventBus", self.type_id)
    }
}

impl Error for InvalidTypeIdError {}

/// Registry for registering new [EventHandlers][eh] to.
///
/// This is the public-facing part of [EventBus], where users can register additional
/// [EventHandlers][eh] to receive published [Events][evt]. In addition to registering standard
/// handlers with [EventBusRegistry::register()], temporary one-off handlers can be registered using
/// [EventBusRegistry::register_once()]. These will receive exactly one [Event][evt], and can be
/// used for scheduling actions according to certain lifecycle breakpoints.
///
/// [eh]: EventHandler
/// [evt]: Event
pub struct EventBusRegistry {
    valid_type_ids: HashSet<TypeId>,
    subscribers: RwLock<HashMap<TypeId, Vec<SubscriberEntry>>>,
    once_pipes: RwLock<HashMap<TypeId, SubscriberOncePipe>>,
}

impl EventBusRegistry {
    /// Register a new [EventHandler].
    ///
    /// The [EventHandler] will be registered under the [TypeId] of `T`.
    pub fn register<T>(
        &self,
        subscriber: impl EventHandler<T>,
    ) -> Result<(), InvalidTypeIdError>
    where
        for<'a> T: 'a,
    {
        let type_id = TypeId::of::<T>();
        if self.valid_type_ids.contains(&type_id) {
            self.subscribers.write()
                .entry(type_id)
                .or_default()
                .push(SubscriberEntry::new(Box::new(subscriber)));
            Ok(())
        } else {
            Err(InvalidTypeIdError { type_id })
        }
    }

    /// Register a one-off event handler.
    ///
    /// The one-off event handler will be registered under the [TypeId] of `T`.
    ///
    /// This one-off handler will receive exactly one [Event] (or none, if the [EventBus] is dropped
    /// before an event occurs). This method returns a [handle][sof] that can be used to wait until
    /// the expected event occurs.
    ///
    /// [sof]: SubscriberOnceFuture
    pub fn register_once<T>(
        &self,
        subscriber_once: impl FnOnce(&mut Event<T>) + Send + 'static,
    ) -> Result<SubscriberOnceFuture, InvalidTypeIdError>
    where
        for<'a> T: 'a,
    {
        let type_id = TypeId::of::<T>();
        if self.valid_type_ids.contains(&type_id) {
            let subscriber_once: SubscriberFnOnce<T> = Box::new(subscriber_once);
            let mut once_pipes_map = self.once_pipes.upgradable_read();
            let once_pipe = if once_pipes_map.contains_key(&type_id) {
                &once_pipes_map[&type_id]
            } else {
                once_pipes_map.with_upgraded(|map| {
                    map.entry(type_id.clone())
                        .or_insert_with(SubscriberOncePipe::new::<T>);
                });
                &once_pipes_map[&type_id]
            };
            Ok(once_pipe.send(subscriber_once))
        } else {
            Err(InvalidTypeIdError { type_id })
        }
    }
}

/// Bus responsible for firing/publishing [Events][evt] from.
///
/// An individual EventBus represents a particular group of [Events][evt] that can be published.
/// For example, a Renderer has one EventBus that is responsible for all of that Renderer's events,
/// but no others.
///
/// An EventBus has an [EventBusRegistry] for registering [EventHandlers][eh], that can be
/// accessed via [EventBus::registry()].
///
/// # Handler Execution Order
///
/// [Handlers][es] that are registered from the same thread will be guaranteed to execute (loosely)
/// in the order that they were registered, but no guarantees about execution order are made beyond
/// that. For example, given handlers A and B registered in that order from the same thread, a third
/// handler C that is registered from a separate thread may or may not be executed between A and B.
///
/// It is recommended to plan accordingly, and design handlers to be as idempotent from each other
/// as possible.
///
/// # Cancelling Events
///
/// [Events][evt] that are considered [cancellable][etc] may be cancelled by any [handler][es] that
/// receives said event. This does *not* stop any further handlers from receiving that event, but
/// the call-site that fires the event may change it's logic based on the set
/// [cancellation flag][ec].
///
/// [evt]: Event
/// [eh]: EventHandler
/// [etc]: EventCancellable
/// [ec]: Event::is_cancelled
pub struct EventBus {
    registry: EventBusRegistry,
}

impl EventBus {
    /// Create an [EventBusBuilder].
    ///
    /// This is the recommended way to create an [EventBus].
    #[inline]
    pub fn builder() -> EventBusBuilder {
        EventBusBuilder::new()
    }

    /// Reference to this EventBus's [EventBusRegistry].
    #[inline]
    pub fn registry(&self) -> &EventBusRegistry { &self.registry }

    /// Fire/publish an [Event].
    ///
    /// Given user-derived event data, create an [Event] and send it to any registered
    /// [EventHandlers][eh] that match the event data's [TypeId]. All standard
    /// [EventHandlers][eh] receive the [Event] first, then any one-off handlers.
    ///
    /// The [Event] will be returned after all handlers are done with it, which allows any relevant
    /// data generated from handlers to be [extracted from it][evid].
    ///
    /// See also: [struct-level documentation](Self)
    ///
    /// [eh]: EventHandler
    /// [evid]: Event::into_data
    pub fn fire<T>(&self, event_data: T) -> Event<T>
    where
        for<'a> T: 'a,
    {
        let type_id = TypeId::of::<T>();
        let mut event = Event::new(event_data);

        let mut subscribers_map = self.registry.subscribers.write();
        let once_pipes_map = self.registry.once_pipes.read();

        if let Some(subscribers) = subscribers_map.get_mut(&type_id) {
            subscribers.retain(|subscriber| subscriber.handle_event(&mut event));
        }

        if let Some(once_pipe) = once_pipes_map.get(&type_id) {
            once_pipe.handle_event(&mut event);
        }

        event
    }
}

/// Builder pattern for [EventBus].
///
/// Usually created via [EventBus::builder()].
pub struct EventBusBuilder {
    valid_type_ids: HashSet<TypeId>,
}

impl EventBusBuilder {
    /// Create a new builder.
    ///
    /// It is recommended to use [EventBus::builder()] instead.
    #[inline]
    pub fn new() -> Self {
        Self {
            valid_type_ids: HashSet::new(),
        }
    }

    /// Add a [TypeId] that the [EventBus] will be expected to fire.
    ///
    /// If any [TypeIds](TypeId) are added, only events of that type are expected to be fired and
    /// handled, otherwise an [InvalidTypeIdError] will be yielded. If no TypeIds are added, any
    /// events may be fired and handled.
    #[inline]
    pub fn event_type<T>(&mut self) -> &mut Self
    where
        for<'a> T: 'a,
    {
        self.valid_type_ids.insert(TypeId::of::<T>());
        self
    }

    /// Build a new [EventBus].
    #[inline]
    pub fn build(&self) -> EventBus {
        EventBus {
            registry: EventBusRegistry {
                valid_type_ids: self.valid_type_ids.clone(),
                subscribers: RwLock::new(HashMap::new()),
                once_pipes: RwLock::new(HashMap::new()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol::future::FutureExt;
    use smol::{future, Timer};
    use std::future::Future;
    use std::time::Duration;

    async fn timeout<T>(fut: impl Future<Output = T>, time: Duration) -> T {
        let timeout_fn = async move {
            Timer::after(time).await;
            panic!("Timeout reached: {time:?}");
        };
        fut.or(timeout_fn).await
    }

    struct IncrementEvent(u32);

    struct CancellableIncrementEvent(u32);
    impl EventCancellable for CancellableIncrementEvent {}

    #[derive(Default)]
    struct IncrementHandler {}
    impl EventHandler<IncrementEvent> for IncrementHandler {
        fn handle_event(&self, event: &mut Event<IncrementEvent>) {
            event.data().0 += 1;
        }
    }

    fn create_event_bus() -> EventBus {
        EventBus::builder()
            .event_type::<IncrementEvent>()
            .event_type::<CancellableIncrementEvent>()
            .build()
    }

    #[test]
    fn test_fire_event_no_subscriber() {
        let event_bus = create_event_bus();

        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 0);

        // Repeat should work
        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 0);
    }

    #[test]
    fn test_fire_event_single_subscriber() {
        let event_bus = create_event_bus();
        event_bus.registry().register(IncrementHandler::default()).unwrap();

        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 1);

        // Repeat should work
        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 1);
    }

    #[test]
    fn test_fire_event_multiple_subscribers() {
        let event_bus = create_event_bus();
        for _ in 0..3 {
            event_bus.registry().register(IncrementHandler::default()).unwrap();
        }

        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 3);

        // Repeat should work
        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 3);
    }

    #[test]
    fn test_fire_event_multiple_types() {
        struct Inc1Event(u32);
        struct Inc2Event(u32);
        struct Inc4Event(u32);

        let event_bus = EventBus::builder()
            .event_type::<Inc1Event>()
            .event_type::<Inc2Event>()
            .event_type::<Inc4Event>()
            .build();

        event_bus.registry().register(|event: &mut Event<Inc1Event>| { event.data().0 += 1; }).unwrap();
        event_bus.registry().register(|event: &mut Event<Inc2Event>| { event.data().0 += 2; }).unwrap();
        event_bus.registry().register(|event: &mut Event<Inc4Event>| { event.data().0 += 4; }).unwrap();

        let result = event_bus.fire(Inc1Event(0));
        assert_eq!(result.into_data().0, 1);

        let result = event_bus.fire(Inc2Event(0));
        assert_eq!(result.into_data().0, 2);

        let result = event_bus.fire(Inc4Event(0));
        assert_eq!(result.into_data().0, 4);
    }

    #[test]
    fn test_fire_event_once_subscriber() {
        let event_bus = create_event_bus();
        let once_fut = event_bus.registry().register_once(|event: &mut Event<IncrementEvent>| {
            event.data().0 += 1;
        }).unwrap();

        // First should work
        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 1);
        future::block_on(timeout(once_fut, Duration::from_secs(1)))
            .expect("SubscriberOnce should not error");

        // Second should do nothing
        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 0);
    }

    #[test]
    fn test_fire_event_subscriber_and_once() {
        let event_bus = create_event_bus();
        event_bus.registry().register(IncrementHandler::default()).unwrap();
        let once_fut = event_bus.registry().register_once(|event: &mut Event<IncrementEvent>| {
            event.data().0 += 1;
        }).unwrap();

        // First should do both
        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 2);
        future::block_on(timeout(once_fut, Duration::from_secs(1)))
            .expect("SubscriberOnce should not error");

        // Second should do one
        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 1);
    }

    #[test]
    fn test_fire_event_cancelled() {
        let event_bus = create_event_bus();
        event_bus.registry().register(|event: &mut Event<CancellableIncrementEvent>| {
            event.data().0 += 1;
        }).unwrap();
        event_bus.registry().register(|event: &mut Event<CancellableIncrementEvent>| {
            event.data().0 += 1;
            event.cancel();
        }).unwrap();
        event_bus.registry().register(|event: &mut Event<CancellableIncrementEvent>| {
            event.data().0 += 1;
        }).unwrap();

        let result = event_bus.fire(CancellableIncrementEvent(0));
        assert!(result.is_cancelled());
        assert_eq!(result.into_data().0, 3);

        // Repeat should work
        let result = event_bus.fire(CancellableIncrementEvent(0));
        assert!(result.is_cancelled());
        assert_eq!(result.into_data().0, 3);
    }

    #[test]
    fn test_fire_event_weak() {
        let event_bus = create_event_bus();
        event_bus.registry().register(IncrementHandler::default()).unwrap();
        let handler2 = Arc::new(IncrementHandler::default());
        event_bus.registry().register(handler2.clone()).unwrap();
        let handler3 = Arc::new(IncrementHandler::default());
        event_bus.registry().register(Arc::downgrade(&handler3)).unwrap();

        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 3);

        // Drop the weak ref - *should* reduce handlers
        drop(handler3);
        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 2);

        // Drop the strong ref - shouldn't reduce handlers
        drop(handler2);
        let result = event_bus.fire(IncrementEvent(0));
        assert_eq!(result.into_data().0, 2);
    }
}