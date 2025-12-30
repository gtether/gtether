use std::collections::{BinaryHeap, HashMap, HashSet};
use std::{fmt, thread};
use std::any::Any;
use std::fmt::Formatter;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use crossbeam::channel::RecvTimeoutError;
use parking_lot::RwLock;
use tracing::{error, event, Level};
use winit::event::{DeviceEvent, KeyEvent as WinitKeyEvent, WindowEvent};

pub use winit::event::{ElementState, MouseButton, MouseScrollDelta};
pub use winit::keyboard::{Key as LogicalKey, KeyCode, KeyLocation, ModifiersState, PhysicalKey};

pub trait InputStateLayer {
    /// Query whether the given key is pressed.
    ///
    /// A [ModifierState] can optionally be specified. If it is None, then this will return true if
    /// the given key is pressed, regardless of modifiers. If it is Some, then this will only return
    /// true if the given key is pressed AND the _exact_ set of modifiers are also pressed.
    ///
    /// If the [InputState] is locked by an [InputDelegate], this will return None if the current
    /// layer does not have priority.
    fn is_key_pressed(&self, key: KeyCode, modifiers: Option<ModifiersState>) -> Option<bool>;

    /// Query whether the given mouse button is pressed.
    ///
    /// If the [InputState] is locked by an [InputDelegate], this will return None if the current
    /// layer does not have priority.
    fn is_mouse_pressed(&self, button: MouseButton) -> Option<bool>;

    /// Get the current position of the mouse cursor in the window.
    ///
    /// (0, 0) is top-left of the window.
    fn mouse_position(&self) -> glm::TVec2<f64>;
}

struct StateManager {
    keys: RwLock<HashSet<KeyCode>>,
    modifiers: RwLock<ModifiersState>,
    mouse_buttons: RwLock<HashSet<MouseButton>>,
    mouse_position: RwLock<glm::TVec2<f64>>,
}

impl Default for StateManager {
    fn default() -> Self {
        Self {
            keys: RwLock::new(HashSet::new()),
            modifiers: RwLock::new(ModifiersState::empty()),
            mouse_buttons: RwLock::new(HashSet::new()),
            mouse_position: RwLock::new(glm::vec2(0.0, 0.0)),
        }
    }
}

impl StateManager {
    fn set_key(&self, key_code: KeyCode, state: ElementState) {
        event!(Level::TRACE, ?key_code, ?state, "Key state change");
        let mut inner = self.keys.write();
        match state {
            ElementState::Pressed => inner.insert(key_code),
            ElementState::Released => inner.remove(&key_code),
        };
    }

    fn set_modifiers(&self, modifiers: ModifiersState) {
        event!(Level::TRACE, ?modifiers, "Modifiers state change");
        *self.modifiers.write() = modifiers;
    }

    fn set_mouse_button(&self, button: MouseButton, state: ElementState) {
        event!(Level::TRACE, ?button, ?state, "Mouse button state change");
        let mut mouse_buttons = self.mouse_buttons.write();
        match state {
            ElementState::Pressed => mouse_buttons.insert(button),
            ElementState::Released => mouse_buttons.remove(&button),
        };
    }

    fn set_mouse_position(&self, position: glm::TVec2<f64>) {
        *self.mouse_position.write() = position;
    }

    fn is_key_pressed(&self, key: KeyCode, modifiers: Option<ModifiersState>) -> bool {
        match modifiers {
            Some(modifiers) =>
                modifiers == *self.modifiers.read() && self.keys.read().contains(&key),
            None => self.keys.read().contains(&key),
        }
    }

    fn is_mouse_pressed(&self, button: MouseButton) -> bool {
        self.mouse_buttons.read().contains(&button)
    }

    fn mouse_position(&self) -> glm::TVec2<f64> {
        self.mouse_position.read().clone()
    }
}

/// Event representing an individual Key state change.
///
/// This event is effectively a recreation of [winit::event::KeyEvent], with crate-private parts
/// stripped away, in order to allow it to be created manually. For additional information on
/// various fields, please see [winit::event::KeyEvent] instead.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyEvent {
    pub physical_key: PhysicalKey,
    pub logical_key: LogicalKey,
    pub location: KeyLocation,
    pub state: ElementState,
    pub repeat: bool,
}

impl From<WinitKeyEvent> for KeyEvent {
    fn from(value: WinitKeyEvent) -> Self {
        Self {
            physical_key: value.physical_key,
            logical_key: value.logical_key,
            location: value.location,
            state: value.state,
            repeat: value.repeat,
        }
    }
}

/// Event representing an individual Mouse Button state change.
///
/// In addition to conveying the affected mouse button and the state it has changed to, this event
/// also contains the position that the event occurred at.
#[derive(Debug, Clone, PartialEq)]
pub struct MouseButtonEvent {
    /// Which [MouseButton] changed state.
    pub button: MouseButton,
    /// What [ElementState] the given button is now in.
    pub state: ElementState,
    /// The current mouse position in the window when the state change happened. (0, 0) is top-left.
    pub position: glm::TVec2<f64>,
}

/// Event representing the mouse scroll wheel moving.
#[derive(Debug, Clone, PartialEq)]
pub struct MouseWheelEvent {
    /// The amount that has been scrolled.
    ///
    /// See [MouseScrollDelta] for more information on possible values.
    pub delta: MouseScrollDelta,
}

/// Events that are consumed by [InputDelegate]s when inputs happen.
#[derive(Clone, Debug, PartialEq)]
pub enum InputDelegateEvent {
    /// A key changed state.
    Key(KeyEvent),
    /// A mouse button changed state.
    MouseButton(MouseButtonEvent),
    /// The cursor moved. This happens after window-specific post-processing, and the value is the
    /// current position of the cursor relative to the window.
    CursorMoved(glm::TVec2<f64>),
    /// The mouse moved. This happens before window-specific post-processing, and so is more
    /// suitable for motion controlled applications such as 3d camera movement. The value is the
    /// delta amount that the mouse moved.
    MouseMotion(glm::TVec2<f64>),
    /// The mouse scroll wheel moved.
    MouseWheel(MouseWheelEvent),
}

/// Lock struct for locking [InputDelegate]s.
///
/// This lock struct will automatically unlock the relevant [InputDelegate] when it is dropped, so
/// it can be used to maintain a lock within a given scope.
pub struct InputDelegateLock {
    id: usize,
    manager: Arc<DelegateManager>,
}

impl InputDelegateLock {
    fn lock(manager: Arc<DelegateManager>, id: usize, priority: u32) -> Self {
        manager.lock(id, priority);

        Self {
            id,
            manager,
        }
    }
}

impl Drop for InputDelegateLock {
    fn drop(&mut self) {
        self.manager.unlock(self.id);
    }
}

/// A delegate that can be used to operate on individual input events instead of just querying
/// active state.
///
/// All input events that occur will be sent to delegates, even duplicates. This makes it more
/// suitable if the count of input changes between ticks is required.
pub struct InputDelegate {
    id: usize,
    receiver: crossbeam::channel::Receiver<InputDelegateEvent>,
    manager: Arc<DelegateManager>,
}

impl fmt::Debug for InputDelegate {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputDelegate")
            .field("id", &self.id)
            .finish()
    }
}

impl InputDelegate {
    /// Get an iterator for all [InputDelegateEvent]s that have happened since events were last
    /// polled.
    pub fn events(&self) -> InputDelegateIter<'_> {
        InputDelegateIter {
            inner: self
        }
    }

    /// Lock this delegate so that input events are only sent here.
    ///
    /// [InputDelegate]s can be locked with a given priority. If _any_ [InputDelegate]s belonging to
    /// a particular [InputState] are locked, then [InputDelegateEvent]s will only be sent to the
    /// delegate with the highest locking priority.
    ///
    /// Multiple [InputDelegate]s can be locked at once, but only the highest priority one will
    /// receive events.
    ///
    /// If _any_ [InputDelegate]s are locked, then the owning [InputState] will also return default
    /// states for any queries (e.g. querying a key state will always return that is unpressed).
    pub fn lock(&self, priority: u32) -> InputDelegateLock {
        InputDelegateLock::lock(self.manager.clone(), self.id, priority)
    }

    /// Start a loop that continually waits for new events and fires the provided handler.
    ///
    /// This function will never terminate, and so is suitable to initialize a separate thread for
    /// input event handling.
    ///
    /// The loop will block and wait for new events when done handling current events, and does not
    /// operate at a consistent interval. If consistent intervals are required (such as 60 "ticks"
    /// per second), it may be more suitable to use a custom loop that calls [Self::events()].
    // TODO: Is 'mut handler' and 'FnMut' correct here?
    pub fn start(self, mut handler: impl FnMut(&InputDelegate, InputDelegateEvent)) -> ! {
        loop {
            // TODO: Possibly break the loop if an error occurred? As that means the sender was disconnected
            let event = self.receiver.recv().unwrap();
            handler(&self, event);
        }
    }

    pub fn spawn(
        self,
        mut handler: impl FnMut(&InputDelegate, InputDelegateEvent) + Send + 'static,
    ) -> InputDelegateJoinHandle {
        let should_exit = Arc::new(AtomicBool::new(false));
        let thread_should_exit = should_exit.clone();

        let thread_name = if let Some(parent_name) = thread::current().name() {
            parent_name.to_owned() + "-input-delegate"
        } else {
            "input-delegate".to_owned()
        };

        let join_handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                while !thread_should_exit.load(Ordering::Relaxed) {
                    let timeout = Duration::from_secs_f32(0.1);
                    match self.receiver.recv_timeout(timeout) {
                        Ok(event) => {
                            // TODO: Error handling?
                            handler(&self, event);
                        },
                        Err(RecvTimeoutError::Timeout) => {},
                        Err(RecvTimeoutError::Disconnected) => {
                            error!(
                                thread = ?thread::current().name(),
                                "Input delegate channel disconnected; halting thread",
                            );
                            break;
                        }
                    };
                }
            })
            .unwrap();

        InputDelegateJoinHandle {
            join_handle,
            should_exit,
        }
    }
}

impl InputStateLayer for InputDelegate {
    fn is_key_pressed(&self, key: KeyCode, modifiers: Option<ModifiersState>) -> Option<bool> {
        self.manager.is_key_pressed(self.id, key, modifiers)
    }

    fn is_mouse_pressed(&self, button: MouseButton) -> Option<bool> {
        self.manager.is_mouse_pressed(self.id, button)
    }

    fn mouse_position(&self) -> glm::TVec2<f64> {
        self.manager.state.mouse_position()
    }
}

pub struct InputDelegateIter<'a> {
    inner: &'a InputDelegate,
}

impl<'a> Iterator for InputDelegateIter<'a> {
    type Item = InputDelegateEvent;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.receiver.try_recv() {
            Ok(event) => Some(event),
            Err(crossbeam::channel::TryRecvError::Empty) => None,
            Err(crossbeam::channel::TryRecvError::Disconnected) => panic!("Server hung up"),
        }
    }
}

#[derive(Debug)]
pub struct InputDelegateJoinHandle {
    join_handle: JoinHandle<()>,
    should_exit: Arc<AtomicBool>,
}

impl InputDelegateJoinHandle {
    pub fn join(self) -> Result<(), Box<dyn Any + Send>> {
        self.should_exit.store(true, Ordering::Relaxed);
        self.join_handle.join()
    }
}

#[derive(Clone, Debug, Eq)]
struct DelegateLockEntry {
    id: usize,
    priority: u32,
}

impl PartialEq for DelegateLockEntry {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl PartialOrd for DelegateLockEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.priority.partial_cmp(&other.priority)
    }
}

impl Ord for DelegateLockEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }
}

struct DelegateManager {
    state: Arc<StateManager>,
    next_id: AtomicUsize,
    senders: RwLock<HashMap<usize, crossbeam::channel::Sender<InputDelegateEvent>>>,
    locks: RwLock<BinaryHeap<DelegateLockEntry>>,
}

impl fmt::Debug for DelegateManager {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("DelegateManager")
            .field("next_id", &self.next_id)
            .field("delegate_ids", &self.senders.read().keys().collect::<Vec<_>>())
            .field("locks", &*self.locks.read())
            .finish()
    }
}

impl DelegateManager {
    fn new(state: Arc<StateManager>) -> Self {
        Self {
            state,
            next_id: AtomicUsize::new(0),
            senders: RwLock::new(HashMap::new()),
            locks: RwLock::new(BinaryHeap::new()),
        }
    }

    fn create_delegate(manager: &Arc<DelegateManager>) -> InputDelegate {
        let (sender, receiver) = crossbeam::channel::unbounded();
        let id = manager.next_id.fetch_add(1, Ordering::Relaxed);

        manager.senders.write().insert(id, sender);

        InputDelegate {
            id,
            receiver,
            manager: manager.clone(),
        }
    }

    fn lock(&self, id: usize, priority: u32) {
        if !self.senders.read().contains_key(&id) {
            panic!("Tried to lock input delegate (id=={id}) that doesn't exist in this manager {self:?}");
        }

        self.locks.write().push(DelegateLockEntry {
            id,
            priority,
        });

        event!(Level::TRACE, id, priority, "Locked input delegate");
    }

    fn unlock(&self, id: usize) {
        if !self.senders.read().contains_key(&id) {
            panic!("Tried to unlock input delegate (id=={id}) that doesn't exist in this manager {self:?}");
        }

        self.locks.write().retain(|lock| lock.id != id);

        event!(Level::TRACE, id, "Unlocked input delegate");
    }

    fn is_locked(&self) -> bool {
        !self.locks.read().is_empty()
    }

    fn send_event(&self, event: InputDelegateEvent) {
        let mut invalid_ids = vec![];
        let mut senders = self.senders.upgradable_read();

        let locks = self.locks.read();
        if let Some(locked) = locks.peek() {
            if senders[&locked.id].send(event).is_err() {
                // Receiver has hung up
                invalid_ids.push(locked.id.clone());
            }
        } else {
            for (id, sender) in &*senders {
                if sender.send(event.clone()).is_err() {
                    // Receiver has hung up
                    invalid_ids.push(id.clone());
                }
            }
        }

        if !invalid_ids.is_empty() {
            senders.with_upgraded(|senders| {
                for id in invalid_ids {
                    senders.remove(&id);
                }
            })
        }
    }

    fn has_priority(&self, id: usize) -> bool {
        if let Some(top) = self.locks.read().peek() {
            top.id == id
        } else {
            true
        }
    }

    fn is_key_pressed(&self, id: usize, key: KeyCode, modifiers: Option<ModifiersState>) -> Option<bool> {
        if !self.has_priority(id) {
            return None;
        } else {
            Some(self.state.is_key_pressed(key, modifiers))
        }
    }

    fn is_mouse_pressed(&self, id: usize, button: MouseButton) -> Option<bool> {
        if !self.has_priority(id) {
            return None;
        } else {
            Some(self.state.is_mouse_pressed(button))
        }
    }
}

pub(crate) enum InputEvent {
    Key(KeyEvent),
    Modifiers(ModifiersState),
    MouseInput{ button: MouseButton, state: ElementState },
    MouseWheel { delta: MouseScrollDelta },
    CursorMoved(glm::TVec2<f64>),
    MouseMotion(glm::TVec2<f64>),
}

impl InputEvent {
    pub(crate) fn from_window_event(event: WindowEvent) -> Option<Self> {
        match event {
            WindowEvent::KeyboardInput { event, .. } => {
                Some(Self::Key(event.into()))
            },
            WindowEvent::ModifiersChanged(modifiers) => {
                Some(Self::Modifiers(modifiers.state()))
            },
            WindowEvent::MouseInput { state, button, .. } => {
                Some(Self::MouseInput { button, state })
            },
            WindowEvent::MouseWheel { delta, .. } => {
                Some(Self::MouseWheel { delta })
            },
            WindowEvent::CursorMoved { position, .. } => {
                Some(Self::CursorMoved(glm::vec2(position.x, position.y)))
            },
            _ => None,
        }
    }

    pub(crate) fn from_device_event(event: DeviceEvent) -> Option<Self> {
        match event {
            DeviceEvent::MouseMotion { delta } => {
                Some(Self::MouseMotion(glm::vec2(delta.0, delta.1)))
            },
            _ => None,
        }
    }
}

/// A managed state of current inputs.
///
/// This state will keep track of all active inputs, and allow them to be queried by other logic.
///
/// This state is thread-safe, and so is suitable for use with e.g. an Arc<>.
pub struct InputState {
    state: Arc<StateManager>,
    delegates: Arc<DelegateManager>,
}

impl Default for InputState {
    fn default() -> Self {
        let state = Arc::new(StateManager::default());
        Self {
            state: state.clone(),
            delegates: Arc::new(DelegateManager::new(state)),
        }
    }
}

impl InputState {
    pub(crate) fn handle_event(&self, event: InputEvent) {
        match event {
            InputEvent::Key(event) => {
                let key_code = match event.physical_key {
                    PhysicalKey::Code(key_code) => Some(key_code),
                    PhysicalKey::Unidentified(native_key_code) => {
                        event!(Level::WARN, "Unknown key code: {native_key_code:?}");
                        None
                    }
                };

                if !event.repeat {
                    if let Some(key_code) = key_code {
                        self.state.set_key(key_code, event.state);
                    }
                }

                self.delegates.send_event(InputDelegateEvent::Key(event));
            },
            InputEvent::Modifiers(modifiers) => {
                self.state.set_modifiers(modifiers);
            },
            InputEvent::MouseInput { button, state } => {
                self.state.set_mouse_button(button, state);

                self.delegates.send_event(InputDelegateEvent::MouseButton(MouseButtonEvent {
                    button,
                    state,
                    position: self.state.mouse_position(),
                }))
            },
            InputEvent::MouseWheel { delta } => {
                self.delegates.send_event(InputDelegateEvent::MouseWheel(MouseWheelEvent {
                    delta,
                }))
            },
            InputEvent::CursorMoved(position) => {
                self.state.set_mouse_position(position);

                self.delegates.send_event(InputDelegateEvent::CursorMoved(position.clone()))
            },
            InputEvent::MouseMotion(motion) => {
                self.delegates.send_event(InputDelegateEvent::MouseMotion(motion.clone()))
            },
        }
    }

    /// Create an [InputDelegate] which will receive copies of input events.
    #[inline]
    pub fn create_delegate(&self) -> InputDelegate {
        DelegateManager::create_delegate(&self.delegates)
    }

    /// Whether the [InputState] is currently priority locked by a delegate.
    ///
    /// If the [InputState] is locked, events will only be sent to the locking delegate, and all
    /// queries about input states will return a default state (e.g. 'false' for key presses).
    #[inline]
    pub fn is_locked_by_delegate(&self) -> bool {
        self.delegates.is_locked()
    }
}

impl InputStateLayer for InputState {
    #[inline]
    fn is_key_pressed(&self, key: KeyCode, modifiers: Option<ModifiersState>) -> Option<bool> {
        if self.delegates.is_locked() {
            None
        } else {
            Some(self.state.is_key_pressed(key, modifiers))
        }
    }

    #[inline]
    fn is_mouse_pressed(&self, button: MouseButton) -> Option<bool> {
        if self.delegates.is_locked() {
            None
        } else {
            Some(self.state.is_mouse_pressed(button))
        }
    }

    #[inline]
    fn mouse_position(&self) -> glm::TVec2<f64> {
        self.state.mouse_position()
    }
}

#[cfg(test)]
mod tests {
    use winit::keyboard::NativeKey;

    use super::*;

    fn create_key_event(key: KeyCode, state: ElementState) -> KeyEvent {
        KeyEvent {
            physical_key: PhysicalKey::Code(key),
            // TODO: Use the actual proper LogicalKey if it's required for these tests
            logical_key: LogicalKey::Unidentified(NativeKey::Unidentified),
            location: KeyLocation::Standard,
            state,
            repeat: false,
        }
    }

    fn do_key_input(input: &InputState, key: KeyCode, state: ElementState) {
        input.handle_event(InputEvent::Key(create_key_event(key, state)));
    }

    fn do_modifiers(input: &InputState, modifiers: ModifiersState) {
        input.handle_event(InputEvent::Modifiers(modifiers));
    }

    fn do_mouse_input(input: &InputState, button: MouseButton, state: ElementState) {
        input.handle_event(InputEvent::MouseInput { button, state });
    }

    fn do_cursor_moved(input: &InputState, position: glm::TVec2<f64>) {
        input.handle_event(InputEvent::CursorMoved(position))
    }

    fn do_mouse_motion(input: &InputState, motion: glm::TVec2<f64>) {
        input.handle_event(InputEvent::MouseMotion(motion))
    }

    #[test]
    fn test_key_pressed() {
        let input = InputState::default();

        do_key_input(&input, KeyCode::KeyX, ElementState::Pressed);
        do_key_input(&input, KeyCode::KeyY, ElementState::Released);
        assert!(input.is_key_pressed(KeyCode::KeyX, None).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyY, None).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyZ, None).unwrap());

        do_key_input(&input, KeyCode::KeyX, ElementState::Pressed);
        assert!(input.is_key_pressed(KeyCode::KeyX, None).unwrap());

        do_key_input(&input, KeyCode::KeyX, ElementState::Released);
        assert!(!input.is_key_pressed(KeyCode::KeyX, None).unwrap());

        do_key_input(&input, KeyCode::KeyX, ElementState::Released);
        assert!(!input.is_key_pressed(KeyCode::KeyX, None).unwrap());
    }

    #[test]
    fn test_key_pressed_modifiers() {
        let input = InputState::default();

        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)).unwrap());

        do_key_input(&input, KeyCode::KeyX, ElementState::Pressed);
        assert!(input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)).unwrap());

        do_modifiers(&input, ModifiersState::SHIFT);
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())).unwrap());
        assert!(input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)).unwrap());

        do_modifiers(&input, ModifiersState::SHIFT | ModifiersState::ALT);
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)).unwrap());
        assert!(input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)).unwrap());

        do_key_input(&input, KeyCode::KeyX, ElementState::Released);
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)).unwrap());
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)).unwrap());
    }

    #[test]
    fn test_key_pressed_locked() {
        let input = InputState::default();
        let delegate = input.create_delegate();

        do_key_input(&input, KeyCode::KeyX, ElementState::Pressed);
        assert!(input.is_key_pressed(KeyCode::KeyX, None).unwrap());

        let lock = delegate.lock(10);
        assert!(input.is_key_pressed(KeyCode::KeyX, None).is_none());

        // Unlocking should once again allow the state to be queried
        drop(lock);
        assert!(input.is_key_pressed(KeyCode::KeyX, None).unwrap());
    }

    #[test]
    fn test_mouse_pressed() {
        let input = InputState::default();

        assert!(!input.is_mouse_pressed(MouseButton::Left).unwrap());
        assert!(!input.is_mouse_pressed(MouseButton::Right).unwrap());

        do_mouse_input(&input, MouseButton::Left, ElementState::Pressed);
        assert!(input.is_mouse_pressed(MouseButton::Left).unwrap());
        assert!(!input.is_mouse_pressed(MouseButton::Right).unwrap());

        do_mouse_input(&input, MouseButton::Left, ElementState::Released);
        assert!(!input.is_mouse_pressed(MouseButton::Left).unwrap());
        assert!(!input.is_mouse_pressed(MouseButton::Right).unwrap());
    }

    #[test]
    fn test_mouse_pressed_locked() {
        let input = InputState::default();
        let delegate = input.create_delegate();

        do_mouse_input(&input, MouseButton::Left, ElementState::Pressed);
        assert!(input.is_mouse_pressed(MouseButton::Left).unwrap());

        let lock = delegate.lock(10);
        assert!(input.is_mouse_pressed(MouseButton::Left).is_none());

        // Unlocking should once again allow the state to be queried
        drop(lock);
        assert!(input.is_mouse_pressed(MouseButton::Left).unwrap());
    }

    #[test]
    fn test_mouse_position() {
        let input = InputState::default();

        assert_eq!(input.mouse_position(), glm::vec2(0.0, 0.0));

        do_cursor_moved(&input, glm::vec2(64.0, 100.1));
        assert_eq!(input.mouse_position(), glm::vec2(64.0, 100.1));

        do_cursor_moved(&input, glm::vec2(9001.420, 0.0));
        assert_eq!(input.mouse_position(), glm::vec2(9001.420, 0.0));
    }

    #[test]
    fn test_delegate_key_event() {
        let input = InputState::default();
        let delegate = input.create_delegate();

        let events = delegate.events().collect::<Vec<_>>();
        assert!(events.is_empty());

        do_key_input(&input, KeyCode::KeyX, ElementState::Pressed);
        do_key_input(&input, KeyCode::Enter, ElementState::Released);
        do_key_input(&input, KeyCode::Enter, ElementState::Pressed);
        let events = delegate.events().collect::<Vec<_>>();
        assert_eq!(events, vec![
            InputDelegateEvent::Key(create_key_event(KeyCode::KeyX, ElementState::Pressed)),
            InputDelegateEvent::Key(create_key_event(KeyCode::Enter, ElementState::Released)),
            InputDelegateEvent::Key(create_key_event(KeyCode::Enter, ElementState::Pressed)),
        ]);

        // Duplicate events should still be tracked
        do_key_input(&input, KeyCode::Tab, ElementState::Pressed);
        do_key_input(&input, KeyCode::Tab, ElementState::Pressed);
        do_key_input(&input, KeyCode::Tab, ElementState::Pressed);
        do_key_input(&input, KeyCode::Tab, ElementState::Released);
        let events = delegate.events().collect::<Vec<_>>();
        assert_eq!(events, vec![
            InputDelegateEvent::Key(create_key_event(KeyCode::Tab, ElementState::Pressed)),
            InputDelegateEvent::Key(create_key_event(KeyCode::Tab, ElementState::Pressed)),
            InputDelegateEvent::Key(create_key_event(KeyCode::Tab, ElementState::Pressed)),
            InputDelegateEvent::Key(create_key_event(KeyCode::Tab, ElementState::Released)),
        ]);
    }

    #[test]
    fn test_delegate_mouse_button_event() {
        let input = InputState::default();
        let delegate = input.create_delegate();

        let events = delegate.events().collect::<Vec<_>>();
        assert!(events.is_empty());

        do_mouse_input(&input, MouseButton::Left, ElementState::Pressed);
        do_cursor_moved(&input, glm::vec2(30.0, 166.6));
        do_mouse_input(&input, MouseButton::Left, ElementState::Released);
        do_cursor_moved(&input, glm::vec2(72.0, 0.0));
        do_mouse_input(&input, MouseButton::Middle, ElementState::Pressed);
        do_mouse_input(&input, MouseButton::Middle, ElementState::Pressed);
        let events = delegate.events().collect::<Vec<_>>();
        assert_eq!(events, vec![
            InputDelegateEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Left,
                state: ElementState::Pressed,
                position: glm::vec2(0.0, 0.0),
            }),
            InputDelegateEvent::CursorMoved(glm::vec2(30.0, 166.6)),
            InputDelegateEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Left,
                state: ElementState::Released,
                position: glm::vec2(30.0, 166.6),
            }),
            InputDelegateEvent::CursorMoved(glm::vec2(72.0, 0.0)),
            InputDelegateEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Middle,
                state: ElementState::Pressed,
                position: glm::vec2(72.0, 0.0),
            }),
            InputDelegateEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Middle,
                state: ElementState::Pressed,
                position: glm::vec2(72.0, 0.0),
            }),
        ]);
    }

    #[test]
    fn test_delegate_mouse_motion_event() {
        let input = InputState::default();
        let delegate = input.create_delegate();

        let events = delegate.events().collect::<Vec<_>>();
        assert!(events.is_empty());

        do_mouse_motion(&input, glm::vec2(0.0, 42.0));
        do_mouse_motion(&input, glm::vec2(-101.0, 99.99));
        do_mouse_motion(&input, glm::vec2(-403.12, 236.46));
        let events = delegate.events().collect::<Vec<_>>();
        assert_eq!(events, vec![
            InputDelegateEvent::MouseMotion(glm::vec2(0.0, 42.0)),
            InputDelegateEvent::MouseMotion(glm::vec2(-101.0, 99.99)),
            InputDelegateEvent::MouseMotion(glm::vec2(-403.12, 236.46)),
        ]);
    }

    #[test]
    fn test_delegate_mixed_events() {
        let input = InputState::default();
        let delegate = input.create_delegate();

        let events = delegate.events().collect::<Vec<_>>();
        assert!(events.is_empty());

        do_key_input(&input, KeyCode::KeyX, ElementState::Pressed);
        do_mouse_input(&input, MouseButton::Left, ElementState::Pressed);
        do_mouse_motion(&input, glm::vec2(0.0, 42.0));
        let events = delegate.events().collect::<Vec<_>>();
        assert_eq!(events, vec![
            InputDelegateEvent::Key(create_key_event(KeyCode::KeyX, ElementState::Pressed)),
            InputDelegateEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Left,
                state: ElementState::Pressed,
                position: glm::vec2(0.0, 0.0),
            }),
            InputDelegateEvent::MouseMotion(glm::vec2(0.0, 42.0)),
        ]);
    }

    #[test]
    fn test_multiple_delegates() {
        let input = InputState::default();
        let delegate_1 = input.create_delegate();
        let delegate_2 = input.create_delegate();
        let delegate_3 = input.create_delegate();

        do_key_input(&input, KeyCode::Escape, ElementState::Pressed);
        do_key_input(&input, KeyCode::Escape, ElementState::Released);

        let events_1 = delegate_1.events().collect::<Vec<_>>();
        assert_eq!(events_1, vec![
            InputDelegateEvent::Key(create_key_event(KeyCode::Escape, ElementState::Pressed)),
            InputDelegateEvent::Key(create_key_event(KeyCode::Escape, ElementState::Released)),
        ]);

        let events_2 = delegate_2.events().collect::<Vec<_>>();
        assert_eq!(events_2, vec![
            InputDelegateEvent::Key(create_key_event(KeyCode::Escape, ElementState::Pressed)),
            InputDelegateEvent::Key(create_key_event(KeyCode::Escape, ElementState::Released)),
        ]);

        let events_3 = delegate_3.events().collect::<Vec<_>>();
        assert_eq!(events_3, vec![
            InputDelegateEvent::Key(create_key_event(KeyCode::Escape, ElementState::Pressed)),
            InputDelegateEvent::Key(create_key_event(KeyCode::Escape, ElementState::Released)),
        ]);
    }

    #[test]
    fn test_delegate_queries() {
        let input = InputState::default();
        let delegate_1 = input.create_delegate();
        let delegate_2 = input.create_delegate();

        do_key_input(&input, KeyCode::KeyA, ElementState::Pressed);
        assert!(delegate_1.is_key_pressed(KeyCode::KeyA, None).unwrap());
        assert!(!delegate_1.is_key_pressed(KeyCode::KeyS, None).unwrap());
        assert!(delegate_2.is_key_pressed(KeyCode::KeyA, None).unwrap());
        assert!(!delegate_2.is_key_pressed(KeyCode::KeyS, None).unwrap());
    }

    #[test]
    fn test_locked_delegate_events() {
        let input = InputState::default();
        let delegate_1 = input.create_delegate();
        let delegate_2 = input.create_delegate();
        let delegate_3 = input.create_delegate();

        let _lock_2 = delegate_2.lock(10);
        let lock_3 = delegate_3.lock(99);

        do_key_input(&input, KeyCode::Escape, ElementState::Pressed);
        let events_1 = delegate_1.events().collect::<Vec<_>>();
        assert!(events_1.is_empty());
        let events_2 = delegate_2.events().collect::<Vec<_>>();
        assert!(events_2.is_empty());
        let events_3 = delegate_3.events().collect::<Vec<_>>();
        assert_eq!(events_3, vec![
            InputDelegateEvent::Key(create_key_event(KeyCode::Escape, ElementState::Pressed)),
        ]);

        // Dropping lock_3 should revert to delegate_2 receiving events
        drop(lock_3);
        do_key_input(&input, KeyCode::Escape, ElementState::Pressed);
        let events_1 = delegate_1.events().collect::<Vec<_>>();
        assert!(events_1.is_empty());
        let events_2 = delegate_2.events().collect::<Vec<_>>();
        assert_eq!(events_2, vec![
            InputDelegateEvent::Key(create_key_event(KeyCode::Escape, ElementState::Pressed)),
        ]);
        let events_3 = delegate_3.events().collect::<Vec<_>>();
        assert!(events_3.is_empty());
    }

    #[test]
    fn test_locked_delegate_queries() {
        let input = InputState::default();
        let delegate_1 = input.create_delegate();
        let delegate_2 = input.create_delegate();
        let delegate_3 = input.create_delegate();

        let _lock_2 = delegate_2.lock(10);
        let lock_3 = delegate_3.lock(99);

        do_key_input(&input, KeyCode::KeyA, ElementState::Pressed);
        assert!(delegate_1.is_key_pressed(KeyCode::KeyA, None).is_none());
        assert!(delegate_1.is_key_pressed(KeyCode::KeyS, None).is_none());
        assert!(delegate_2.is_key_pressed(KeyCode::KeyA, None).is_none());
        assert!(delegate_2.is_key_pressed(KeyCode::KeyS, None).is_none());
        assert!(delegate_3.is_key_pressed(KeyCode::KeyA, None).unwrap());
        assert!(!delegate_3.is_key_pressed(KeyCode::KeyS, None).unwrap());

        // Dropping lock_3 should revert to delegate_2 being able to query
        drop(lock_3);
        assert!(delegate_1.is_key_pressed(KeyCode::KeyA, None).is_none());
        assert!(delegate_1.is_key_pressed(KeyCode::KeyS, None).is_none());
        assert!(delegate_2.is_key_pressed(KeyCode::KeyA, None).unwrap());
        assert!(!delegate_2.is_key_pressed(KeyCode::KeyS, None).unwrap());
        assert!(delegate_3.is_key_pressed(KeyCode::KeyA, None).is_none());
        assert!(delegate_3.is_key_pressed(KeyCode::KeyS, None).is_none());
    }
}