---
name: rune-input
description: Rune input behaviour as used by agentd - key routing, physical keys, IME, the Option-as-Alt constraint, pointer drag, wheel, clipboard. Use when changing input.rs, on_key, mouse reporting, selection gestures, scrolling, paste/copy or IME, instead of reading the Rune source.
---

# Rune input

Verified against Rune rev `ba5e7c8` and winit 0.30.13. If the pin has moved, check anything you rely on.

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

## References

| Task | Reference |
|---|---|
| `Key`, `Mods`, `Command(c)`, Ctrl and dead-key quirks, `on_physical_input` | [references/keys.md](references/keys.md) |
| IME preedit and commit, `.ime_cursor()`, why Option-as-Alt is limited | [references/ime-and-option-as-alt.md](references/ime-and-option-as-alt.md) |
| `on_drag`, pointer move, hover, buttons, `on_wheel` | [references/pointer-and-wheel.md](references/pointer-and-wheel.md) |
| Copy, read, paste, ⌘C/⌘V routing | [references/clipboard.md](references/clipboard.md) |

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
