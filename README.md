# Benches

A native macOS workspace where each project is a group of terminals. See `AGENTS.md` for development.

## Terminfo and ssh

The bundled app sets `TERM=xterm-ghostty` and points `TERMINFO` at a compiled copy of Ghostty's entry, so programs such as nvim see undercurl, synchronized output and true colour. Builds outside a bundle (`cargo run`) fall back to `xterm-256color`.

ssh doesn't forward `TERMINFO`, so remote hosts need the entry installed once:

```sh
infocmp -x | ssh host tic -x -
```
