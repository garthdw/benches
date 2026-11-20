# Key events and physical input

## Key events

`KeyEvent { key: Key, mods: Mods }`.

- `Key`: `Text(String)` (Space is `Text(" ")`), `Enter, Backspace, Delete, Left, Right, Up, Down, Home, End, PageUp, PageDown, Insert, PrintScreen, ScrollLock, Pause, Escape, Tab, Function(u8), ContextMenu, Modifier{..}` (a modifier pressed or released on its own, only sent if the app opts in with `App::modifier_keys`), and `Command(char)`. `Key` is `#[non_exhaustive]`, so a `match` needs a `_` arm.
- `Command(c)` is any character typed with ⌘ (letter, digit, punctuation), lowercased. With Shift the character follows the layout (⌘⇧7 is `Command('&')` on US, `Command('/')` on German), so match shifted shortcuts on `tx.physical_key()`. Exception: ⌘Space arrives as `Text(" ")` with `mods.cmd`.
- `Mods { shift, cmd, alt, ctrl, logo }`. On macOS `cmd` and `logo` are both ⌘.
- During a key handler: `tx.physical_key() -> Option<PhysicalKeyCode>`, `tx.key_base() -> Option<char>` (the character under the current layout with no modifiers), `tx.key_repeat()`, `tx.mods()`, `tx.input_time()`. `El::on_key_up` hears releases (logical key; a root handler gets unclaimed ones); an `on_physical_input` that takes a release hides it from `on_key_up`.
- With Ctrl held, winit on macOS reports the logical key as the shift-only character (`create_key_event`), so ⌃2 arrives as `Key::Text("2")` with `mods.ctrl`, not `Command`. ⌃⌘ combos are `Command`.
- `Key::Dead` from winit maps to **no logical event at all**, so `on_key` never runs for it.

## Physical input

`El::on_physical_input(|s, &PhysicalInput, tx| -> bool)`, with variants `Key { key, mods, pressed, repeat }`, `Modifiers(Mods)`, `FocusLost`.

**It is only offered to the focused element's handler** (`offer_physical` uses `focus.path()`). On a root element with nothing focused it never fires. `tx.key_base` isn't set for this handler.

## Held modifiers

`App::on_modifiers(|s, Mods, tx|)` hears the held modifiers each time they change, wherever focus is and whether or not anything takes the key, and none held once the window loses focus. It runs after the key's own routing in the same dispatch and can't take the key. Call `tx.live_only()` if the view reads nothing that changed. Every host, the native window included, gets it through `RuntimeConfig` (`app/config.rs`), so a modifier-held reveal needs neither `App::modifier_keys` nor `Key::Modifier`.
