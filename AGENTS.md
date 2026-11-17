# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

Benches is a native macOS workspace where each project is a group of terminals, built on [Rune](https://github.com/anomalyco/rune) (UI) and `libghostty-vt` (terminal emulation). macOS only.

## Commands

The toolchain is pinned in `rust-toolchain.toml`. Building `libghostty-vt` needs Zig 0.17.x on `PATH` (or selected through `ZIG`).

```sh
cargo run                                   # open the app with a live shell
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo test ctrl_c_interrupts                # single test, by name substring; some tests spawn real shells
cargo run -- --snap out.png --scene fixture --size 620x300   # headless render of fixture VT content
cargo run --release --features tooling -- --snap out.png --scene fixture   # release builds need `tooling` for headless flags
cargo run -- config init                    # write a commented config.toml and its schema file; refuses to overwrite
cargo run -- config migrate                 # refresh the schema file and the `#:schema` line of an existing config.toml
cargo run -- config schema > schema/config.schema.json   # regenerate the committed schema
scripts/nerd-font-table PATH/nerd_font_tables.zig   # regenerate src/nerd_font_table.rs from Ghostty's generated table, then cargo fmt
scripts/bundle                              # release build into a signed target/release/benches.app, without installing
scripts/install                             # scripts/bundle, then into ~/Applications/benches.app, relaunching a running copy
scripts/icon                                # regenerate assets/benches.icns from scripts/icon.swift
```

`--snap`, `--scene` and the other headless flags come from Rune. Scenes are registered in `AppSpec::scenes` in `window_app` in `src/app.rs`.

`scripts/install` uses the pinned Rune checkout's installer and launches the app with a Finder-like environment, so nothing from the calling terminal leaks into tabs. It signs with the identity in `MAC_APP_SIGN` (create an Apple Development certificate in Xcode → Settings → Accounts → Manage Certificates; `security find-identity -v -p codesigning` lists it), or ad hoc without one, in which case macOS may ask again for folder access after each install. A bundled app logs to `~/Library/Logs/benches.log`.

Publishing a GitHub release for a `vX.Y.Z` tag runs `.github/workflows/release.yml`, which bundles an ad hoc signed Apple Silicon build versioned from the tag and attaches it to the release as a zip.

## Architecture

Data flows one way from the PTY to the screen, and each `Session` owns its own threads:

```
shell <-> PTY --reader thread--> bounded channel --> actor thread (owns Terminal)
                                                       | publishes Arc<Grid>, calls Waker
                                                       v
         UI thread: poll() compares generation --> TerminalCompositor draws quads
```

- **`session.rs`**: PTY, process and the `Session` handle; the actor thread itself is in `actor.rs` (`Actor`, `ResizeDebounce`, `Titles`, `Publisher`). libghostty types are `!Send`, so a `Terminal` must stay on its actor thread; everything else talks to it through `Msg` over a channel (`protocol.rs` holds `Msg`, `Event` and `Visibility`). The UI only sees immutable `Arc<Grid>` snapshots swapped into a mutex slot, with a `generation` counter for change detection.
- **`grid.rs`**: the snapshot side. `Grid` and `Cell`, `GridBuilder` (reads the render state into a `Grid`), `Presentation`, `Metrics`, `Surface` and `Scrollbar` with its fade, plus `new_terminal`, `apply_theme` and the `FRAME` publish interval.
- **Back-pressure**: PTY output goes through a bounded channel and publishing is capped by `FRAME`, so output floods can't stall the UI. Publishing also respects libghostty's render hold (`on_render_hold`).
- **Spawning**: `openpty` plus `login_tty` in `pre_exec`. Only async-signal-safe calls may run between fork and exec, because the app is multi-threaded.
- **`render.rs`**: a Rune `Compositor` that draws into the `effect(TERMINAL_MARK)` slot declared in `view` with a custom wgpu pass (`cell.wgsl`). Glyphs live in a shared atlas and quads are instanced; a `Slot` per effect mark rebuilds only when generation, rect, atlas epoch or blend mode changes. The compositor also derives cols and rows from the slot size and sends `Session::resize`. The actor applies a new grid size, and so sends SIGWINCH, only once it has held for `RESIZE_SETTLE`, because inline TUIs such as Claude Code reprint on every SIGWINCH.
- **`font.rs`**: CoreText shaping and fallback through `CTLine`, rasterised into whole-cell premultiplied RGBA bitmaps in device pixels. A glyph with any `rgb != alpha` pixel is treated as colour, so font smoothing must stay off. The atlas resets when the scale changes.
- **`constraint.rs`** and **`nerd_font_table.rs`**: Ghostty's `Constraint`/`constrain` (size modes, scale groups, padding, `icon_height`) over `font.rs` metrics (`adjust-icon-height` scales its icon heights), with Ghostty's `isSymbol` rule fitting non-Nerd-Font symbols (arrows, dingbats, pictographs) and the per-codepoint table generated from Ghostty's `nerd_font_tables.zig` (libghostty-vt's build leaves it under `target/*/build/libghostty-vt-sys-*/out/ghostty-src/src/font/`). The bundled `SymbolsNerdFont-Regular.ttf` must be the non-Mono font Ghostty embeds, as the scale groups were measured from its glyph bounds.
- **`sprite.rs`**: procedural box drawing, blocks, Powerline glyphs and tiling underlines, so they meet their neighbours with no seams.
- **`workspace.rs`**: projects (folders) holding tabs and a tray, each with its own `Session`. Selection hides the outgoing session and shows the incoming one: a hidden session keeps reading its PTY but doesn't publish grids or wake the UI, so a flood in a background tab costs nothing on the UI thread. The compositor resizes every session to the terminal area, so hidden tabs are already the right size when shown, and keys its quad cache by session id because generations restart per session.
- **Windows**: each window has its own `State` and `Workspace`, so selection, hidden sessions and sizing stay per window. `Fleet` (in `windows.rs`) hands a `Project` to another window's inbox, which takes it in `poll` and retargets each tab's `Retarget` waker. Closing a torn-off window merges its projects back into the main window; closing the main window quits. Torn-off windows aren't restored: the layout saved at quit lists their projects after the main window's.
- **Reported directory**: the actor stores the local pwd from `on_pwd_changed` (OSC 7/9;9/1337, parsed in `link.rs`, ignoring other hosts), and `Session::cwd` prefers it over `proc_pidinfo`.
- **`app.rs`**: `State`, `window_app` (the `AppSpec` and hooks), `poll`, `on_key` and the tab, layout and close-confirm logic. `main.rs` is only the entry point, `config` CLI dispatch and the crate-wide `LogErr`.
- **`input.rs`**: maps Rune key events to `KeyInput`; the actor encodes them with libghostty's encoder using the terminal's current modes. Returning `None` from `map_key` keeps a key (such as a ⌘ chord) away from the terminal.
- **Pointer input** (`terminal_input.rs`): the UI sends raw `Msg::Pointer` and `Msg::Wheel`, and `TerminalInput` on the actor decides between mouse reports, selection, alt-screen arrows and scrollback, because only the actor knows the terminal's modes.

Dependencies run one way, UI down to core: `app`/`view`/`input`/`render` -> `session` -> `actor` -> `protocol` -> `grid`/`terminal_input`. Core modules (`actor`, `protocol`, `grid`, `terminal_input`, `config`, `font`, `constraint`, `sprite`, `link`, `notify`) must not import `app`, `view`, `input`, `render`, `workspace` or `session`, and use Rune only for `Waker` (and in tests), so they can move into their own crate once a second frontend needs them.

`Grid::from_vt` (`grid.rs`) and `Session::fixture` give a process-free grid for tests and scenes.

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

A change is done when it works in the real app, not just when tests pass. Use `--snap --scene fixture` to inspect rendering. Rune's headless runs go through the same compositor, but a snapshot can't prove physical keyboard behaviour.
