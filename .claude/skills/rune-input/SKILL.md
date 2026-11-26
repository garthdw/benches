---
name: rune-input
description: Rune input behaviour as used by Benches - key routing, physical keys, IME, Option as Alt, pointer drag, wheel, clipboard. Use when changing input.rs, on_key, mouse reporting, selection gestures, scrolling, paste/copy or IME, instead of reading the Rune source.
---

# Rune input

Verified against Rune rev `4918969` and winit 0.30.13 (Rune's vendored copy, which Benches patches in). If the pin has moved, check anything you rely on.

## Key routing (first taker wins)

Global shortcuts and native menu key equivalents are handled before the window sees the key.

1. Return and Escape while a field or `App::on_ime` composes go to the composition.
2. `El::on_key_preview` on the focused element and its ancestors, outermost first.
3. `El::on_key` and `El::on_key_any` on the focused element and its ancestors, innermost first (not while a text field has focus).
4. Unmodified Return presses the window's (or open modal's) `default_button()`, unless the focused element takes Return.
5. **Nothing focused: `AppSpec::on_key`**, with `tx.focused() == None`. Benches lives here.
6. Focused field's `on_input_key`.
7. Focus navigation (Escape, Tab, arrows and so on). Space presses the focused control; Return presses it only if it opts in with `press_on_return()`.
8. Focused text field's editor.
9. `on_key` again, with `tx.focused()` set.
10. Selected text's Cmd+A and Cmd+C.
11. Page Up, Page Down, Home and End scroll the nearest scroll view. A scroll view that itself has focus also scrolls on Up and Down (40 points) and Space or Shift+Space (a page).

Making the terminal `.focusable(true)` moves it out of step 5 and exposes Tab and Escape to step 7. Don't do that casually: clicks must not leave Rune focus holding keys.

## References

| Task | Reference |
|---|---|
| `Key`, `Mods`, `Command(c)`, typed text, Ctrl and dead-key quirks, `on_physical_input`, `on_modifiers` | [references/keys.md](references/keys.md) |
| IME preedit, commit and direct commits, `.ime_cursor()`, Option as Alt | [references/ime-and-option-as-alt.md](references/ime-and-option-as-alt.md) |
| `on_drag`, `press_on_down`, pointer move, hover, cursor, buttons, `on_wheel`, wheel chaining, Ctrl+wheel | [references/pointer-and-wheel.md](references/pointer-and-wheel.md) |
| Copy, read, paste, ⌘C/⌘V routing | [references/clipboard.md](references/clipboard.md) |

## File map (under `crates/` in the Rune checkout)

| Topic | Path |
|---|---|
| Key routing | `rune/README.md`, "Key routing"; `rune/src/runtime/input/keys.rs` (`route_key`) |
| `Key`, `KeyEvent`, `Mods` | `rune-edit/src/keys.rs` (re-exported as `rune::edit`) |
| winit to `KeyEvent` | `rune/src/window/keys.rs`, `rune/src/window/host/events.rs`, `rune/src/input/host_events.rs` |
| Physical dispatch, `on_modifiers` | `rune/src/runtime/input/physical.rs`, `rune/src/input/physical_input.rs` |
| App IME | `rune/src/runtime/input/app_ime.rs`, `rune-edit/src/ime.rs`, `rune/src/input/host_events.rs` (`sync_ime`) |
| macOS direct commits | `rune/src/platform/ime.rs`, `rune/src/platform/ime/macos.rs` |
| Wheel, Ctrl+wheel, buttons | `rune/src/input/canvas_input.rs`, `rune/src/input/host_events.rs` (`wheel_core`), `rune/src/runtime/input/gesture.rs`, `rune/src/runtime/scroll/x.rs` |
| Pointer moves, hover, held drags, cursor | `rune/src/runtime/input/pointer.rs`, `rune/src/runtime/input/hover.rs`, `rune/src/runtime/input/hit.rs` |
| `Drag`, `PointerEvent`, `press_on_down` | `rune/src/el/handlers.rs` |
| Clipboard | `rune/src/clipboard/tx.rs`, `rune/src/clipboard/content.rs` |
| Worked example | `rune/examples/canvas_input.rs` |
