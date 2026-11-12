# Clipboard

- `tx.copy(text)`, `tx.copy_rich(ClipboardContent)`.
- `tx.read_clipboard(|s: &mut S, content: &ClipboardContent, tx| ...)` reads off-thread; `content.text()`. It doesn't paste anywhere by itself.
- `tx.request_paste()` routes to the focused field or a root `on_paste`.
- ⌘C and ⌘V arrive in `on_key` as `Key::Command('c' | 'v')` when nothing is focused. Whether a native Edit menu's key equivalent intercepts them first needs a live check: Rune's `native_menu::take_unhandled` (`native_menu/standard_macos.rs`) hands such keys back, only for `Command(c)` without alt or ctrl.
