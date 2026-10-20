//! Frame-based input state, fed from winit events.
//!
//! winit delivers input as a stream of one-shot events; almost every caller
//! actually wants to ask *state* questions ("is Ctrl held right now?",
//! "where is the mouse?") at an arbitrary point in a frame. [`Input`] sits
//! between the two: [`Input::handle_event`] absorbs the event stream,
//! everything else is a cheap state query callable from anywhere.
//!
//! The frame contract is the one thing a caller has to get right:
//!
//! 1. forward every `WindowEvent` to [`Input::handle_event`] (and every
//!    `DeviceEvent` to [`Input::handle_device_event`], if raw motion is
//!    wanted),
//! 2. read whatever state the frame needs,
//! 3. call [`Input::end_frame`] exactly once at the end of the frame.
//!
//! `end_frame` is what makes the edge-triggered queries (`is_key_pressed`,
//! `is_key_released`, `mouse_delta`, `scroll_delta`, `text`) mean "since the
//! last frame" rather than "since the program started". Skipping it leaves
//! every edge query permanently true.
//!
//! **Stuck-key safety.** Losing focus mid-chord (alt-tab while holding
//! Ctrl) is the classic way input state desyncs: the OS delivers the press
//! but the release goes to whoever has focus now. [`WindowEvent::Focused`]`(false)`
//! therefore force-releases every held key, button, and modifier — see
//! [`Input::release_all`]. Positions are kept (the cursor didn't move), only
//! held-ness is dropped.

use std::collections::HashSet;

use winit::{
    dpi::{LogicalPosition, LogicalSize},
    event::{DeviceEvent, ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent},
    keyboard::{KeyCode, ModifiersState, PhysicalKey},
    window::Window,
};

/// Logical pixels one "line" of scrolling is worth, used to put
/// [`MouseScrollDelta::PixelDelta`] (touchpads, high-resolution wheels) on
/// the same scale as [`MouseScrollDelta::LineDelta`] (classic notched
/// wheels) so callers only deal with one unit.
// ponytail: one constant instead of surfacing both units separately —
// revisit if smooth-scroll physics ever needs the true pixel deltas.
const PIXELS_PER_LINE: f32 = 20.0;

/// The platform's text-input conventions, which differ in which modifier
/// chords produce characters instead of shortcuts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Windows,
    MacOs,
    Other,
}

impl Platform {
    const CURRENT: Platform = if cfg!(target_os = "windows") {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Other
    };
}

/// Whether the held modifiers make a key press a shortcut, so the text it
/// carries must not be typed.
///
/// - **Windows** reports AltGr (used by French, German, Spanish, Swiss, ...
///   layouts for characters like `@ ~ { } [ ] €`) as Ctrl+Alt held together,
///   so that combination types; Ctrl or Alt alone is a shortcut.
/// - **macOS** composes characters with Option (`å`, `€`, dead keys), so
///   Option alone types; Control and Command are shortcuts.
/// - **Elsewhere** AltGr is a distinct level-3 shift that winit does not
///   report as Ctrl or Alt, so any held Ctrl, Alt or Super is a shortcut.
///
/// Shift never suppresses: it only selects the shifted character.
fn suppresses_text(modifiers: ModifiersState, platform: Platform) -> bool {
    if modifiers.super_key() {
        return true;
    }
    match platform {
        Platform::Windows => modifiers.control_key() ^ modifiers.alt_key(),
        Platform::MacOs => modifiers.control_key(),
        Platform::Other => modifiers.control_key() || modifiers.alt_key(),
    }
}

/// One input occurrence, in the order the OS delivered it. Frame-level
/// queries on [`Input`] answer "what happened this frame"; this stream
/// answers "in what order", which an editor needs so that, say, a mode
/// change from one key applies before the character typed right after it.
///
/// A key press that produces characters yields [`InputEvent::Key`] followed
/// immediately by [`InputEvent::Text`].
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// A physical key edge. `code` is `None` for keys winit cannot identify.
    /// `modifiers` are those held at the moment of the event.
    Key {
        code: Option<KeyCode>,
        state: ElementState,
        repeat: bool,
        modifiers: ModifiersState,
    },
    /// Characters to insert into a document: typed text, or an IME commit
    /// (never truncated). Control characters are stripped.
    Text(String),
    /// Provisional IME composition, replacing any previous one wholesale.
    /// Not part of the document until committed. `cursor` is the caret or
    /// selection as byte offsets into `text`. Empty `text` ends the
    /// composition.
    Preedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    MouseButton {
        button: MouseButton,
        state: ElementState,
        /// Logical pixels.
        position: (f32, f32),
        modifiers: ModifiersState,
    },
    MouseMoved {
        /// Logical pixels.
        position: (f32, f32),
        delta: (f32, f32),
    },
    /// Scroll in lines.
    Scroll { delta: (f32, f32) },
    /// Keyboard focus changed. Losing focus force-releases everything held.
    Focus(bool),
}

/// An in-progress IME composition. See [`Input::preedit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preedit {
    pub text: String,
    /// Caret or selection as byte offsets into `text`.
    pub cursor: Option<(usize, usize)>,
}

/// Aggregated input state for the current frame. See the module doc for the
/// event-forwarding/`end_frame` contract.
#[derive(Debug)]
pub struct Input {
    /// Physical-to-logical divisor, kept in sync from
    /// `WindowEvent::ScaleFactorChanged` so mouse positions come out in the
    /// same logical pixels the renderer's `draw_*` API takes.
    scale_factor: f64,

    keys_down: HashSet<KeyCode>,
    keys_pressed: HashSet<KeyCode>,
    keys_typed: HashSet<KeyCode>,
    keys_released: HashSet<KeyCode>,
    modifiers: ModifiersState,

    buttons_down: HashSet<MouseButton>,
    buttons_pressed: HashSet<MouseButton>,
    buttons_released: HashSet<MouseButton>,

    mouse_position: (f32, f32),
    /// `None` until the first `CursorMoved`, so the first movement doesn't
    /// report a delta measured from a fictional origin.
    last_mouse_position: Option<(f32, f32)>,
    mouse_delta: (f32, f32),
    raw_mouse_delta: (f32, f32),
    scroll_delta: (f32, f32),

    text: String,
    /// Current IME composition; persists across frames until committed or
    /// cancelled, unlike the per-frame edge state.
    preedit: Option<Preedit>,
    events: Vec<InputEvent>,

    /// Caret rectangle (top-left, size) in logical pixels for the OS
    /// candidate window, set by [`Input::set_ime_cursor_area`] and applied
    /// by [`Input::sync_ime`].
    ime_area: Option<((f32, f32), (f32, f32))>,
    ime_area_dirty: bool,
    ime_enabled: bool,

    focused: bool,
    cursor_in_window: bool,
}

// The whole query surface is the point of this type; `main.rs` only drives
// the event/frame plumbing, so most queries have no caller until editor code
// lands. Same situation (and same treatment) as the renderer's draw API.
impl Input {
    /// `scale_factor` is the window's current DPI scale (`window.scale_factor()`);
    /// it is kept current from `ScaleFactorChanged` afterwards.
    pub fn new(scale_factor: f64) -> Self {
        Self {
            scale_factor,
            keys_down: HashSet::new(),
            keys_pressed: HashSet::new(),
            keys_typed: HashSet::new(),
            keys_released: HashSet::new(),
            modifiers: ModifiersState::empty(),
            buttons_down: HashSet::new(),
            buttons_pressed: HashSet::new(),
            buttons_released: HashSet::new(),
            mouse_position: (0.0, 0.0),
            last_mouse_position: None,
            mouse_delta: (0.0, 0.0),
            raw_mouse_delta: (0.0, 0.0),
            scroll_delta: (0.0, 0.0),
            text: String::new(),
            preedit: None,
            events: Vec::new(),
            ime_area: None,
            ime_area_dirty: false,
            ime_enabled: false,
            focused: true,
            cursor_in_window: false,
        }
    }

    // ---- event intake ----------------------------------------------------

    /// Absorb one `WindowEvent`. Safe to call with every event — anything
    /// input-irrelevant is ignored.
    ///
    /// Returns whether the event actually changed input state, which is the
    /// signal a frame scheduler needs: `Moved`, `Occluded`, `ThemeChanged`
    /// and friends reach this method but must not wake the renderer.
    pub fn handle_event(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::KeyboardInput { event, .. } => {
                // Layout-independent identity: `KeyCode::KeyW` is the
                // physical W position regardless of the active layout, which
                // is what keybinds should be expressed against. A key with
                // no such identity is still user intent and may produce text.
                let code = match event.physical_key {
                    PhysicalKey::Code(code) => Some(code),
                    PhysicalKey::Unidentified(_) => None,
                };
                if let Some(code) = code {
                    match event.state {
                        ElementState::Pressed => {
                            self.keys_typed.insert(code);
                            // `repeat` is the OS auto-repeat stream from a
                            // key that's already held — not a new press edge.
                            if !event.repeat {
                                self.keys_pressed.insert(code);
                            }
                            self.keys_down.insert(code);
                        }
                        ElementState::Released => {
                            self.keys_down.remove(&code);
                            self.keys_released.insert(code);
                        }
                    }
                }
                self.events.push(InputEvent::Key {
                    code,
                    state: event.state,
                    repeat: event.repeat,
                    modifiers: self.modifiers,
                });
                if let Some(text) = &event.text
                    && event.state.is_pressed()
                    && !suppresses_text(self.modifiers, Platform::CURRENT)
                {
                    self.commit_text(text);
                }
                true
            }

            WindowEvent::Ime(ime) => {
                match ime {
                    Ime::Enabled => {}
                    Ime::Preedit(text, cursor) => self.set_preedit(text, *cursor),
                    Ime::Commit(text) => {
                        // The composition ends with its commit.
                        self.set_preedit("", None);
                        self.commit_text(text);
                    }
                    Ime::Disabled => self.set_preedit("", None),
                }
                true
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                true
            }

            WindowEvent::MouseInput { state, button, .. } => {
                match state {
                    ElementState::Pressed => {
                        self.buttons_down.insert(*button);
                        self.buttons_pressed.insert(*button);
                    }
                    ElementState::Released => {
                        self.buttons_down.remove(button);
                        self.buttons_released.insert(*button);
                    }
                }
                self.events.push(InputEvent::MouseButton {
                    button: *button,
                    state: *state,
                    position: self.mouse_position,
                    modifiers: self.modifiers,
                });
                true
            }

            WindowEvent::CursorMoved { position, .. } => {
                let next = (
                    (position.x / self.scale_factor) as f32,
                    (position.y / self.scale_factor) as f32,
                );
                let delta = self
                    .last_mouse_position
                    .map_or((0.0, 0.0), |last| (next.0 - last.0, next.1 - last.1));
                self.mouse_delta.0 += delta.0;
                self.mouse_delta.1 += delta.1;
                self.events.push(InputEvent::MouseMoved {
                    position: next,
                    delta,
                });
                self.mouse_position = next;
                self.last_mouse_position = Some(next);
                self.cursor_in_window = true;
                true
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (*x, *y),
                    MouseScrollDelta::PixelDelta(position) => (
                        position.x as f32 / PIXELS_PER_LINE,
                        position.y as f32 / PIXELS_PER_LINE,
                    ),
                };
                self.scroll_delta.0 += lines.0;
                self.scroll_delta.1 += lines.1;
                self.events.push(InputEvent::Scroll { delta: lines });
                true
            }

            WindowEvent::CursorEntered { .. } => {
                self.cursor_in_window = true;
                true
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_in_window = false;
                // The next `CursorMoved` after re-entry is a teleport, not a
                // drag — forget the old anchor so it doesn't report one huge
                // delta across the gap.
                self.last_mouse_position = None;
                true
            }

            WindowEvent::Focused(focused) => {
                self.focused = *focused;
                if !focused {
                    self.release_all();
                }
                self.events.push(InputEvent::Focus(*focused));
                true
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = *scale_factor;
                // Stale logical coordinates measured against the old factor
                // would produce a phantom delta at the next move.
                self.last_mouse_position = None;
                true
            }

            _ => false,
        }
    }

    fn commit_text(&mut self, text: &str) {
        // Control characters (Esc, Backspace, ...) arrive as `text` from
        // winit; a document wants typed characters, not control bytes.
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() {
            return;
        }
        self.text.push_str(&text);
        self.events.push(InputEvent::Text(text));
    }

    fn set_preedit(&mut self, text: &str, cursor: Option<(usize, usize)>) {
        let next = (!text.is_empty()).then(|| Preedit {
            text: text.to_owned(),
            cursor,
        });
        if next != self.preedit {
            self.events.push(InputEvent::Preedit {
                text: text.to_owned(),
                cursor,
            });
            self.preedit = next;
        }
    }

    /// Absorb one `DeviceEvent`, for raw (unaccelerated, not clipped to the
    /// window) mouse motion — see [`Input::raw_mouse_delta`]. Optional:
    /// skip wiring this up entirely if only cursor-space motion is needed.
    pub fn handle_device_event(&mut self, event: &DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            self.raw_mouse_delta.0 += delta.0 as f32;
            self.raw_mouse_delta.1 += delta.1 as f32;
        }
    }

    /// Force-release everything currently held, reporting each as a release
    /// edge this frame so callers with press/release pairing (drag handlers,
    /// held-key repeat) unwind cleanly instead of waiting forever for a
    /// release event that will never arrive.
    ///
    /// Called automatically on focus loss; also useful manually when the
    /// app takes over input itself (opening a modal, entering a capture
    /// mode).
    pub fn release_all(&mut self) {
        self.keys_released.extend(self.keys_down.drain());
        self.buttons_released.extend(self.buttons_down.drain());
        self.modifiers = ModifiersState::empty();
        self.mouse_delta = (0.0, 0.0);
        self.raw_mouse_delta = (0.0, 0.0);
        self.scroll_delta = (0.0, 0.0);
    }

    /// End the current frame: clear everything that means "since last
    /// frame". Held state (`is_key_down`, `mouse_position`, `modifiers`)
    /// survives; edges and deltas do not. Call exactly once per frame,
    /// after the frame has read what it needs.
    pub fn end_frame(&mut self) {
        self.keys_pressed.clear();
        self.keys_typed.clear();
        self.keys_released.clear();
        self.buttons_pressed.clear();
        self.buttons_released.clear();
        self.mouse_delta = (0.0, 0.0);
        self.raw_mouse_delta = (0.0, 0.0);
        self.scroll_delta = (0.0, 0.0);
        self.text.clear();
        self.events.clear();
    }

    // ---- keyboard --------------------------------------------------------

    /// Is this key held right now? (Level-triggered — true every frame the
    /// key stays down.)
    pub fn is_key_down(&self, key: KeyCode) -> bool {
        self.keys_down.contains(&key)
    }

    /// Did this key go down *this frame*? (Edge-triggered, excludes OS
    /// auto-repeat — one `true` per physical press.)
    pub fn is_key_pressed(&self, key: KeyCode) -> bool {
        self.keys_pressed.contains(&key)
    }

    /// [`Input::is_key_pressed`], but *including* OS auto-repeat — the
    /// "should this fire again?" question for held-key actions like
    /// backspace or arrow-key cursor movement, where the OS's own repeat
    /// rate is the behaviour users expect.
    pub fn is_key_typed(&self, key: KeyCode) -> bool {
        self.keys_typed.contains(&key)
    }

    /// Did this key come up this frame? (Edge-triggered.)
    pub fn is_key_released(&self, key: KeyCode) -> bool {
        self.keys_released.contains(&key)
    }

    /// Every key currently held, in arbitrary order.
    pub fn keys_down(&self) -> impl Iterator<Item = KeyCode> + '_ {
        self.keys_down.iter().copied()
    }

    /// The current modifier state (shift/ctrl/alt/super), as a bitflag set.
    pub fn modifiers(&self) -> ModifiersState {
        self.modifiers
    }

    pub fn shift(&self) -> bool {
        self.modifiers.shift_key()
    }

    pub fn ctrl(&self) -> bool {
        self.modifiers.control_key()
    }

    pub fn alt(&self) -> bool {
        self.modifiers.alt_key()
    }

    /// The Super/Command/Windows key.
    pub fn super_key(&self) -> bool {
        self.modifiers.super_key()
    }

    /// Did `key` go down this frame with *exactly* `modifiers` held? The
    /// exactness matters: `Ctrl+S` must not also fire on `Ctrl+Shift+S`,
    /// which is usually a different command.
    ///
    /// ```ignore
    /// if input.is_shortcut_pressed(ModifiersState::CONTROL, KeyCode::KeyS) { save(); }
    /// ```
    pub fn is_shortcut_pressed(&self, modifiers: ModifiersState, key: KeyCode) -> bool {
        self.modifiers == modifiers && self.is_key_pressed(key)
    }

    /// [`Input::is_shortcut_pressed`]'s level-triggered counterpart — is
    /// this exact chord held right now?
    pub fn is_shortcut_down(&self, modifiers: ModifiersState, key: KeyCode) -> bool {
        self.modifiers == modifiers && self.is_key_down(key)
    }

    /// Characters typed this frame, control characters stripped — what to
    /// insert into a document. Empty on frames with no typing.
    ///
    /// This is deliberately separate from the [`KeyCode`] queries: `KeyCode`
    /// is a physical position (right for keybinds), this is layout- and
    /// modifier-resolved text (right for typing). A French layout's
    /// `KeyCode::KeyQ` types "a"; only one of those two answers is ever the
    /// one wanted.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Everything that happened this frame, in delivery order. Prefer this
    /// over the aggregate queries when order matters (editing, key
    /// sequences); the aggregates stay for widgets that only ask "did X
    /// happen". Cleared by [`Input::end_frame`].
    pub fn events(&self) -> &[InputEvent] {
        &self.events
    }

    /// The IME composition in progress, if any. Render it at the caret,
    /// conventionally underlined; it is not in the document until an
    /// [`InputEvent::Text`] commit arrives.
    pub fn preedit(&self) -> Option<&Preedit> {
        self.preedit.as_ref()
    }

    /// Record where the caret is, in logical pixels, so the OS can place
    /// its candidate window beside it instead of over the text being
    /// composed. Takes effect at the next [`Input::sync_ime`].
    pub fn set_ime_cursor_area(&mut self, top_left: (f32, f32), size: (f32, f32)) {
        let area = Some((top_left, size));
        if self.ime_area != area {
            self.ime_area = area;
            self.ime_area_dirty = true;
        }
    }

    /// Enable IME on `window` (off by default in winit, so
    /// `WindowEvent::Ime` never fires otherwise) and forward the latest
    /// caret rectangle. Cheap to call every frame; it only talks to the OS
    /// when something changed.
    pub fn sync_ime(&mut self, window: &Window) {
        if !self.ime_enabled {
            window.set_ime_allowed(true);
            self.ime_enabled = true;
        }
        if self.ime_area_dirty
            && let Some((position, size)) = self.ime_area
        {
            window.set_ime_cursor_area(
                LogicalPosition::new(position.0, position.1),
                LogicalSize::new(size.0, size.1),
            );
            self.ime_area_dirty = false;
        }
    }

    // ---- mouse -----------------------------------------------------------

    pub fn is_mouse_down(&self, button: MouseButton) -> bool {
        self.buttons_down.contains(&button)
    }

    pub fn is_mouse_pressed(&self, button: MouseButton) -> bool {
        self.buttons_pressed.contains(&button)
    }

    pub fn is_mouse_released(&self, button: MouseButton) -> bool {
        self.buttons_released.contains(&button)
    }

    /// Every mouse button currently held, in arbitrary order.
    pub fn buttons_down(&self) -> impl Iterator<Item = MouseButton> + '_ {
        self.buttons_down.iter().copied()
    }

    /// Cursor position in **logical** pixels, in the same coordinate space
    /// the renderer's `draw_*` methods take, so a hit-test is a direct
    /// comparison against drawn geometry with no conversion.
    pub fn mouse_position(&self) -> (f32, f32) {
        self.mouse_position
    }

    /// How far the cursor moved this frame, in logical pixels. Clipped to
    /// the window and subject to OS pointer acceleration — the right
    /// measure for dragging UI. Zero on frames with no movement.
    pub fn mouse_delta(&self) -> (f32, f32) {
        self.mouse_delta
    }

    /// Raw device motion this frame, unaccelerated and unclipped, from
    /// `DeviceEvent::MouseMotion`. Non-zero only if
    /// [`Input::handle_device_event`] is wired up. Units are device-defined,
    /// not pixels.
    pub fn raw_mouse_delta(&self) -> (f32, f32) {
        self.raw_mouse_delta
    }

    /// Scroll accumulated this frame in lines — positive `y` is scroll-up /
    /// content-down, positive `x` is scroll-right, matching winit. Pixel
    /// deltas from touchpads are converted at [`PIXELS_PER_LINE`].
    pub fn scroll_delta(&self) -> (f32, f32) {
        self.scroll_delta
    }

    // ---- window ----------------------------------------------------------

    /// Does the window have keyboard focus? Held input is force-released
    /// when this goes false, so a chord can't survive an alt-tab.
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Is the cursor over the window? [`Input::mouse_position`] keeps its
    /// last known value while this is false, so check it before treating a
    /// position as a live hover.
    pub fn is_cursor_in_window(&self) -> bool {
        self.cursor_in_window
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::{
        dpi::PhysicalPosition,
        event::{DeviceId, TouchPhase},
    };

    /// `KeyEvent` can't be built outside winit (its `platform_specific`
    /// field is crate-private), so keyboard cases seed `keys_down` directly
    /// — the state these tests care about is what `release_all`/`end_frame`
    /// do to it, not the event decoding on the way in.
    fn input() -> Input {
        Input::new(1.0)
    }

    fn cursor(x: f64, y: f64) -> WindowEvent {
        WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(x, y),
        }
    }

    fn click(state: ElementState) -> WindowEvent {
        WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state,
            button: MouseButton::Left,
        }
    }

    #[test]
    fn button_edges_last_exactly_one_frame() {
        let mut input = input();
        input.handle_event(&click(ElementState::Pressed));
        assert!(input.is_mouse_down(MouseButton::Left));
        assert!(input.is_mouse_pressed(MouseButton::Left));

        input.end_frame();
        assert!(
            input.is_mouse_down(MouseButton::Left),
            "held state must survive the frame"
        );
        assert!(
            !input.is_mouse_pressed(MouseButton::Left),
            "press edge must not repeat"
        );

        input.handle_event(&click(ElementState::Released));
        assert!(!input.is_mouse_down(MouseButton::Left));
        assert!(input.is_mouse_released(MouseButton::Left));
    }

    #[test]
    fn focus_loss_releases_everything_held() {
        let mut input = input();
        input.keys_down.insert(KeyCode::ControlLeft);
        input.keys_down.insert(KeyCode::KeyS);
        input.modifiers = ModifiersState::CONTROL;
        input.handle_event(&click(ElementState::Pressed));
        input.end_frame();

        input.handle_event(&WindowEvent::Focused(false));

        assert!(!input.is_focused());
        assert_eq!(
            input.keys_down().count(),
            0,
            "keys must not stay stuck across focus loss"
        );
        assert!(!input.is_mouse_down(MouseButton::Left));
        assert!(input.modifiers().is_empty());
        // Reported as edges so press/release pairing unwinds instead of hanging.
        assert!(input.is_key_released(KeyCode::KeyS));
        assert!(input.is_mouse_released(MouseButton::Left));
    }

    #[test]
    fn mouse_position_is_logical_and_delta_is_per_frame() {
        let mut input = Input::new(2.0);
        input.handle_event(&cursor(200.0, 100.0));
        assert_eq!(
            input.mouse_position(),
            (100.0, 50.0),
            "physical px must be scaled down"
        );
        assert_eq!(
            input.mouse_delta(),
            (0.0, 0.0),
            "first move has no previous anchor"
        );

        input.end_frame();
        input.handle_event(&cursor(220.0, 100.0));
        assert_eq!(input.mouse_delta(), (10.0, 0.0));

        input.end_frame();
        assert_eq!(input.mouse_delta(), (0.0, 0.0));
    }

    #[test]
    fn leaving_the_window_drops_the_delta_anchor() {
        let mut input = input();
        input.handle_event(&cursor(10.0, 10.0));
        input.end_frame();

        input.handle_event(&WindowEvent::CursorLeft {
            device_id: DeviceId::dummy(),
        });
        input.handle_event(&cursor(900.0, 600.0));

        assert!(input.is_cursor_in_window());
        assert_eq!(
            input.mouse_delta(),
            (0.0, 0.0),
            "re-entry is a teleport, not a drag"
        );
    }

    #[test]
    fn scroll_accumulates_both_units_into_lines() {
        let mut input = input();
        let wheel = |delta| WindowEvent::MouseWheel {
            device_id: DeviceId::dummy(),
            delta,
            phase: TouchPhase::Moved,
        };
        input.handle_event(&wheel(MouseScrollDelta::LineDelta(0.0, 2.0)));
        input.handle_event(&wheel(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            0.0,
            f64::from(PIXELS_PER_LINE),
        ))));

        assert_eq!(input.scroll_delta(), (0.0, 3.0));
        input.end_frame();
        assert_eq!(input.scroll_delta(), (0.0, 0.0));
    }

    fn ime(event: Ime) -> WindowEvent {
        WindowEvent::Ime(event)
    }

    #[test]
    fn an_ime_commit_becomes_typed_text() {
        let mut input = input();
        input.handle_event(&ime(Ime::Commit("é".into())));
        assert_eq!(input.text(), "é");
        assert_eq!(input.events(), [InputEvent::Text("é".into())]);
        input.end_frame();
        assert_eq!(input.text(), "", "a commit is per-frame like any typing");
        assert!(input.events().is_empty());
    }

    #[test]
    fn a_long_commit_is_never_truncated_or_dropped() {
        let mut input = input();
        let long = "あ".repeat(400);
        input.handle_event(&ime(Ime::Commit(long.clone())));
        input.handle_event(&ime(Ime::Commit("tail".into())));
        assert_eq!(input.text(), format!("{long}tail"));
    }

    #[test]
    fn a_preedit_is_provisional_and_replaced_wholesale() {
        let mut input = input();
        input.handle_event(&ime(Ime::Preedit("ni".into(), Some((0, 2)))));
        assert_eq!(input.text(), "", "a preedit is not in the document");
        assert_eq!(
            input.preedit(),
            Some(&Preedit {
                text: "ni".into(),
                cursor: Some((0, 2)),
            })
        );

        input.handle_event(&ime(Ime::Preedit("nih".into(), Some((0, 3)))));
        assert_eq!(input.preedit().map(|p| p.text.as_str()), Some("nih"));

        input.end_frame();
        assert!(input.preedit().is_some(), "a composition spans frames");
    }

    #[test]
    fn a_commit_ends_the_composition_before_inserting() {
        let mut input = input();
        input.handle_event(&ime(Ime::Preedit("ni".into(), None)));
        input.handle_event(&ime(Ime::Commit("你".into())));
        assert_eq!(input.preedit(), None);
        assert_eq!(
            input.events(),
            [
                InputEvent::Preedit {
                    text: "ni".into(),
                    cursor: None,
                },
                InputEvent::Preedit {
                    text: String::new(),
                    cursor: None,
                },
                InputEvent::Text("你".into()),
            ]
        );
    }

    #[test]
    fn disabling_ime_cancels_the_composition() {
        let mut input = input();
        input.handle_event(&ime(Ime::Preedit("ni".into(), None)));
        input.handle_event(&ime(Ime::Disabled));
        assert_eq!(input.preedit(), None);
    }

    #[test]
    fn control_characters_are_not_text() {
        let mut input = input();
        input.commit_text("\u{1b}\u{8}");
        assert_eq!(input.text(), "");
        assert!(input.events().is_empty());
    }

    #[test]
    fn events_keep_delivery_order_across_kinds() {
        let mut input = input();
        input.handle_event(&cursor(10.0, 20.0));
        input.handle_event(&click(ElementState::Pressed));
        input.handle_event(&ime(Ime::Commit("a".into())));
        input.handle_event(&WindowEvent::Focused(false));

        let kinds: Vec<&str> = input
            .events()
            .iter()
            .map(|event| match event {
                InputEvent::MouseMoved { .. } => "moved",
                InputEvent::MouseButton { .. } => "button",
                InputEvent::Text(_) => "text",
                InputEvent::Focus(_) => "focus",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["moved", "button", "text", "focus"]);
    }

    #[test]
    fn windows_treats_ctrl_alt_as_altgr_text() {
        let both = ModifiersState::CONTROL.union(ModifiersState::ALT);
        assert!(!suppresses_text(both, Platform::Windows));
        assert!(suppresses_text(ModifiersState::CONTROL, Platform::Windows));
        assert!(suppresses_text(ModifiersState::ALT, Platform::Windows));
    }

    #[test]
    fn macos_option_types_but_control_and_command_do_not() {
        assert!(!suppresses_text(ModifiersState::ALT, Platform::MacOs));
        assert!(suppresses_text(ModifiersState::CONTROL, Platform::MacOs));
        assert!(suppresses_text(ModifiersState::SUPER, Platform::MacOs));
    }

    #[test]
    fn elsewhere_every_ctrl_alt_or_super_chord_is_a_shortcut() {
        let both = ModifiersState::CONTROL.union(ModifiersState::ALT);
        assert!(suppresses_text(both, Platform::Other));
        assert!(suppresses_text(ModifiersState::ALT, Platform::Other));
        assert!(suppresses_text(ModifiersState::SUPER, Platform::Other));
        assert!(!suppresses_text(ModifiersState::SHIFT, Platform::Other));
    }
}
