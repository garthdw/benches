# Benches

A native macOS terminal workspace. Each project is a folder with its own group of terminal tabs, so
switching projects brings back the shells, editors and agents you left running there.

Benches is built on [Rune](https://github.com/anomalyco/rune) for the UI and
[libghostty-vt](https://github.com/ghostty-org/ghostty) for terminal emulation, and reads your
Ghostty config for its font and theme.

## Features

- Projects in a sidebar, each with tabs and a drawer (`⌘J`) for a side process such as a dev server.
  Background tabs keep running but cost nothing to draw.
- A command centre (`⌘P`) to switch tabs, open a folder found by a jump command such as `zoxide`, create
  a git worktree with [Worktrunk](https://github.com/max-sixty/worktrunk), or run a `!command` in a
  new tab, completed from your shell history.
- A git overlay (`⌘G`, `lazygit` by default).
- Find in scrollback (`⌘F`), jump between prompts (`⌘↑`/`⌘↓`), and select the last command's output.
- A notification when a long command finishes in a tab you aren't looking at.
- Secure keyboard entry turns on by itself while a program reads a password.
- Tear a project off into its own window and merge it back.
- Kitty graphics, OSC 52 clipboard, hyperlinks, and Nerd Font icons sized like Ghostty sizes them.

## Install

Requires macOS 13 or later. Release builds are for Apple Silicon.

Download the zip from the [latest release](../../releases/latest), unzip it and move `Benches.app` to
`~/Applications` or `/Applications`. The app is not notarized, so macOS blocks the first launch:
open it once, then choose **Open Anyway** in System Settings → Privacy & Security, or run:

```sh
xattr -dr com.apple.quarantine /Applications/Benches.app
```

## Build from source

You need the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it), Zig 0.17.x on
`PATH` to build libghostty-vt, and [`cargo-about`](https://github.com/EmbarkStudios/cargo-about)
to bundle the app's third-party notices (`cargo install cargo-about --features cli`).

```sh
cargo run          # run from the checkout
scripts/install    # build a signed Benches.app into ~/Applications
```

## Configuration

Benches works without a config file. Font and theme settings fall back to
`~/.config/ghostty/config`. To customise it:

```sh
benches=/Applications/Benches.app/Contents/MacOS/Benches
$benches config init      # write ~/.config/benches/config.toml with every option commented
$benches config migrate   # refresh the schema reference after an upgrade
```

Set the shell, theme, font, the tabs a new project opens with, the jump and git commands, the
starting folder, clipboard access and notifications. Each option is documented in the generated file and in
[`schema/config.schema.json`](schema/config.schema.json). Reload with `⇧⌘,`.

## Terminfo and ssh

The bundled app sets `TERM=xterm-ghostty` and points `TERMINFO` at a compiled copy of Ghostty's entry, so programs such as nvim see undercurl, synchronized output and true colour. Builds outside a bundle (`cargo run`) fall back to `xterm-256color`.

ssh doesn't forward `TERMINFO`, so remote hosts need the entry installed once:

```sh
infocmp -x | ssh host tic -x -
```

## Contributing

See [`AGENTS.md`](AGENTS.md) for the architecture, commands and coding guidelines.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

Benches includes code from Ghostty, libghostty-vt and its dependencies, Rune, libghostty-rs and
the Symbols Nerd Font. Their notices are in the app's About window, generated from
[`about.toml`](about.toml) and [`about.hbs`](about.hbs).
