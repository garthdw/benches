# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

agentd is a native macOS workspace where each project is a group of terminals, built on [Rune](https://github.com/anomalyco/rune) (UI) and `libghostty-vt` (terminal emulation). macOS only. `docs/ROADMAP.md` lists the delivery stages (M0–M6); a stage is done when every exit criterion is met in the real app.

## Commands

The toolchain is pinned in `rust-toolchain.toml`. Building `libghostty-vt` needs Zig 0.17.x on `PATH` (or selected through `ZIG`).

```sh
cargo run                                   # open the app with a live shell
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo test ctrl_c_interrupts                # single test, by name substring; some tests spawn real shells
cargo run -- --snap out.png --scene fixture --size 620x300   # headless render of fixture VT content
```

`--snap`, `--scene` and the other headless flags come from Rune. Scenes are registered in `AppSpec::scenes` in `src/main.rs`.

## Architecture

Data flows one way from the PTY to the screen, and each `Session` owns its own threads:

```
shell <-> PTY --reader thread--> bounded channel --> actor thread (owns Terminal)
                                                       | publishes Arc<Grid>, calls Waker
                                                       v
         UI thread: poll() compares generation --> TerminalCompositor draws quads
```

- **`session.rs`**: PTY, process and actor thread. libghostty types are `!Send`, so a `Terminal` must stay on its actor thread; everything else talks to it through `Msg` over a channel. The UI only sees immutable `Arc<Grid>` snapshots swapped into a mutex slot, with a `generation` counter for change detection.
- **Back-pressure**: PTY output goes through a bounded channel and publishing is capped by `FRAME`, so output floods can't stall the UI. Publishing also respects libghostty's render hold (`on_render_hold`).
- **Spawning**: `openpty` plus `login_tty` in `pre_exec`. Only async-signal-safe calls may run between fork and exec, because the app is multi-threaded.
- **`render.rs`**: a Rune `Compositor` that draws into the `effect(TERMINAL_MARK)` slot declared in `view` with a custom wgpu pass (`cell.wgsl`). Glyphs live in a shared atlas and quads are instanced; a `Slot` per effect mark rebuilds only when generation, rect, atlas epoch or blend mode changes. The compositor also derives cols and rows from the slot size and sends `Session::resize`.
- **`font.rs`**: CoreText rasterisation into whole-cell coverage bitmaps, in device pixels. The atlas resets when the scale changes.
- **`input.rs`**: maps Rune key events to `KeyInput`; the actor encodes them with libghostty's encoder using the terminal's current modes. Returning `None` from `map_key` keeps a key (such as a ⌘ chord) away from the terminal.

`Grid::from_vt` and `Session::fixture` give a process-free grid for tests and scenes.

## Rust coding guidelines

- Prioritize code correctness and clarity. Speed and efficiency are secondary priorities unless otherwise specified.
- Do not write organizational or comments that summarize the code. Comments should only be written in order to explain "why" the code is written in some way in the case there is a reason that is tricky / non-obvious.
- Prefer implementing functionality in existing files unless it is a new logical component. Avoid creating many small files.
- Avoid using functions that panic like `unwrap()`, instead use mechanisms like `?` to propagate errors.
- Be careful with operations like indexing which may panic if the indexes are out of bounds.
- Never silently discard errors with `let _ =` on fallible operations. Always handle errors appropriately:
  - Propagate errors with `?` when the calling function should handle them
  - Use `.log_err()` or similar when you need to ignore errors but want visibility
  - Use explicit error handling with `match` or `if let Err(...)` when you need custom logic
  - Example: avoid `let _ = client.request(...).await?;` - use `client.request(...).await?;` instead
- When implementing async operations that may fail, ensure errors propagate to the UI layer so users get meaningful feedback.
- Never create files with `mod.rs` paths - prefer `src/some_module.rs` instead of `src/some_module/mod.rs`.
- When creating new crates, prefer specifying the library root path in `Cargo.toml` using `[lib] path = "...rs"` instead of the default `lib.rs`, to maintain consistent and descriptive naming (e.g., `main.rs`).
- Avoid creative additions unless explicitly requested
- Use full words for variable names (no abbreviations like "q" for "queue")
- Use variable shadowing to scope clones in async contexts for clarity, minimizing the lifetime of borrowed references.
  Example:
  ```rust
  executor.spawn({
      let task_ran = task_ran.clone();
      async move {
          *task_ran.borrow_mut() = true;
      }
  });
  ```

## Verifying changes

A roadmap exit criterion means working in the real app, not just passing tests. Use `--snap --scene fixture` to inspect rendering. Rune's headless runs go through the same compositor, but a snapshot can't prove physical keyboard behaviour.
