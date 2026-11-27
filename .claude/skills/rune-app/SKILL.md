---
name: rune-app
description: Rune app structure as used by Benches - AppSpec, App hooks, the view/poll loop, Waker, Tx, timers, layout builders. Use when changing main.rs, State, the view, poll, or wiring background threads into the UI, instead of reading the Rune source.
---

# Rune app structure

Verified against Rune rev `4918969` (the pin in `Cargo.toml`). If the pin has moved, check anything you rely on.
Source, when this isn't enough: `~/.cargo/git/checkouts/rune-c9b805e9ed8a3730/4918969/crates/rune`. Use the file map at the end.

## The loop

- `view(&S, &mut Cx<S>) -> El` is pure. It re-runs after a handler or `poll` reports a change.
- `poll(&mut S, now_secs, &mut Tx) -> bool` runs at the start of every frame. Return true **only** if visible state changed: false means no view re-run and no frame. An idle window draws nothing.
- Background threads call the `Waker` from `setup` (`Arc<dyn Fn() + Send + Sync>`) after publishing data. `poll` then picks it up. Benches publishes `Arc<Grid>` behind a mutex and compares `generation`.
- `Live<T>` (`rune::Live`, `LiveText`) is the lighter path for values bound to elements (`live_offset`, `live_trim` and so on): `Live::set` from any thread returns whether the bit pattern changed (equal values do nothing), wakes a frame, and bound elements update with no view commit. Values `poll` or a handler publishes land in that same frame (`latch_live`; `Runtime::live_pending()` is true while a visible window has one unsampled). Hidden windows keep the wake until shown.
- A window going off screen or back wakes `poll` once (after the first frame), so `poll` can read `tx.window_visible()` and pause work no one sees. `App::on_visibility` hears the same change.
- `Tx` requests made in `poll` are honoured. `tx.copy(..)` from `poll` reaches the clipboard, since `latch_requests` runs for polls including hidden ones.
- A `Compositor` only gets `&State` at draw time. It can't write back to state without interior mutability or a channel.

After any handler not marked `live_only`, `poll` runs again in the same frame before the view commits (up to 4 rounds, `MAX_POLL_ROUNDS`), so `poll` sees a handler's change at once. A test that reads state after `press` sees what that `poll` left, not the handler's raw result. A `live_only` handler that leaves work for `poll` calls `tx.request_poll()`.

## References

| Task | Reference |
|---|---|
| `AppSpec` literal, `App` builder hooks (`on_focus`, `on_modifiers`, `on_ready`, ...), menu items, macOS launch order | [references/app-spec-and-hooks.md](references/app-spec-and-hooks.md) |
| `Tx`: timers and wakes, `live_only`, visibility, focus requests, quit and relaunch | [references/tx.md](references/tx.md) |
| Building views: constructors, builders, theme, `Cx`, `El` handlers, performance rules | [references/views.md](references/views.md) |

## File map (under `crates/rune/`)

Foundation, text, editing, GPU, image and media code lives in sibling crates (`rune-core`, `rune-text`, `rune-edit`, `rune-gpu`, `rune-image`, `rune-media`); `rune` re-exports what Benches uses.

| Topic | Path |
|---|---|
| `AppSpec`, CLI wiring | `src/tooling.rs`, `src/tooling/args.rs` |
| `App` hooks | `src/app.rs`; `src/app/config.rs` (`RuntimeConfig`) hands them to every host |
| `Tx` | `src/el/tx.rs`, `src/el/tx/{keys,press,scroll}.rs`; `tx.copy` in `src/clipboard/tx.rs` |
| `Live`, `LiveText`, `Waker`, `tok` | `../rune-core/src/{live,lib,tok}.rs` |
| Live wakes | `src/runtime/motion/live_latch.rs`; `latch_live` calls in `src/runtime/frame.rs`, `src/runtime/frame/motion.rs` |
| `TextEdit`, `KeyEvent`, `Ime` | `../rune-edit/src/` |
| `El` builders and handlers | `src/el.rs`, `src/el/{layout,layout_watch,handlers,drop,focus,scroll,pixel_snap}.rs`; `on_wheel` in `src/input/canvas_input.rs` |
| `Cx` | `src/runtime/cx.rs`; `layout_size` in `src/runtime/layout_read.rs` |
| `App::layout_seed`, `LayoutSeed` | `src/runtime/layout_seed.rs` |
| `WindowHandle`, `WindowSpec`, `open_window` | `src/window/handle.rs`, `src/window/spec.rs` |
| File dialogs (`⌘N`), not yet read | `src/file_dialog.rs` |
| Native menus: `MenuItem`, `App::menu_item`; macOS default and standard items | `src/native_menu/mod.rs`; `src/native_menu/{standard,default_macos}.rs` |
| Full docs | `README.md`, `examples/` |
