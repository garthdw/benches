---
name: rune-app
description: Rune app structure as used by Benches - AppSpec, App hooks, the view/poll loop, Waker, Tx, timers, layout builders. Use when changing main.rs, State, the view, poll, or wiring background threads into the UI, instead of reading the Rune source.
---

# Rune app structure

Verified against Rune rev `7dfaa8f` (the pin in `Cargo.toml`). If the pin has moved, check anything you rely on.
Source, when this isn't enough: `~/.cargo/git/checkouts/rune-c9b805e9ed8a3730/7dfaa8f/crates/rune`. Use the file map at the end.

## The loop

- `view(&S, &mut Cx<S>) -> El` is pure. It re-runs after a handler or `poll` reports a change.
- `poll(&mut S, now_secs, &mut Tx) -> bool` runs at the start of every frame. Return true **only** if visible state changed: false means no view re-run and no frame. An idle window draws nothing.
- Background threads call the `Waker` from `setup` (`Arc<dyn Fn()>`-style) after publishing data. `poll` then picks it up. Benches publishes `Arc<Grid>` behind a mutex and compares `generation`.
- `Live<T>` (`rune::Live`, `LiveText`) is the lighter path for values bound to elements (`live_offset`, `live_trim` and so on): `Live::set` from any thread returns whether the bit pattern changed (equal values do nothing), wakes a frame, and bound elements update with no view commit. Values `poll` or a handler publishes land in that same frame (`latch_live`; `Runtime::live_pending()` is true while a visible window has one unsampled). Hidden windows keep the wake until shown.
- `Tx` requests made in `poll` are honoured. `tx.copy(..)` from `poll` reaches the clipboard, since `latch_requests` runs for polls including hidden ones.
- A `Compositor` only gets `&State` at draw time. It can't write back to state without interior mutability or a channel.

Don't assume poll runs before or after queued input within one frame. I did not confirm the order. If a handler leaves work for `poll`, also call `tx.wake_after(Duration::ZERO)`.

## References

| Task | Reference |
|---|---|
| `AppSpec` literal, `App` builder hooks (`on_focus`, `on_modifiers`, `on_ready`, ...) | [references/app-spec-and-hooks.md](references/app-spec-and-hooks.md) |
| `Tx`: timers and wakes, `live_only`, focus requests, relaunch | [references/tx.md](references/tx.md) |
| Building views: constructors, builders, theme, `Cx`, `El` handlers, performance rules | [references/views.md](references/views.md) |

## File map (under `crates/rune/`)

| Topic | Path |
|---|---|
| `AppSpec`, CLI wiring | `src/tooling.rs`, `src/tooling/args.rs` |
| `App` hooks | `src/app.rs` |
| `Tx` | `src/el/tx.rs`, `src/el/tx/{keys,press,scroll}.rs`; `tx.copy` in `src/clipboard/tx.rs` |
| `Live`, `LiveText`, live wakes | `src/live.rs`, `src/runtime/motion/live_latch.rs`, `latch_live` call in `src/runtime/frame.rs` |
| `El` builders and handlers | `src/el.rs`, `src/el/{layout,handlers,drop,focus,scroll}.rs` |
| `Cx` | `src/runtime/cx.rs` |
| File dialogs (`⌘N`), not yet read | `src/file_dialog.rs` |
| Native menus (not yet read) | `src/native_menu/` |
| Full docs | `README.md`, `examples/` |
