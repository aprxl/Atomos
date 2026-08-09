# TODO

Deferred work with a known trigger. Each entry says *when* to do it, not
just what — an item with no trigger is a wish, not a task.

## IME support (input)

**Trigger:** land it alongside the rope + caret, not after.

Without it, users of Chinese/Japanese/Korean input methods cannot type at
all — the IME intercepts keystrokes upstream of us, so `KeyEvent.text` (what
`Input::text` reads) comes back empty. Also covers Compose-key sequences and
dead keys.

What it takes:

- `window.set_ime_allowed(true)` — off by default in winit, so
  `WindowEvent::Ime` never fires today.
- Handle `Ime::{Enabled, Preedit(text, cursor_range), Commit(text), Disabled}`
  in `src/input.rs`. Preedit is provisional text: it replaces wholesale on
  every keystroke and is **not** in the document until `Commit`.
- Editor side: render preedit distinctly (conventionally underlined, at the
  caret), and route `Commit` through the normal text-insertion path.
- `window.set_ime_cursor_area(..)` so the OS candidate popup doesn't cover
  the text being composed.

The input-side change is small (~15 lines); the cost is in the editor, which
is why it waits for a buffer and caret to attach to. Doing it *with* the
text-insertion path is cheap; retrofitting a path built on
"one keystroke = one character" is not.

**Testing caveat:** April overrides compose keys with a separate
Atomos-based system, so local verification needs a real CJK input method
(e.g. `ibus` + `ibus-anthy`) rather than the usual Compose-key smoke test.
