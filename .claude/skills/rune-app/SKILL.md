---
name: rune-app
description: Rune app structure as used by agentd - AppSpec, App hooks, the view/poll loop, Waker, Tx, timers, layout builders. Use when changing main.rs, State, the view, poll, or wiring background threads into the UI, instead of reading the Rune source.
---

# Rune app structure

Verified against Rune rev `ba5e7c8` (the pin in `Cargo.toml`). If the pin has moved, check anything you rely on.
Source, when this isn't enough: `~/.cargo/git/checkouts/rune-c9b805e9ed8a3730/ba5e7c8/crates/rune`. Use the file map at the end.

## Entry point

```rust
let spec = AppSpec::<State> {
    scenes: &[("fixture", |s| /* mutate initial state */)],
    setup: Some(|s, waker| s.waker = Some(waker)),
    poll: Some(poll),
    on_key: Some(on_key),
    ..AppSpec::new("agentd", (960.0, 640.0), State::default, view)
};
App::new(spec).compositor(render::TerminalCompositor::default()).run();
```

`AppSpec` fields: `title, size, min_size, init, view, scenes, fonts, families, setup, poll, on_key`. Always finish a literal with `..AppSpec::new(..)`.

Other hooks are builder methods on `App`, not spec fields. All are plain `fn` unless noted:

| Hook | Signature | Notes |
|---|---|---|
| `on_focus` | `fn(&mut S, bool, &mut Tx)` | Native window focus; also fires while hidden |
| `on_modifiers` | `fn(&mut S, Mods, &mut Tx)` | Every modifier change, regardless of focus. Cannot consume. None held after blur |
| `on_visibility` | `fn(&mut S, bool, &mut Tx)` | Window shown or hidden (minimised, covered whole), once per change after the first frame. Native only |
| `on_ime` | `fn(&mut S, &Ime, &mut Tx) -> bool` | See `rune-input` |
| `on_close`, `on_quit` | `fn(&mut S, &mut Tx)` | |
| `on_reopen` | `fn(&mut S, bool, &mut Tx) -> bool` | |
| `on_open_url` | `fn(&mut S, &str, &mut Tx)` | |
| `on_window`, `on_ready` | `impl FnMut(&WindowHandle)` | `WindowHandle::with_raw_handle` gives the raw handle only, no winit `Window` |
| `on_frame_encoded`, `on_frame_drawn` | `impl FnMut` | |
| `macos_window(MacWindowOptions)`, `window(WindowSpec)`, `overlay(..)`, `compositor(..)` | | |

Do slow start-up work from `on_ready`, after the first frame.

## The loop

- `view(&S, &mut Cx<S>) -> El` is pure. It re-runs after a handler or `poll` reports a change.
- `poll(&mut S, now_secs, &mut Tx) -> bool` runs at the start of every frame. Return true **only** if visible state changed: false means no view re-run and no frame. An idle window draws nothing.
- Background threads call the `Waker` from `setup` (`Arc<dyn Fn()>`-style) after publishing data. `poll` then picks it up. agentd publishes `Arc<Grid>` behind a mutex and compares `generation`.
- `Live<T>` (`rune::Live`, `LiveText`) is the lighter path for values bound to elements (`live_offset`, `live_trim` and so on): `Live::set` from any thread returns whether the bit pattern changed (equal values do nothing), wakes a frame, and bound elements update with no view commit. Values `poll` or a handler publishes land in that same frame (`latch_live`; `Runtime::live_pending()` is true while a visible window has one unsampled). Hidden windows keep the wake until shown.
- `Tx` requests made in `poll` are honoured. `tx.copy(..)` from `poll` reaches the clipboard, since `latch_requests` runs for polls including hidden ones.
- A `Compositor` only gets `&State` at draw time. It can't write back to state without interior mutability or a channel.

Don't assume poll runs before or after queued input within one frame. I did not confirm the order. If a handler leaves work for `poll`, also call `tx.wake_after(Duration::ZERO)`.

## Tx

- Timers: `tx.wake_after(Duration)`, `tx.wake_at(clock_secs)`, `tx.clock()`. Wakes schedule a frame, not a view commit, and all due ones collapse into one poll. `wake_after(Duration::ZERO)` from `poll` asks for the next frame (paced by the display link on macOS; unpaced or hidden, it waits about a frame), so a repeating poll can't spin. A timed state stores its due time and `poll` checks `clock >= due`.
- `tx.live_only()`: this handler changed only live values, so skip the view re-run. Debug builds check the claim.
- `tx.instant()`: land this change without animation. `tx.focus(key, ring)`: focus an element once on screen; `tx.return_focus(key, ring)` hands focus back to a menu or popover's trigger; `tx.by_keys()`: a key (or Return/Space on a control) brought this change; `tx.focused` works in every handler. `tx.relaunch()`: quit, then start again with the same args.
- Input accessors and clipboard are in `rune-input`.

## Building views

```rust
col().w_full().h_full().fill(t.bg)
    .child(row().w_full().pl(cx.titlebar_inset() + 8.0).pr(8.0).py(6.0).child(chrome))
    .child(effect(TERMINAL_MARK).key("terminal").w_full().grow(1.0).min_h(0.0))
```

- Constructors: `col()`, `row()`, `el()`, `text(..)`, `effect(mark)`, `text_input(&TextEdit, TextStyle)`.
- Builders: `.key .role .label .child .children .w .h .size .w_full .h_full .grow .min_h .px .py .pl .pr .radius .fill .cursor(Cursor::Text) .absolute(left, top) .pin(l, t, r, b) .scroll(ScrollSpec{..}) .clip_soft(r)`.
- Theme: `rune::tok::theme(dark)` has `.bg`, `.text`, `.control_off` and more.
- `Cx`: `titlebar_inset() chrome_insets() env() system_dark() accent() caret(key) layout_size`.
- Handlers on `El`: `on_press`, `on_press_tx`, `on_double_press`, `on_drag`, `on_key`, `on_drop_files`, `on_paste`, `on_secondary_press[_local]`, `on_pointer_move[_local]`, `on_hover_change`, `on_wheel`, `on_layout[_tx]`, `on_drawn_layout[_tx]`. Handlers that take `S` are closures: `|s: &mut State, ..|`.

Performance rules from Rune's own guide: use `Cx::layout_size` rather than whole rects, so scrolling doesn't rebuild the view; use `virtual_rows` or `memo` for long lists; bound every cache in entries and bytes.

## File map (under `crates/rune/`)

| Topic | Path |
|---|---|
| `AppSpec`, CLI wiring | `src/tooling.rs`, `src/tooling/args.rs` |
| `App` hooks | `src/app.rs` |
| `Tx` | `src/el/tx.rs`, `src/el/tx/{keys,press,scroll}.rs`; `tx.copy` in `src/clipboard/tx.rs` |
| `Live`, `LiveText`, live wakes | `src/live.rs`, `src/runtime/motion/live_latch.rs`, `latch_live` call in `src/runtime/frame.rs` |
| `El` builders and handlers | `src/el.rs`, `src/el/{layout,handlers,drop,focus,scroll}.rs` |
| `Cx` | `src/runtime/cx.rs` |
| File dialogs (M3 `⌘N`), not yet read | `src/file_dialog.rs` |
| Native menus (not yet read) | `src/native_menu/` |
| Full docs | `README.md`, `examples/` |
