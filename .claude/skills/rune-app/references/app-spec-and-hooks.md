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

`AppSpec` fields: `title, size, min_size, init, view, scenes, fonts, families, setup, poll, on_key`. Always finish a literal with `..AppSpec::new(..)`.

Other hooks are builder methods on `App`, not spec fields. `RuntimeConfig` copies `on_focus`, `on_modifiers`, `on_visibility` and `on_ime` into every host's runtime (native window, overlay, headless, web), so each hook that applies there fires. All are plain `fn` unless noted:

| Hook | Signature | Notes |
|---|---|---|
| `on_focus` | `fn(&mut S, bool, &mut Tx)` | Native window focus; also fires while hidden |
| `on_modifiers` | `fn(&mut S, Mods, &mut Tx)` | Every change to Shift, Control, Option or ⌘ (`cmd` and `logo` on macOS), regardless of focus. Cannot consume. None held after blur |
| `on_visibility` | `fn(&mut S, bool, &mut Tx)` | Window shown or hidden (minimised, covered whole), once per change after the first frame. Native only |
| `on_ime` | `fn(&mut S, &Ime, &mut Tx) -> bool` | See `rune-input` |
| `on_close`, `on_quit` | `fn(&mut S, &mut Tx)` | |
| `on_reopen` | `fn(&mut S, bool, &mut Tx) -> bool` | |
| `on_open_url` | `fn(&mut S, &str, &mut Tx)` | |
| `on_window`, `on_ready` | `impl FnMut(&WindowHandle)` | `WindowHandle::with_raw_handle` gives the raw handle only, no winit `Window` |
| `on_frame_encoded`, `on_frame_drawn` | `impl FnMut` | |
| `macos_window(MacWindowOptions)`, `window(WindowSpec)`, `overlay(..)`, `compositor(..)` | | |

Do slow start-up work from `on_ready`, after the first frame.
