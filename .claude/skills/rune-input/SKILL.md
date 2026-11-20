---
name: rune-input
description: Rune input behaviour as used by Benches - key routing, physical keys, IME, Option as Alt, pointer drag, wheel, clipboard. Use when changing input.rs, on_key, mouse reporting, selection gestures, scrolling, paste/copy or IME, instead of reading the Rune source.
---

# Rune input

Verified against Rune rev `e6deb86` and winit 0.30.13 (Rune's vendored copy, which Benches patches in). If the pin has moved, check anything you rely on.

## Key routing (first taker wins)

Global shortcuts and native menu key equivalents are handled before the window sees the key.

1. Return and Escape during an IME composition go to the composition.
2. Unmodified Return presses the window's `default_button()`.
3. **Nothing focused: `AppSpec::on_key`**, with `tx.focused() == None`. Benches lives here.
4. Focused field's `on_input_key`.
5. Focus navigation (Escape, Tab, arrows and so on).
6. Focused text field's editor.
7. `on_key` again, with `tx.focused()` set.
8. Selected text's Cmd+A and Cmd+C.
9. Page Up, Page Down, Home and End scroll the nearest scroll view. A scroll view that itself has focus also scrolls on Up and Down (40 points) and Space or Shift+Space (a page).

Making the terminal `.focusable(true)` moves it out of step 3 and exposes Tab and Escape to step 5. Don't do that casually: clicks must not leave Rune focus holding keys.

## References

| Task | Reference |
|---|---|
| `Key`, `Mods`, `Command(c)`, Ctrl and dead-key quirks, `on_physical_input`, `on_modifiers` | [references/keys.md](references/keys.md) |
| IME preedit and commit, `.ime_cursor()`, Option as Alt | [references/ime-and-option-as-alt.md](references/ime-and-option-as-alt.md) |
| `on_drag`, `press_on_down`, pointer move, hover, buttons, `on_wheel` and wheel chaining | [references/pointer-and-wheel.md](references/pointer-and-wheel.md) |
| Copy, read, paste, ⌘C/⌘V routing | [references/clipboard.md](references/clipboard.md) |

## File map (under `crates/` in the Rune checkout)

| Topic | Path |
|---|---|
| Key routing docs | `rune/README.md`, "Key routing" |
| `Key`, `KeyEvent`, `Mods` | `rune-edit/src/keys.rs` (re-exported as `rune::edit`) |
| winit to `KeyEvent` | `rune/src/window/keys.rs`, `rune/src/window/host/events.rs`, `rune/src/input/host_events.rs` |
| Physical dispatch, `on_modifiers` | `rune/src/runtime/input/physical.rs`, `rune/src/input/physical_input.rs` |
| App IME | `rune/src/runtime/input/app_ime.rs`, `rune-edit/src/ime.rs` |
| Wheel, buttons | `rune/src/input/canvas_input.rs`, `rune/src/runtime/input/gesture.rs`, `rune/src/runtime/scroll/x.rs` |
| `Drag`, `PointerEvent`, `press_on_down` | `rune/src/el/handlers.rs` |
| Clipboard | `rune/src/clipboard/tx.rs`, `rune/src/clipboard/content.rs` |
| Worked example | `rune/examples/canvas_input.rs` |
