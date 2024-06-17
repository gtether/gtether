use std::collections::HashSet;
use std::fmt;
use std::fmt::Formatter;

use parking_lot::RwLock;
use tracing::{event, Level};
use ump::Error;
use winit::event::{DeviceEvent, KeyEvent as WinitKeyEvent, WindowEvent};

pub use winit::event::{ElementState, MouseButton};
pub use winit::keyboard::{Key as LogicalKey, KeyCode, KeyLocation, ModifiersState, PhysicalKey};

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

/// Events that are consumed by [InputDelegate]s when inputs happen.
#[derive(Clone, Debug, PartialEq)]
pub enum InputDelegateEvent {
    /// A key changed state.
    Key(KeyEvent),
    /// A mouse button changed state.
    MouseButton(MouseButtonEvent),
    /// The mouse moved. This happens before window-specific post-processing, and so is more
    /// suitable for motion controlled applications such as 3d camera movement.
    MouseMotion(glm::TVec2<f64>),
}

/// A delegate that can be used to operate on individual input events instead of just querying
/// active state.
///
/// All input events that occur will be sent to delegates, even duplicates. This makes it more
/// suitable if the count of input changes between ticks is required.
pub struct InputDelegate {
    endpoint: ump::Server<InputDelegateEvent, (), ()>,
}

impl fmt::Debug for InputDelegate {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputDelegate").finish()
    }
}

impl InputDelegate {
    /// Get an iterator for all [InputDelegateEvent]s that have happened since events were last
    /// polled.
    pub fn events(&self) -> InputDelegateIter {
        InputDelegateIter {
            delegate: self
        }
    }

    /// Start a loop that continually waits for new events and fires the provided handler.
    ///
    /// This function will never terminate, and so is suitable to initialize a separate thread for
    /// input event handling.
    ///
    /// The loop will block and wait for new events when done handling current events, and does not
    /// operate at a consistent interval. If consistent intervals are required (such as 60 "ticks"
    /// per second), it may be more suitable to use a custom loop that calls [Self::events()].
    pub fn start(self, handler: impl Fn(InputDelegateEvent)) -> ! {
        loop {
            let (input, rctx) = self.endpoint.wait().unwrap();
            handler(input);
            rctx.reply(()).unwrap();
        }
    }
}

pub struct InputDelegateIter<'a> {
    delegate: &'a InputDelegate,
}

impl<'a> Iterator for InputDelegateIter<'a> {
    type Item = InputDelegateEvent;

    fn next(&mut self) -> Option<Self::Item> {
        let (event, rctx) = self.delegate.endpoint.try_pop().unwrap()?;
        rctx.reply(()).unwrap();
        Some(event)
    }
}

pub(in crate::gui) enum InputEvent {
    Key(KeyEvent),
    Modifiers(ModifiersState),
    MouseInput{ button: MouseButton, state: ElementState },
    CursorMoved(glm::TVec2<f64>),
    MouseMotion(glm::TVec2<f64>),
}

impl InputEvent {
    pub(in crate::gui) fn from_window_event(event: WindowEvent) -> Option<Self> {
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
            WindowEvent::CursorMoved { position, .. } => {
                Some(Self::CursorMoved(glm::vec2(position.x, position.y)))
            },
            _ => None,
        }
    }

    pub(in crate::gui) fn from_device_event(event: DeviceEvent) -> Option<Self> {
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
    keys: RwLock<HashSet<KeyCode>>,
    modifiers: RwLock<ModifiersState>,
    mouse_buttons: RwLock<HashSet<MouseButton>>,
    mouse_position: RwLock<glm::TVec2<f64>>,
    delegate_senders: RwLock<Vec<ump::Client<InputDelegateEvent, (), ()>>>,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            keys: RwLock::new(HashSet::new()),
            modifiers: RwLock::new(ModifiersState::empty()),
            mouse_buttons: RwLock::new(HashSet::new()),
            mouse_position: RwLock::new(glm::vec2(0.0, 0.0)),
            delegate_senders: RwLock::new(Vec::new()),
        }
    }
}

impl InputState {
    fn send_delegate_event(&self, event: InputDelegateEvent) {
        let mut invalid_indices = vec![];
        let mut delegates = self.delegate_senders.upgradable_read();
        for (idx, delegate) in delegates.iter().enumerate() {
            match delegate.req_async(event.clone()) {
                Ok(_) => {
                    // Don't care about the response
                },
                Err(Error::ServerDisappeared) => {
                    invalid_indices.push(idx);
                },
                Err(e) => {
                    panic!("Unrecoverable pipe error: {e}");
                },
            }
        }

        if !invalid_indices.is_empty() {
            delegates.with_upgraded(|delegates| {
                for idx in invalid_indices.into_iter().rev() {
                    delegates.remove(idx);
                }
            })
        }
    }

    pub(in crate::gui) fn handle_event(&self, event: InputEvent) {
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
                        let state = event.state;
                        event!(Level::TRACE, "Key event: {key_code:?} - {state:?}");
                        let mut inner = self.keys.write();
                        match state {
                            ElementState::Pressed => inner.insert(key_code),
                            ElementState::Released => inner.remove(&key_code),
                        };
                    }
                }

                self.send_delegate_event(InputDelegateEvent::Key(event));
            },
            InputEvent::Modifiers(modifiers) => {
                *self.modifiers.write() = modifiers;
            },
            InputEvent::MouseInput { button, state } => {
                let mut mouse_buttons = self.mouse_buttons.write();
                match state {
                    ElementState::Pressed => mouse_buttons.insert(button),
                    ElementState::Released => mouse_buttons.remove(&button),
                };

                self.send_delegate_event(InputDelegateEvent::MouseButton(MouseButtonEvent {
                    button,
                    state,
                    position: self.mouse_position.read().clone(),
                }))
            },
            InputEvent::CursorMoved(position) => {
                *self.mouse_position.write() = position;
            },
            InputEvent::MouseMotion(motion) => {
                self.send_delegate_event(InputDelegateEvent::MouseMotion(motion.clone()))
            },
        }
    }

    /// Create an [InputDelegate] which will receive copies of input events.
    pub fn create_delegate(&self) -> InputDelegate {
        let (endpoint, sender) = ump::channel();

        self.delegate_senders.write().push(sender);

        InputDelegate {
            endpoint,
        }
    }

    /// Query whether the given key is pressed.
    ///
    /// A [ModifierState] can optionally be specified. If it is None, then this will return true if
    /// the given key is pressed, regardless of modifiers. If it is Some, then this will only return
    /// true if the given key is pressed AND the _exact_ set of modifiers are also pressed.
    #[inline]
    pub fn is_key_pressed(&self, key: KeyCode, modifiers: Option<ModifiersState>) -> bool {
        match modifiers {
            Some(modifiers)
                => modifiers == *self.modifiers.read() && self.keys.read().contains(&key),
            None => self.keys.read().contains(&key),
        }
    }

    /// Query whether the given mouse button is pressed.
    #[inline]
    pub fn is_mouse_pressed(&self, button: MouseButton) -> bool {
        self.mouse_buttons.read().contains(&button)
    }

    /// Get the current position of the mouse cursor in the window.
    ///
    /// (0, 0) is top-left of the window.
    #[inline]
    pub fn mouse_position(&self) -> glm::TVec2<f64> {
        self.mouse_position.read().clone()
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
        assert!(input.is_key_pressed(KeyCode::KeyX, None));
        assert!(!input.is_key_pressed(KeyCode::KeyY, None));
        assert!(!input.is_key_pressed(KeyCode::KeyZ, None));

        do_key_input(&input, KeyCode::KeyX, ElementState::Pressed);
        assert!(input.is_key_pressed(KeyCode::KeyX, None));

        do_key_input(&input, KeyCode::KeyX, ElementState::Released);
        assert!(!input.is_key_pressed(KeyCode::KeyX, None));

        do_key_input(&input, KeyCode::KeyX, ElementState::Released);
        assert!(!input.is_key_pressed(KeyCode::KeyX, None));
    }

    #[test]
    fn test_key_pressed_modifiers() {
        let input = InputState::default();

        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())));
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)));
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)));

        do_key_input(&input, KeyCode::KeyX, ElementState::Pressed);
        assert!(input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())));
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)));
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)));

        do_modifiers(&input, ModifiersState::SHIFT);
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())));
        assert!(input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)));
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)));

        do_modifiers(&input, ModifiersState::SHIFT | ModifiersState::ALT);
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())));
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)));
        assert!(input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)));

        do_key_input(&input, KeyCode::KeyX, ElementState::Released);
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::empty())));
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT)));
        assert!(!input.is_key_pressed(KeyCode::KeyX, Some(ModifiersState::SHIFT | ModifiersState::ALT)));
    }

    #[test]
    fn test_mouse_pressed() {
        let input = InputState::default();

        assert!(!input.is_mouse_pressed(MouseButton::Left));
        assert!(!input.is_mouse_pressed(MouseButton::Right));

        do_mouse_input(&input, MouseButton::Left, ElementState::Pressed);
        assert!(input.is_mouse_pressed(MouseButton::Left));
        assert!(!input.is_mouse_pressed(MouseButton::Right));

        do_mouse_input(&input, MouseButton::Left, ElementState::Released);
        assert!(!input.is_mouse_pressed(MouseButton::Left));
        assert!(!input.is_mouse_pressed(MouseButton::Right));
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
            InputDelegateEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Left,
                state: ElementState::Released,
                position: glm::vec2(30.0, 166.6),
            }),
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
}