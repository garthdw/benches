# IME and Option as Alt

## IME

- `.ime_cursor()` on the element you draw as the caret: while drawn and no text field has focus, the window (when focused) enables its input method and places the candidate window at that element's rect.
- `App::on_ime` gets `Ime::Preedit(String, Option<(usize, usize)>)` (empty ends it), `Ime::Commit(String)`, `Ime::Enabled`, `Ime::Disabled`. Return true if taken. Native windows only.
- Plain typing still arrives once, as `Key::Text`. `Commit` only carries what the IME composed.
- Toggling `.ime_cursor()` off cancels a composition, so keep it stable. Programs hide the cursor while redrawing, so remember the last caret position rather than dropping the element.
- When the window goes inactive mid-composition, `on_ime` hears `Ime::Commit` with the last preedit (`Runtime::finish_composition`), as a browser keeps it; the composition is not discarded.

## Option as Alt: the constraint

Still true at `7dfaa8f`: Rune has no `option_as_alt` setting (no use of it or of winit's `WindowExtMacOS`), and `WindowHandle` doesn't expose the winit `Window` that would set it (`window()` is `pub(super)`; only `with_raw_handle` is public).

In winit's `keyDown`, when `ime_allowed` is true, `interpretKeyEvents` runs first. ⌥E, ⌥U, ⌥I, ⌥N and ⌥\` on a US layout become marked text and no `KeyboardInput` is queued, so neither `on_key` nor physical input sees them.

What follows:
- Remove `.ime_cursor()` while Option is held (track with `App::on_modifiers`). winit then skips `interpretKeyEvents`.
- Non-dead combos then arrive as `Key::Text("∫")` with `mods.alt`, and `tx.key_base() == Some('b')`.
- Dead-key combos then arrive with **no logical event**. Physical input is the only signal, and it needs focus (see [keys.md](keys.md)), so they do nothing for now. The fix is deferred: expose `option_as_alt` through Rune (see "Future changes" in `docs/ROADMAP.md`). Don't make the terminal focusable to work around it.
