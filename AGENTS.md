# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

Benches is a native workspace where each project is a group of terminals, built on [Rune](https://github.com/anomalyco/rune) (UI) and `libghostty-vt` (terminal emulation). It runs on macOS and on Linux, where it is built from source; releases are macOS only. The README's "Building on Linux" lists the Linux packages.

## Commands

The toolchain is pinned in `rust-toolchain.toml`. Building `libghostty-vt` needs Zig 0.17.x on `PATH` (or selected through `ZIG`).

```sh
cargo run                                   # open the app with a live shell
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test   # every crate, via default-members
cargo test ctrl_c_interrupts                # single test, by name substring; some tests spawn real shells
cargo run -- --snap out.png --scene fixture --size 620x300   # headless render of fixture VT content
cargo run --release --features tooling -- --snap out.png --scene fixture   # release builds need `tooling` for headless flags
cargo test --features swash && cargo run --features swash -- --snap out.png --scene unicode   # the Linux font backend on macOS, to compare with CoreText
cargo run -- config init                    # write a commented config.toml and its schema file; refuses to overwrite
cargo run -- config migrate                 # refresh the schema file and the `#:schema` line of an existing config.toml
cargo run -- config schema > schema/config.schema.json   # regenerate the committed schema
scripts/nerd-font-table PATH/nerd_font_tables.zig   # regenerate crates/benches-font/src/nerd_font_table.rs from Ghostty's generated table, then cargo fmt
scripts/bundle                              # macOS: release build into a signed target/release/Benches.app, without installing
scripts/install                             # macOS: scripts/bundle, then into ~/Applications/Benches.app, relaunching a running copy
scripts/icon                                # macOS: regenerate assets/benches.icns from scripts/icon.swift
```

On Linux, the tests also expect fontconfig's `monospace` family, an emoji font, Unifont, `/bin/bash` and `pgrep` (`sudo apt install fonts-dejavu-core fonts-noto-color-emoji fonts-unifont procps` on Debian and Ubuntu; add `zsh` for the tests that skip without it), and Monaspace Neon, the default font, which they don't package: install its static fonts from [its releases](https://github.com/githubnext/monaspace/releases) into `~/.local/share/fonts`. Without a GPU, `--snap` runs on Mesa's lavapipe.

`scripts/bundle` needs `cargo-about` (`cargo install cargo-about --features cli`): it writes the About panel's third-party notices from `about.toml` and `about.hbs`, whose non-Cargo entries (Ghostty, the Zig and C libraries linked into libghostty-vt, the Symbols Nerd Font) are kept by hand. Code ported from Ghostty says so in a header comment.

`--snap`, `--scene` and the other headless flags come from Rune. Scenes are registered in `AppSpec::scenes` in `window_app` in `src/app.rs`.

`scripts/install` uses the pinned Rune checkout's installer and launches the app with a Finder-like environment, so nothing from the calling terminal leaks into tabs. It signs with the identity in `MAC_APP_SIGN` (create an Apple Development certificate in Xcode → Settings → Accounts → Manage Certificates; `security find-identity -v -p codesigning` lists it), or ad hoc without one, in which case macOS may ask again for folder access after each install, and a Full Disk Access grant must be added again. A bundled app logs to `~/Library/Logs/benches.log`. On Linux, a launch whose stderr isn't a terminal logs to `~/.local/state/benches/benches.log` (`dirs::log_file`).

CI (`.github/workflows/ci.yml`) runs the checks on macOS and, without `cargo fmt`, on Ubuntu, where it also uploads a `--snap` of the fixture scene as the `linux-fixture` artifact. Its Linux job lists the packages and fonts (Monaspace's static build, Noto Color Emoji, Unifont) that the tests expect.

Publishing a GitHub release for a `vX.Y.Z` tag runs `.github/workflows/release.yml`, which bundles an ad hoc signed Apple Silicon build versioned from the tag and attaches it to the release as a zip.

## Architecture

The repository is a Cargo workspace:

- `crates/benches-platform` (`platform.rs`): OS services behind one API: opening a link, opening a PTY with both ends close-on-exec, process queries, the system locale and the fallback locale and shell, thread priority, the Full Disk Access settings link, the secure-input call and notifications. `platform.rs` declares each and forwards to `os` (`open_pty` is shared code there but for opening the replica), which `#[cfg(target_os)]` binds to `macos.rs` or `linux.rs`; each OS's dependencies are target-specific in the crate's `Cargo.toml`. On Linux, each D-Bus user (notifications, revealing a file) blocks on its own thread, using Rune's zbus (5, over async-io). `dirs.rs` holds the folders for config, state, logs, Ghostty's themes and the installed terminfo (`PREFIX/share/benches/terminfo` beside `PREFIX/bin` on Linux), branching with `cfg!`.
- `crates/benches-font` (`font.rs` is the crate root): `font`, `coretext`, `swash`, `colr`, `constraint`, `sprite` and `nerd_font_table`, with `DEFAULT_FONT_FAMILY` and the `Fnv` hasher.
- `crates/benches-core` (`core.rs`, which also holds `LogErr`): `actor`, `protocol`, `grid`, `terminal_input`, `link`, `search`, `images`, `config` and `history`.
- The root `benches` package: `app`, `view`, `input`, `render`, `session`, `workspace`, `windows`, `palette`, `keymap`, `find`, `jump`, `scenes`, `secure_input` and `main`.

Other platform differences stay where they're used, behind `cfg!(target_os = "macos")` or `#[cfg]` (`rg target_os` finds them), apart from `keymap.rs`'s `Keys`, which pairs each chord for macOS and Linux through Rune's `KeyPlatform`. Rune's native menu, which holds the Reload Config item, exists only on macOS.

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
- **Spawning**: `benches_platform::open_pty` plus `login_tty` in `pre_exec`. Only async-signal-safe calls may run between fork and exec, because the app is multi-threaded, and every fd the app opens must be close-on-exec from the start, or another thread's child inherits it.
- **`render.rs`**: a Rune `Compositor` that draws into the `effect(TERMINAL_MARK)` slot declared in `view` with a custom wgpu pass (`cell.wgsl`). Glyphs live in a shared atlas and quads are instanced; a `Slot` per effect mark rebuilds only when generation, rect, atlas epoch or blend mode changes. The compositor also derives cols and rows from the slot size and sends `Session::resize`. The actor applies a new grid size, and so sends SIGWINCH, only once it has held for `RESIZE_SETTLE`, because inline TUIs such as Claude Code reprint on every SIGWINCH.
- **`font.rs`**: face choice, Ghostty's metrics, run shaping and constraint placement over a private `Backend` trait, rasterised into whole-cell premultiplied RGBA bitmaps in device pixels. A glyph with any `rgb != alpha` pixel is treated as colour, so font smoothing must stay off. The atlas resets when the scale changes.
- **`coretext.rs`** (macOS) and **`swash.rs`** (Linux, or macOS with the `swash` feature): the backends. CoreText falls back through `CTLine`. The swash backend finds fonts with fontique, shapes with harfrust and draws with swash; it falls back per grapheme cluster (emoji font, the script's fallback, the generic families, then every family, then a monochrome pass for emoji), synthesises missing bold and italic as Ghostty's FreeType path does, except for colour glyphs, which stay upright and regular as CoreText draws them, and draws outlines, bitmap strikes and COLR v0, CBDT and sbix emoji. `colr.rs`, ported from Rune, paints COLR v1 glyphs (gradients, transforms, composite layers) through skrifa's paint graph, ahead of swash's sources. Its faces hold no backend state: the backend owns the font collection, scale context and fallback caches, so every measuring and drawing method takes `&self`. Under `--features swash`, tests also check its metrics and ink against CoreText.
- **`constraint.rs`** and **`nerd_font_table.rs`**: Ghostty's `Constraint`/`constrain` (size modes, scale groups, padding, `icon_height`) over `font.rs` metrics (`adjust-icon-height` scales its icon heights), with Ghostty's `isSymbol` rule fitting non-Nerd-Font symbols (arrows, dingbats, pictographs) and the per-codepoint table generated from Ghostty's `nerd_font_tables.zig` (libghostty-vt's build leaves it under `target/*/build/libghostty-vt-sys-*/out/ghostty-src/src/font/`). The bundled `crates/benches-font/assets/SymbolsNerdFont-Regular.ttf` must be the non-Mono font Ghostty embeds, as the scale groups were measured from its glyph bounds.
- **`sprite.rs`**: procedural box drawing, blocks, Powerline glyphs and tiling underlines, so they meet their neighbours with no seams.
- **`workspace.rs`**: projects (folders) holding tabs and a tray, each with its own `Session`. Selection hides the outgoing session and shows the incoming one: a hidden session keeps reading its PTY but doesn't publish grids or wake the UI, so a flood in a background tab costs nothing on the UI thread. The compositor resizes every session to the terminal area, so hidden tabs are already the right size when shown, and keys its quad cache by session id because generations restart per session.
- **Windows**: each window has its own `State` and `Workspace`, so selection, hidden sessions and sizing stay per window. `Fleet` (in `windows.rs`) hands a `Project` to another window's inbox, which takes it in `poll` and retargets each tab's `Retarget` waker. Closing a torn-off window merges its projects back into the main window; closing the main window quits. Torn-off windows aren't restored: the layout saved at quit lists their projects after the main window's.
- **Reported directory**: the actor stores the local pwd from `on_pwd_changed` (OSC 7/9;9/1337, parsed in `link.rs`, ignoring other hosts), and `Session::cwd` prefers it over the platform crate's `process_cwd`.
- **`app.rs`**: `State`, `window_app` (the `AppSpec` and hooks), `poll`, `on_key` and the tab, layout and close-confirm logic. `main.rs` is only the entry point and `config` CLI dispatch.
- **`input.rs`**: maps Rune key events to `KeyInput`; the actor encodes them with libghostty's encoder using the terminal's current modes. Returning `None` from `map_key` keeps a key (such as a ⌘ chord) away from the terminal.
- **Pointer input** (`terminal_input.rs`): the UI sends raw `Msg::Pointer` and `Msg::Wheel`, and `TerminalInput` on the actor decides between mouse reports, selection, alt-screen arrows and scrollback, because only the actor knows the terminal's modes.

Dependencies run one way, UI down to core: `app`/`view`/`input`/`render` -> `session` -> `actor` -> `protocol` -> `grid`/`terminal_input`. Across crates, `benches` -> `benches-core` -> `benches-font` and `benches-platform`; the font and platform crates depend on neither core nor each other, and the font crate stays free of libghostty. Core doesn't depend on Rune (its `Waker` is the same type as Rune's), so a second frontend can reuse it.

Core and font helpers that the app's tests share (`test_support`, `Font::new`, `Grid::lines`) sit behind each crate's `test-support` feature, which the root enables as a dev-dependency. The feature only exposes helpers: Cargo unifies features across one build, so `--all-targets` would hand it to the dev binary too. Test-only behaviour, such as `app::default_config`'s `/bin/sh` shell, belongs in the app under `cfg!(test)`.

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
