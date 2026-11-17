# IME and Option as Alt

## IME

- `.ime_cursor()` on the element you draw as the caret: while drawn and no text field has focus, the window (when focused) enables its input method and places the candidate window at that element's rect.
- `App::on_ime` gets `Ime::Preedit(String, Option<(usize, usize)>, Vec<Clause>)` (empty ends it; match with `..`), `Ime::Commit(String)`, `Ime::Enabled`, `Ime::Disabled`. Return true if taken. Native windows only.
- Plain typing still arrives once, as `Key::Text`. `Commit` only carries what the IME composed.
- Toggling `.ime_cursor()` off cancels a composition, so keep it stable. Programs hide the cursor while redrawing, so remember the last caret position rather than dropping the element.
- When the window goes inactive mid-composition, `on_ime` hears `Ime::Commit` with the last preedit (`Runtime::finish_composition`), as a browser keeps it; the composition is not discarded.

## Option as Alt

`.ime_option_as_alt(true)` beside `.ime_cursor()` sets winit's `OptionAsAlt::Both` while that caret has the input method (off again when a field takes it). Option and a key, dead keys such as ⌥E and ⌥U included, then arrive as that key with `mods.alt` and `tx.key_base()` its character, instead of the composed character or marked text. Benches always sets it on the terminal caret, so keep `.ime_cursor()` stable rather than toggling it on Option.
