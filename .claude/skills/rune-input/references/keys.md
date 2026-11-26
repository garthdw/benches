# Key events and physical input

## Key events

`KeyEvent { key: Key, mods: Mods }`.

- `Key`: `Text(String)`, `Enter, Backspace, Delete, Left, Right, Up, Down, Home, End, PageUp, PageDown, Insert, PrintScreen, ScrollLock, Pause, Escape, Tab, Function(u8), ContextMenu, Modifier{..}` (a modifier pressed or released on its own, only sent if the app opts in with `App::modifier_keys`), and `Command(char)`. `Key` is `#[non_exhaustive]`, so a `match` needs a `_` arm.
- `Command(c)` is any character typed with ⌘ (letter, digit, punctuation), lowercased. With Shift the character follows the layout (⌘⇧7 is `Command('&')` on US, `Command('/')` on German), so match shifted shortcuts on `tx.physical_key()`. Exception: ⌘Space arrives as `Text(" ")` with `mods.cmd`.
- `Text` carries what the layout typed (winit's `text_with_all_modifiers`) unless that is empty or a control character, or the key is a chord (on macOS ⌘ or Ctrl held; elsewhere ⌘, Ctrl or Alt, but not Ctrl with Alt, which is AltGr), so Space is `Text(" ")` on most layouts but Shift+Space on a Persian layout is `Text("\u{200C}")`. With `.ime_option_as_alt(true)` winit hands Rune the characters without Option, so ⌥Space is `Text(" ")` with `mods.alt`.
- Off macOS, Alt+letter without Ctrl is a chord: it never types into a focused field and reaches `on_key`.
- Off macOS, Shift+Insert is a paste: with nothing focused it still reaches `on_key` as `Key::Insert` with `mods.shift` first, and only if that declines does Rune paste to a root `on_paste`. A focused field pastes instead.
- `Mods { shift, cmd, alt, ctrl, logo }`. On macOS `cmd` and `logo` are both ⌘.
- During a key handler: `tx.physical_key() -> Option<PhysicalKeyCode>`, `tx.key_base() -> Option<char>` (the character under the current layout with no modifiers), `tx.key_repeat()`, `tx.mods()`, `tx.input_time()`. `El::on_key_up` hears releases (logical key; a root handler gets unclaimed ones); an `on_physical_input` that takes a release hides it from `on_key_up`.
- With Ctrl held, winit on macOS reports the logical key as the shift-only character (`create_key_event`), so ⌃2 arrives as `Key::Text("2")` with `mods.ctrl`, not `Command`. ⌃⌘ combos are `Command`.
- A dead key that types nothing (`Key::Dead` with no text) maps to **no logical event at all**, so `on_key` never runs for it. A dead key that carries text (a second dead key on Windows, `'` then `'` typing `''`) arrives as `Text` unless a chord modifier is held. While `.ime_cursor()` has the input method, macOS composes dead keys as IME preedit and commit instead.

## Physical input

`El::on_physical_input(|s, &PhysicalInput, tx| -> bool)`, with variants `Key { key, mods, pressed, repeat }`, `Modifiers(Mods)`, `FocusLost`.

**It is only offered to the focused element's handler** (`offer_physical` uses `focus.path()`). On a root element with nothing focused it never fires. `tx.key_base` isn't set for this handler.

## Held modifiers

`App::on_modifiers(|s, Mods, tx|)` hears the held modifiers each time they change, wherever focus is and whether or not anything takes the key, and none held once the window loses focus. It runs after the key's own routing in the same dispatch and can't take the key. Call `tx.live_only()` if the view reads nothing that changed. Every host, the native window included, gets it through `RuntimeConfig` (`app/config.rs`), so a modifier-held reveal needs neither `App::modifier_keys` nor `Key::Modifier`.
