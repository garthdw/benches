---
name: libghostty-vt
description: libghostty-vt Rust bindings as used by agentd - threading, render state, cells and graphemes, key and mouse encoders, selection gestures, formatting, paste, scrollback, callbacks. Use when changing session.rs or anything that talks to Terminal, instead of reading the bindings source.
---

# libghostty-vt

Verified against libghostty-rs rev `768add9` (the pin in `Cargo.toml`). The API is marked unstable upstream, so if the pin has moved, check anything you rely on.
Source: `~/.cargo/git/checkouts/libghostty-rs-28fee7453bdb2b25/768add9/crates/libghostty-vt/src`.

## Threading

Every type is `!Send` and `!Sync`. agentd keeps `Terminal` and everything tied to it on one actor thread per session and talks to it over channels. Callbacks run on the thread that called `vt_write`, `reset` or `resize`, so they need no locking.

## References

Read the one that matches the task:

| Task | Reference |
|---|---|
| `Terminal` setup, state, modes, callbacks, render hold, scrollback, building Ghostty | [references/terminal.md](references/terminal.md) |
| Reading the screen: `RenderState`, rows, cells, graphemes, styles, cursor | [references/render-state.md](references/render-state.md) |
| Encoding keys, mouse reports and focus events | [references/encoders.md](references/encoders.md) |
| Selection gestures, copying a selection, paste | [references/selection-and-paste.md](references/selection-and-paste.md) |

## File map (under `crates/`)

| Topic | Path |
|---|---|
| Terminal, modes, callbacks | `libghostty-vt/src/terminal.rs` |
| Render iterators | `libghostty-vt/src/render.rs` |
| Cells, `GridRef`, `Screen` | `libghostty-vt/src/screen.rs` |
| Selection | `libghostty-vt/src/selection.rs`, `selection/gesture.rs` |
| Encoders | `libghostty-vt/src/key.rs`, `mouse.rs`, `focus.rs` |
| Paste, formatting | `libghostty-vt/src/paste.rs`, `fmt.rs` |
| Raw FFI with docs | `libghostty-vt-sys/src/bindings.rs` |
| Complete small terminal | `../example/ghostling_rs/src/main.rs` |
