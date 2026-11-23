# IME and Option as Alt

## IME

- `.ime_cursor()` on the element you draw as the caret: while drawn and no text field has focus, the window (when focused) enables its input method and places the candidate window at that element's rect.
- `App::on_ime` gets `Ime::Preedit(String, Option<(usize, usize)>, Vec<Clause>)` (empty ends it; match with `..`), `Ime::Commit(String)`, `Ime::Select(Range<usize>)` (reconversion of committed text; a terminal can ignore it), `Ime::Enabled`, `Ime::Disabled`. Return true if taken. Native windows only.
- Empty `Preedit` and `Commit` outside a composition still reach `on_ime` (only a focused field skips them; Wayland sends empty preedits in reply to cursor-area updates), so ignore an empty `Commit`.
- Plain typing still arrives once, as `Key::Text`. `Commit` only carries what the IME composed, plus, on macOS, text an input method inserts for a key without marked text when it differs from the key's own characters (full-width punctuation such as `，` for `,` from a Chinese input method): that key arrives as `Ime::Commit` and never as a key event. Keys with ⌘ or Ctrl, and Option under `.ime_option_as_alt(true)`, are never taken this way (`platform/ime.rs`, `should_direct_commit`).
- `Ime::Disabled` mid-composition (the caret's input method turned off, or the input source switched) first sends `on_ime` an empty `Preedit`, and Return, Escape and key releases reach `on_key` again.
- Toggling `.ime_cursor()` off cancels a composition, so keep it stable. Programs hide the cursor while redrawing, so remember the last caret position rather than dropping the element.
- When the window goes inactive mid-composition, `on_ime` hears `Ime::Commit` with the last preedit (`Runtime::finish_composition`), as a browser keeps it; the composition is not discarded.

## Option as Alt

`.ime_option_as_alt(true)` beside `.ime_cursor()` sets winit's `OptionAsAlt::Both` while that caret has the input method (off again when a field takes it). Option and a key, dead keys such as ⌥E and ⌥U included, then arrive as that key with `mods.alt` and `tx.key_base()` its character, instead of the composed character or marked text. Benches always sets it on the terminal caret, so keep `.ime_cursor()` stable rather than toggling it on Option.
