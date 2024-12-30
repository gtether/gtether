//! Event lifecycles.
//!
//! This module contains the framework for creating, driving, and hooking into
//! [event-driven buses][eb]. These event buses follow a publish-subscribe pattern, where a
//! particular code site may [publish][ebf] an event, which is then propagated to all
//! [subscribers][es] of that [event type][et].
//!
//! These event buses are primarily synchronous, in that any subscribed event handlers are triggered
//! inline on the same thread that fires the event.
//!
//! [eb]: EventBus
//! [ebf]: EventBus::fire
//! [es]: EventSubscriber
//! [et]: EventType

use parking_lot::RwLock;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Weak};

/// User-defined type for [events][evt].
///
/// EventTypes should be simply defined, and using an enum to represent one is common.
///
/// # Examples
/// ```
/// use gtether::event::EventType;
///
/// #[derive(Clone, Debug, Hash, PartialEq, Eq)]
/// pub enum MyEventType {
///     Type1,
///     Type2,
/// }
///
/// impl EventType for MyEventType {}
/// ```
///
/// [evt]: Event
pub trait EventType: Clone + Debug + Hash + Eq + 'static {
    fn is_cancellable(&self) -> bool { false }
}

/// Instance of an event that has been published.
///
/// Each instance of Event represents a single event that has been fired/published, and will be
/// handled by possibly many [subscribers][es]. This struct effectively represents a pairing of
/// [EventType] and user-derived data.
///
/// [es]: EventSubscriber
pub struct Event<T: EventType, D> {
    event_type: T,
    event_data: D,
    cancelled: bool,
}

impl<T: EventType, D> Event<T, D> {
    fn new(event_type: T, event_data: D) -> Self {
        Self {
            event_type,
            event_data,
            cancelled: false,
        }
    }

    /// [EventType] associated with this Event.
    #[inline]
    pub fn event_type(&self) -> &T { &self.event_type }

    /// User-derived data associated with this Event.
    #[inline]
    pub fn event_data(&mut self) -> &mut D { &mut self.event_data }

    /// Consume this event and yield the data it contains.
    ///
    /// This is usually used after an event has been fired, to retrieve any data that
    /// [subscribers][es] may have added to it.
    ///
    /// # Examples
    /// ```
    /// # use gtether::event::{EventBus, EventType};
    /// #
    /// # fn wrapper<T: EventType>(event_type: T, input_data: u32, event_bus: EventBus<T, u32>) {
    /// let event = event_bus.fire(event_type, input_data);
    /// let output_data = event.into_data();
    /// // Do something with 'output_data'...
    /// # }
    /// ```
    ///
    /// [es]: EventSubscriber
    #[inline]
    pub fn into_data(self) -> D { self.event_data }

    /// Cancel this Event.
    ///
    /// Not every event is cancellable. An event is considered cancellable if it's associated
    /// [EventType::is_cancellable()] returns true.
    ///
    /// # Panics
    ///
    /// Panics if the associated [EventType] is not cancellable. See [Self::try_cancel()] for a
    /// version of this method that does not panic.
    pub fn cancel(&mut self) {
        if self.event_type.is_cancellable() {
            self.cancelled = true;
        } else {
            panic!("Event type is not cancellable: {:?}", &self.event_type)
        }
    }

    /// Attempt to cancel this Event.
    ///
    /// Not every event is cancellable. An event is considered cancellable if it's associated
    /// [EventType::is_cancellable()] returns true.
    ///
    /// # Errors
    ///
    /// Errors if the associated [EventType] is not cancellable.
    pub fn try_cancel(&mut self) -> Result<(), ()> {
        if self.event_type.is_cancellable() {
            self.cancelled = true;
            Ok(())
        } else {
            Err(())
        }
    }

    /// Whether this Event has been cancelled.
    ///
    /// Returns true if the event has been cancelled, false otherwise. If the associated [EventType]
    /// is not cancellable, will always return false.
    #[inline]
    pub fn is_cancelled(&self) -> bool { self.cancelled }
}

impl<T: EventType, D> Deref for Event<T, D> {
    type Target = D;

    #[inline]
    fn deref(&self) -> &Self::Target { &self.event_data }
}

impl<T: EventType, D> DerefMut for Event<T, D> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.event_data }
}

pub type SubscriberFn<T, D> = Arc<dyn Fn(&mut Event<T, D>) + Send + Sync + 'static>;
pub type SubscriberFnOnce<T, D> = Box<dyn FnOnce(&mut Event<T, D>) + Send>;

/// User-defined event handler.
///
/// This trait can be used to make a complex struct-based event handler. If a simple function event
/// handler is desired, a function that aligns with [SubscriberFn] can be used instead. See
/// [EventSubscriber] for more.
pub trait EventHandler<T: EventType, D>: Send + Sync + 'static {
    /// Handle a particular [Event] that has been fired.
    fn handle_event(&self, event: &mut Event<T, D>);
}

/// Event handler subscribed to specific [Events][evt].
///
/// An EventSubscriber is triggered when any of the [Events][evt] that it is listening for are fired.
/// Any given EventSubscriber can be re-used for multiple [EventTypes][et]. An EventSubscriber can
/// be created using several different methods, depending on the use case.
///
/// If you have a simple event handler, you can create an EventSubscriber from a closure that
/// matches [SubscriberFn]. These closures are wrapped in Arc, so they can still be re-used and/or
/// used for multiple [EventTypes][et], but they are generally used in a "subscribe-and-forget"
/// manner.
///
/// For more complex cases, a struct-based event handler that implements [EventHandler] can be used.
/// These are also expected to be wrapped in Arc, but generally can handle more complex interactions.
///
/// Lastly, if you expect your struct-based event handler to have a limited lifetime, you can create
/// an EventSubscriber from a Weak reference.
///
/// [evt]: Event
/// [et]: EventType
pub enum EventSubscriber<T: EventType, D> {
    Strong(Arc<dyn EventHandler<T, D>>),
    Weak(Weak<dyn EventHandler<T, D>>),
    Fn(SubscriberFn<T, D>),
}

impl<T, D, H> From<Arc<H>> for EventSubscriber<T, D>
where
    T: EventType,
    H: EventHandler<T, D>,
{
    #[inline]
    fn from(value: Arc<H>) -> Self { Self::Strong(value) }
}

impl<T, D, H> From<Weak<H>> for EventSubscriber<T, D>
where
    T: EventType,
    H: EventHandler<T, D>,
{
    #[inline]
    fn from(value: Weak<H>) -> Self { Self::Weak(value) }
}

impl<T, D, F> From<F> for EventSubscriber<T, D>
where
    T: EventType,
    F: Fn(&mut Event<T, D>) + Send + Sync + 'static,
{
    #[inline]
    fn from(value: F) -> Self { Self::Fn(Arc::new(value)) }
}

impl<T: EventType, D> Clone for EventSubscriber<T, D> {
    fn clone(&self) -> Self {
        match self {
            Self::Strong(handler) => Self::Strong(handler.clone()),
            Self::Weak(handler) => Self::Weak(handler.clone()),
            Self::Fn(handler) => Self::Fn(handler.clone()),
        }
    }
}

impl<T: EventType, D: 'static> EventSubscriber<T, D> {
    fn handle_event(&self, event: &mut Event<T, D>) -> bool {
        match self {
            Self::Strong(handler) => {
                handler.handle_event(event);
                true
            },
            Self::Weak(handler) => {
                if let Some(handler) = handler.upgrade() {
                    handler.handle_event(event);
                    true
                } else {
                    false
                }
            },
            Self::Fn(handler) => {
                handler(event);
                true
            },
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

/// Handle for a queued [SubscriberFnOnce].
///
/// Can be waited on either [synchronously][sync] or [asynchronously][async].
///
/// [sync]: SubscriberOnceFuture::wait_blocking
/// [async]: SubscriberOnceFuture::wait
pub struct SubscriberOnceFuture(Result<ump::WaitReply<(), ()>, ump::Error<()>>);

impl SubscriberOnceFuture {
    fn map_ump_error(error: ump::Error<()>) -> SubscriberOnceError {
        match error {
            ump::Error::ServerDisappeared | ump::Error::ClientsDisappeared | ump::Error::NoReply
            => SubscriberOnceError::Dropped,
            ump::Error::App(e) => panic!("Unexpected App error: {e:?}"),
        }
    }

    /// Asynchronously wait for a [SubscriberFnOnce] to complete.
    ///
    /// # Examples
    /// ```
    /// # use gtether::event::{EventBus, EventType, SubscriberOnceError};
    /// use gtether::event::Event;
    ///
    /// # async fn wrapper<T: EventType, D>(event_type: T, event_bus: EventBus<T, D>) -> Result<(), SubscriberOnceError> {
    /// let fut = event_bus.registry().register_once(event_type, |event: &mut Event<T, D>| {
    ///     // Some one-time action
    /// });
    /// fut.wait().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub async fn wait(self) -> Result<(), SubscriberOnceError> {
        self.0.map_err(Self::map_ump_error)?
            .wait_async().await.map_err(Self::map_ump_error)
    }

    /// Synchronously wait for a [SubscriberFnOnce] to complete.
    ///
    /// This method should NOT be used on the same thread that runs the [EventBus] the one-off
    /// handler was registered to, as this would cause a deadlock.
    ///
    /// # Examples
    /// ```
    /// # use gtether::event::{EventBus, EventType, SubscriberOnceError};
    /// use gtether::event::Event;
    ///
    /// # fn wrapper<T: EventType, D>(event_type: T, event_bus: EventBus<T, D>) -> Result<(), SubscriberOnceError> {
    /// let fut = event_bus.registry().register_once(event_type, |event: &mut Event<T, D>| {
    ///     // Some one-time action
    /// });
    /// fut.wait_blocking()?;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn wait_blocking(self) -> Result<(), SubscriberOnceError> {
        self.0.map_err(Self::map_ump_error)?
            .wait().map_err(Self::map_ump_error)
    }
}

struct SubscriberOncePipe<T: EventType, D> {
    client: ump::Client<SubscriberFnOnce<T, D>, (), ()>,
    server: ump::Server<SubscriberFnOnce<T, D>, (), ()>,
}

impl<T: EventType, D> Default for SubscriberOncePipe<T, D> {
    #[inline]
    fn default() -> Self {
        let (server, client) = ump::channel();
        Self {
            client,
            server,
        }
    }
}

impl<T: EventType, D: 'static> SubscriberOncePipe<T, D> {
    fn send(&self, subscriber: SubscriberFnOnce<T, D>) -> SubscriberOnceFuture {
        SubscriberOnceFuture(self.client.req_async(subscriber))
    }

    fn handle_event(&self, event: &mut Event<T, D>) {
        while let Some((subscriber, ctx)) = self.server.try_pop().unwrap() {
            subscriber(event);
            ctx.reply(()).unwrap();
        }
    }
}

/// Helper type for passing parameters of [EventTypes][et].
///
/// Not meant to be used directly, but is used by several functions in this module to accept either
/// a single or a collection of [EventTypes][et].
///
/// [et]: EventType
#[derive(Default)]
pub struct EventTypeList<T: EventType>(Vec<T>);

impl<T: EventType> From<Vec<T>> for EventTypeList<T> {
    #[inline]
    fn from(value: Vec<T>) -> Self { Self(value) }
}

impl<T: EventType, const N: usize> From<[T; N]> for EventTypeList<T> {
    #[inline]
    fn from(value: [T; N]) -> Self { Self(value.into_iter().collect()) }
}

impl<T: EventType> From<T> for EventTypeList<T> {
    #[inline]
    fn from(value: T) -> Self { Self(vec![value.into()]) }
}

/// Registry for registering new [EventSubscribers][es] to.
///
/// This is the public-facing part of [EventBus], where users can register additional
/// [EventSubscribers][es] to receive published [Events][evt]. In addition to registering standard
/// handlers with [EventBusRegistry::register()], temporary one-off handlers can be registered using
/// [EventBusRegistry::register_once()]. These will receive exactly one [Event][evt], and can be
/// used for scheduling actions according to certain lifecycle breakpoints.
///
/// [es]: EventSubscriber
/// [evt]: Event
pub struct EventBusRegistry<T: EventType, D> {
    subscribers: RwLock<HashMap<T, Vec<EventSubscriber<T, D>>>>,
    once_pipes: RwLock<HashMap<T, SubscriberOncePipe<T, D>>>,
}

impl<T: EventType, D: 'static> EventBusRegistry<T, D> {

    /// Register a new [EventSubscriber].
    ///
    /// Multiple [EventTypes][et] can be specified, and the [EventSubscriber] will be cloned for
    /// each [EventType][et].
    ///
    /// [et]: EventType
    /// [evt]: Event
    pub fn register(
        &self,
        event_types: impl Into<EventTypeList<T>>,
        subscriber: impl Into<EventSubscriber<T, D>>,
    ) {
        self.register_impl(
            event_types.into().0,
            subscriber.into(),
        )
    }

    fn register_impl(&self, event_types: Vec<T>, subscriber: EventSubscriber<T, D>) {
        let mut subscribers_map = self.subscribers.write();
        for event_type in event_types {
            subscribers_map.entry(event_type).or_default().push(subscriber.clone());
        }
    }

    /// Register a one-off event handler.
    ///
    /// This one-off handler will receive exactly one [Event] (or none, if the [EventBus] is dropped
    /// before an event occurs). This method returns a [handle][sof] that can be used to wait until
    /// the expected event occurs.
    ///
    /// [sof]: SubscriberOnceFuture
    pub fn register_once(
        &self,
        event_type: impl Into<T>,
        subscriber: impl FnOnce(&mut Event<T, D>) + Send + 'static,
    ) -> SubscriberOnceFuture {
        self.register_once_impl(event_type.into(), Box::new(subscriber))
    }

    fn register_once_impl(&self, event_type: T, subscriber: SubscriberFnOnce<T, D>) -> SubscriberOnceFuture {
        let mut once_pipes_map = self.once_pipes.upgradable_read();
        let once_pipe = if once_pipes_map.contains_key(&event_type) {
            &once_pipes_map[&event_type]
        } else {
            once_pipes_map.with_upgraded(|map| {
                map.entry(event_type.clone()).or_default();
            });
            &once_pipes_map[&event_type]
        };
        once_pipe.send(subscriber)
    }
}

/// Bus responsible for firing/publishing [Events][evt] from.
///
/// An individual EventBus represents a particular group of [Events][evt] that can be published.
/// For example, a Renderer has one EventBus that is responsible for all of that Renderer's events,
/// but no others.
///
/// An EventBus has an [EventBusRegistry] for registering [EventSubscribers][es], that can be
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
/// [es]: EventSubscriber
/// [etc]: EventType::is_cancellable
/// [ec]: Event::is_cancelled
pub struct EventBus<T: EventType, D> {
    registry: EventBusRegistry<T, D>,
}

impl<T: EventType, D> Default for EventBus<T, D> {
    fn default() -> Self {
        Self {
            registry: EventBusRegistry {
                subscribers: RwLock::new(HashMap::new()),
                once_pipes: RwLock::new(HashMap::new()),
            }
        }
    }
}

impl<T: EventType, D: 'static> EventBus<T, D> {
    /// Reference to this EventBus's [EventBusRegistry].
    #[inline]
    pub fn registry(&self) -> &EventBusRegistry<T, D> { &self.registry }

    /// Fire/publish an [Event].
    ///
    /// Given a particular [EventType] and user-derived event data, create an [Event] and send it
    /// to any registered [EventSubscribers][es]. All standard [EventSubscribers][es] receive the
    /// [Event] first, then any one-off handlers.
    ///
    /// The [Event] will be returned after all handlers are done with it, which allows any relevant
    /// data generated from handlers to be [extracted from it][evid].
    ///
    /// See also: [struct-level documentation](Self)
    ///
    /// [es]: EventSubscriber
    /// [evc]: EventType::is_cancellable
    /// [evid]: Event::into_data
    pub fn fire(&self, event_type: T, event_data: D) -> Event<T, D> {
        let mut event = Event::new(event_type.clone(), event_data);

        let mut subscribers_map = self.registry.subscribers.write();
        let once_pipes_map = self.registry.once_pipes.read();

        if let Some(subscribers) = subscribers_map.get_mut(&event_type) {
            subscribers.retain(|subscriber| subscriber.handle_event(&mut event));
        }

        if let Some(once_pipe) = once_pipes_map.get(&event_type) {
            once_pipe.handle_event(&mut event);
        }

        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    struct StringEventType(String);

    impl EventType for StringEventType {}

    impl From<&str> for StringEventType {
        fn from(value: &str) -> Self { Self(value.to_owned()) }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    struct CancellableStringEventType(String);

    impl EventType for CancellableStringEventType {
        fn is_cancellable(&self) -> bool { true }
    }

    impl From<&str> for CancellableStringEventType {
        fn from(value: &str) -> Self { Self(value.to_owned()) }
    }

    struct IncrementHandler {}

    impl<T: EventType> EventHandler<T, u32> for IncrementHandler {
        fn handle_event(&self, event: &mut Event<T, u32>) {
            *event.event_data() += 1;
        }
    }

    #[test]
    fn test_fire_event_no_subscriber() {
        let event_bus = EventBus::<StringEventType, u32>::default();

        let mut result = event_bus.fire("no_event".into(), 0);
        assert_eq!(*result.event_data(), 0);

        // Repeat should work
        let mut result = event_bus.fire("no_event".into(), 0);
        assert_eq!(*result.event_data(), 0);
    }

    #[test]
    fn test_fire_event_one_subscriber() {
        let event_bus = EventBus::<StringEventType, u32>::default();
        event_bus.registry().register(StringEventType::from("inc1"), |event: &mut Event<StringEventType, u32>| {
            *event.event_data() += 1;
        });

        let mut result = event_bus.fire("inc1".into(), 0);
        assert_eq!(*result.event_data(), 1);

        // Repeat should work
        let mut result = event_bus.fire("inc1".into(), 0);
        assert_eq!(*result.event_data(), 1);
    }

    #[test]
    fn test_fire_event_three_subscriber() {
        let event_bus = EventBus::<StringEventType, u32>::default();
        for _ in 0..3 {
            event_bus.registry().register(StringEventType::from("inc3"), |event: &mut Event<StringEventType, u32>| {
                *event.event_data() += 1;
            });
        }

        let mut result = event_bus.fire("inc3".into(), 0);
        assert_eq!(*result.event_data(), 3);

        // Repeat should work
        let mut result = event_bus.fire("inc3".into(), 0);
        assert_eq!(*result.event_data(), 3);
    }

    #[test]
    fn test_fire_event_three_types() {
        let event_bus = EventBus::<StringEventType, u32>::default();
        event_bus.registry().register(
            ["inc1".into(), "inc2".into(), "inc3".into()],
            |event: &mut Event<StringEventType, u32>| {
                *event.event_data() += 1;
            },
        );

        for i in 1..4 {
            let result = event_bus.fire(format!("inc{i}").as_str().into(), 0);
            assert_eq!(result.into_data(), 1);
        }
    }

    #[test]
    fn test_fire_event_once_subscriber() {
        let event_bus = EventBus::<StringEventType, u32>::default();
        let once_fut = event_bus.registry().register_once(StringEventType::from("inc"), |event| {
            *event.event_data() += 1;
        });

        // First should work
        let mut result = event_bus.fire("inc".into(), 0);
        assert_eq!(*result.event_data(), 1);
        // TODO: wait for future with timeout

        // Second should do nothing
        let mut result = event_bus.fire("inc".into(), 0);
        assert_eq!(*result.event_data(), 0);
    }

    #[test]
    fn test_fire_event_subscriber_and_once() {
        let event_bus = EventBus::<StringEventType, u32>::default();
        event_bus.registry().register(StringEventType::from("inc"), |event: &mut Event<StringEventType, u32>| {
            *event.event_data() += 1;
        });
        let once_fut = event_bus.registry().register_once(StringEventType::from("inc"), |event| {
            *event.event_data() += 1;
        });

        // First should do both
        let mut result = event_bus.fire("inc".into(), 0);
        assert_eq!(*result.event_data(), 2);
        // TODO: wait for future with timeout

        // Second should do one
        let mut result = event_bus.fire("inc".into(), 0);
        assert_eq!(*result.event_data(), 1);
    }

    #[test]
    fn test_fire_event_cancelled() {
        let event_bus = EventBus::<CancellableStringEventType, u32>::default();
        event_bus.registry().register(CancellableStringEventType::from("event"), |event: &mut Event<CancellableStringEventType, u32>| {
            *event.event_data() += 1;
        });
        event_bus.registry().register(CancellableStringEventType::from("event"), |event: &mut Event<CancellableStringEventType, u32>| {
            *event.event_data() += 1;
            event.cancel();
        });
        event_bus.registry().register(CancellableStringEventType::from("event"), |event: &mut Event<CancellableStringEventType, u32>| {
            *event.event_data() += 1;
        });

        let result = event_bus.fire("event".into(), 0);
        assert!(result.is_cancelled());
        assert_eq!(result.into_data(), 3);

        // Repeat should work
        let result = event_bus.fire("event".into(), 0);
        assert!(result.is_cancelled());
        assert_eq!(result.into_data(), 3);
    }

    #[test]
    fn test_fire_event_weak() {
        let event_bus = EventBus::<StringEventType, u32>::default();
        let handler1 = Arc::new(IncrementHandler {});
        event_bus.registry().register(StringEventType::from("event"), handler1.clone());
        let handler2 = Arc::new(IncrementHandler {});
        event_bus.registry().register(StringEventType::from("event"), handler2.clone());
        let handler3 = Arc::new(IncrementHandler {});
        event_bus.registry().register(StringEventType::from("event"), Arc::downgrade(&handler3));
        let handler4 = Arc::new(IncrementHandler {});
        event_bus.registry().register(StringEventType::from("event"), Arc::downgrade(&handler4));

        let result = event_bus.fire("event".into(), 0);
        assert_eq!(result.into_data(), 4);

        // Drop 1 strong and 1 weak
        drop(handler2);
        drop(handler4);

        // Only the 1 weak should no longer be handling
        let result = event_bus.fire("event".into(), 0);
        assert_eq!(result.into_data(), 3);
    }
}