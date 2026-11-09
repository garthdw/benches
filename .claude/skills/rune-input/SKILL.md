---
name: rune-input
description: Rune input behaviour as used by agentd - key routing, physical keys, IME, the Option-as-Alt constraint, pointer drag, wheel, clipboard. Use when changing input.rs, on_key, mouse reporting, selection gestures, scrolling, paste/copy or IME, instead of reading the Rune source.
---

# Rune input

Verified against Rune rev `268ac7c` and winit 0.30.13. If the pin has moved, check anything you rely on.

## Key routing (first taker wins)

Global shortcuts and native menu key equivalents are handled before the window sees the key.

1. Return and Escape during an IME composition go to the composition.
2. Unmodified Return presses the window's `default_button()`.
3. **Nothing focused: `AppSpec::on_key`**, with `tx.focused() == None`. agentd lives here.
4. Focused field's `on_input_key`.
5. Focus navigation (Escape, Tab, arrows and so on).
6. Focused text field's editor.
7. `on_key` again, with `tx.focused()` set.
8. Selected text's Cmd+A and Cmd+C.
9. Page Up, Page Down, Home and End scroll the nearest scroll view.

Making the terminal `.focusable(true)` moves it out of step 3 and exposes Tab and Escape to step 5. Don't do that casually: M1's exit criterion is that clicks don't leave Rune focus holding keys.

## Key events

`KeyEvent { key: Key, mods: Mods }`.

- `Key`: `Text(String)` (Space is `Text(" ")`), `Enter, Backspace, Delete, Left, Right, Up, Down, Home, End, PageUp, PageDown, Insert, PrintScreen, ScrollLock, Pause, Escape, Tab, Function(u8), ContextMenu, Modifier{..}`, and `Command(char)`.
- `Command(c)` is any character typed with ⌘, lowercased. Exception: ⌘Space arrives as `Text(" ")` with `mods.cmd`.
- `Mods { shift, cmd, alt, ctrl, logo }`. On macOS `cmd` and `logo` are both ⌘.
- During a key handler: `tx.physical_key() -> Option<PhysicalKeyCode>`, `tx.key_base() -> Option<char>` (the character under the current layout with no modifiers), `tx.key_repeat()`, `tx.mods()`, `tx.input_time()`.
- `Key::Dead` from winit maps to **no logical event at all**, so `on_key` never runs for it.

## Physical input

`El::on_physical_input(|s, &PhysicalInput, tx| -> bool)`, with variants `Key { key, mods, pressed, repeat }`, `Modifiers(Mods)`, `FocusLost`.

**It is only offered to the focused element's handler** (`offer_physical` uses `focus.path()`). On a root element with nothing focused it never fires. `on_modifiers` has no such limit. `tx.key_base` isn't set for this handler.

## IME

- `.ime_cursor()` on the element you draw as the caret: while drawn and no text field has focus, the window enables its input method and places the candidate window at that element's rect.
- `App::on_ime` gets `Ime::Preedit(String, Option<(usize, usize)>)` (empty ends it), `Ime::Commit(String)`, `Ime::Enabled`, `Ime::Disabled`. Return true if taken. Native windows only.
- Plain typing still arrives once, as `Key::Text`. `Commit` only carries what the IME composed.
- Toggling `.ime_cursor()` off cancels a composition, so keep it stable. Programs hide the cursor while redrawing, so remember the last caret position rather than dropping the element.

## Option as Alt: the constraint

Rune has no `option_as_alt` setting, and `WindowHandle` doesn't expose the winit `Window` that would set it.

In winit's `keyDown`, when `ime_allowed` is true, `interpretKeyEvents` runs first. ⌥E, ⌥U, ⌥I, ⌥N and ⌥\` on a US layout become marked text and no `KeyboardInput` is queued, so neither `on_key` nor physical input sees them.

What follows:
- Remove `.ime_cursor()` while Option is held (track with `App::on_modifiers`). winit then skips `interpretKeyEvents`.
- Non-dead combos then arrive as `Key::Text("∫")` with `mods.alt`, and `tx.key_base() == Some('b')`.
- Dead-key combos then arrive with **no logical event**. Physical input is the only signal, and it needs focus (see above), so they do nothing for now. The fix is deferred: expose `option_as_alt` through Rune (see "Future changes" in `docs/ROADMAP.md`). Don't make the terminal focusable to work around it.

## Pointer

- `El::on_drag(|s: &mut S, d: &mut Drag, tx|)`. `Drag`: `phase` (`Phase::{Down, Move, Up}`), `x, y, w, h, start_x, start_y` in element-local points, `dx, dy` in window points, `data` scratch, `clicks` (1, 2, 3...), `cancelled()` on `Up`.
- Primary only, unless `.pointer_buttons([PointerButton::Middle, ..])`. `tx.button()` says which. Secondary always goes to `on_secondary_press[_local]`, which is press only with no release event.
- `.on_pointer_move_local(|s, PointerEvent { x, y, window_x, window_y, .. }, tx|)`. Call `tx.live_only()` if the view reads nothing that changed.
- `tx.mods()` works in all of these. Multiply points by the scale for device pixels.

## Wheel

`El::on_wheel(|s, WheelEvent, tx| -> bool)`, return true to take it. Once a handler takes a gesture's first delta it gets the rest, momentum included.

`WheelEvent { dx, dy, x, y, mods, phase, momentum, precise }`. Deltas are points, and positive `dy` moves content down (scrolling toward older lines). Element-local `x, y`.

## Clipboard

- `tx.copy(text)`, `tx.copy_rich(ClipboardContent)`.
- `tx.read_clipboard(|s: &mut S, content: &ClipboardContent, tx| ...)` reads off-thread; `content.text()`. It doesn't paste anywhere by itself.
- `tx.request_paste()` routes to the focused field or a root `on_paste`.
- ⌘C and ⌘V arrive in `on_key` as `Key::Command('c' | 'v')` when nothing is focused. Whether a native Edit menu's key equivalent intercepts them first needs a live check: Rune's `native_menu::take_unhandled` hands such keys back.

## File map (under `crates/rune/`)

| Topic | Path |
|---|---|
| Key routing docs | `README.md`, "Key routing" |
| `Key`, `KeyEvent`, `Mods` | `src/edit/keys.rs` |
| winit to `KeyEvent` | `src/window/keys.rs`, `src/window/events.rs` |
| Physical dispatch | `src/runtime/input/physical.rs`, `src/input/physical_input.rs` |
| App IME | `src/runtime/input/app_ime.rs`, `src/edit/ime.rs` |
| Wheel, buttons | `src/input/canvas_input.rs`, `src/runtime/input/gesture.rs` |
| `Drag`, `PointerEvent` | `src/el.rs` |
| Clipboard | `src/clipboard/tx.rs`, `src/clipboard/content.rs` |
| Worked example | `examples/canvas_input.rs` |
