# Entry point and hooks

```rust
let spec = AppSpec::<State> {
    scenes: &[("fixture", |s| /* mutate initial state */)],
    setup: Some(|s, waker| s.waker = Some(waker)),
    poll: Some(poll),
    on_key: Some(on_key),
    ..AppSpec::new("Benches", (960.0, 640.0), State::default, view)
};
App::new(spec).compositor(render::TerminalCompositor::default()).run();
```

`AppSpec` fields: `title, size, min_size, init, view, scenes, fonts, families, setup, poll, on_key`. Always finish a literal with `..AppSpec::new(..)`. `App::init(impl FnOnce() -> S)` replaces the spec's `fn` init with a closure, as `window_app` does to carry the fleet and window id.

Other hooks are builder methods on `App`, not spec fields. `RuntimeConfig` copies `on_focus`, `on_modifiers`, `on_visibility` and `on_ime` into every host's runtime (native window, overlay, headless, web), so each hook that applies there fires. All are plain `fn` unless noted:

| Hook | Signature | Notes |
|---|---|---|
| `on_focus` | `fn(&mut S, bool, &mut Tx)` | Native window focus; also fires while hidden |
| `on_modifiers` | `fn(&mut S, Mods, &mut Tx)` | Every change to Shift, Control, Option or ⌘ (`cmd` and `logo` on macOS), regardless of focus. Cannot consume. None held after blur |
| `on_visibility` | `fn(&mut S, bool, &mut Tx)` | Window shown or hidden (minimised, covered whole), once per change after the first frame. The same change also wakes `poll` once; `tx.window_visible()` reads it there |
| `on_rest` | `fn(&mut S, &mut Tx) -> bool` | The window drew nothing for 10 s (1 s under memory pressure), or was hidden. For dropping the app's caches of what isn't on screen; return whether view state changed (normally false) |
| `on_ime` | `fn(&mut S, &Ime, &mut Tx) -> bool` | See `rune-input` |
| `on_close`, `on_quit` | `fn(&mut S, &mut Tx)` | `on_close` runs for the close button, `Tx::close_window`, `AppHandle::close` and macOS Quit; its `tx.quit()` quits. `Tx::quit` runs every window's `on_quit` and no `on_close` |
| `on_reopen` | `fn(&mut S, bool, &mut Tx) -> bool` | |
| `on_open_url` | `fn(&mut S, &str, &mut Tx)` | |
| `on_window`, `on_ready` | `impl FnMut(&WindowHandle)` | `WindowHandle::with_raw_handle` gives the raw handle only, no winit `Window`. `WindowHandle::scale()` is the scale factor without a placement read. Headless runs skip `on_ready` unless `--realtime` |
| `on_frame_encoded`, `on_frame_drawn` | `impl FnMut` | |
| `layout_seed(seed, keep)` | `Option<LayoutSeed>`, `impl FnMut(LayoutSeed)` | For a view that reads `Cx::layout`/`layout_size`: `keep` gets the first view's rects when the window rests, closes or quits; passed back next launch, the first view builds once. Ignored for another size or scale. Also on `WindowSpec`. Benches' view reads no rects |
| `menu_item(MenuItem)` | | See below |
| `macos_window(MacWindowOptions)`, `window(WindowSpec)`, `overlay(..)`, `compositor(..)`, `standard_menus()` | | |

Do slow start-up work from `on_ready`, after the first frame.

On macOS a regular app's first window is made and drawn in `applicationWillFinishLaunching:`, before AppKit finishes launching, so `init`, `setup` and the first view run that early (`RUNE_WINDOW_AFTER_LAUNCH=1` restores the old order). Text input (IME) starts after the first frame.

## Native menus (macOS)

`MenuItem::new(title, fn(&mut S, &mut Tx))` with `.id`, `.menu(title)` (default: the app menu after About), `.dock()`, `.after(anchor)` (an item id or an AppKit action such as `"paste:"`), `.key_equivalent(Shortcut)`, `.keys_first()` (the window's key handling sees the chord first; the item runs only if nothing takes it), `.enabled`, `.checked`, `.alternate()` (shown in place of the previous item while the extra modifier in its key equivalent is held) and `MenuItem::separator()`. `AppHandle::set_menu` replaces the custom items. Linux and Windows have no native menu.

A regular app whose menu bar is still winit's default gets `standard_menus()`'s items without asking: Edit and Window menus, with no Quit item. Edit ends with AutoFill, Start Dictation and Emoji & Symbols (⌃⌘Space); Window has Zoom, Bring All to Front and AppKit's window list. About, Hide and Quit are titled with `CFBundleDisplayName`, else `CFBundleName`, else the process name.
