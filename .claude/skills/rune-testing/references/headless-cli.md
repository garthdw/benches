# Headless CLI

Rune intercepts these flags on the app binary and renders with no window. The compositor still runs, on the same native Metal backend as the window; `RUNE_GPU_BACKEND=wgpu` forces wgpu-hal. At `b5c4fa6` the `fixture`, `unicode` and `tray` scenes snap byte-identical on both.

Unlike a test `Runtime::new`, a headless run builds the runtime from the `App`, so `poll`, `on_key`, `setup` and the `App` hooks (`on_focus`, `on_modifiers`, `on_ime`, `on_visibility`, `modifier_keys`) all apply; a script's `{"focus": false}` reaches `on_window_focus`.

```sh
cargo run -- --snap "$SCRATCH/out.png" --scene fixture --size 960x640 --scale 2
```

| Flag | Use |
|---|---|
| `--snap out.png` | Render offscreen at `--at SECONDS`, or `--at mark:NAME` (`mark:NAME+0.5`) for a script mark's time |
| `--scene NAME`, `--list-scenes` | Start from an `AppSpec::scenes` entry |
| `--chrome` | Paint the macOS traffic lights and their 4 pt clearance box onto the frame, and print each element overlapping them (Benches' sidebar and tab strip sit under them) |
| `--crop KEY\|x,y,w,h` | Crop `--snap` to an element's key or path, or a rect in points |
| `--size WxH`, `--scale N`, `--theme dark\|light` | Viewport in points, device scale (default 2), appearance |
| `--script FILE.json` | Replay timestamped input |
| `--tree out.json` | Element tree with animated values |
| `--frames N --fps 60 --start S --out DIR` | Dump frames |
| `--strip out.png --crop x,y,w,h --every K` | Tile dumped frames |
| `--trace out.jsonl` | Per-frame rects and motion |
| `--pointer`, `--keys [CORNER]` | Draw presses or keycaps on dumped frames |
| `--realtime` | Hold frames to the wall clock, print `rune-mark NAME T`, run `on_ready` after frame one |

`--help` on the binary prints the full list.

## `--script`

`--script` takes a JSON array or `{"events": [...]}`. Each event has `t` (seconds). Forms confirmed:

- `{"focus": false}`, `{"menu": "item"}`, `{"reopen": true}`, `{"open_url": "myapp://..."}`
- `{"physical": "alt+e", "repeat": false}`: press, release, then modifiers up
- `{"hold": "alt"}` until `{"hold": ""}`
- `{"type": "mark", "name": N}`, `{"type": "shortcut", "chord": "cmd+space"}`
- `{"type": "type", "text": "ls", "every": 0.035}` types one `Key::Text` per character, `every` seconds apart; put the next event after the last character
- `{"type": "key", "name": "Enter", "cmd": true}` (modifiers `shift`, `ctrl`, `alt`, `logo`, `cmd`; `"repeat": true`; `"type": "key-up"`); a one-letter name with `cmd` is `Key::Command`
- `{"paste": "text"}`, or `paste` content `{"text", "html", "rtf", "png"}`
- `{"drop_files": {"paths": [...], "key": K}}` (or `"at": [x, y]`) hovers then drops; `{"drag_files": …}` only hovers, `{"drag_files": "leave"}` leaves
- `{"type": "ime", "commit": "…"}` or `{"type": "ime", "preedit": "…", "cursor": [start, end]}`; an empty preedit ends the composition
- Pointer: `{"t": 0.5, "type": "move", "key": "project-1"}` (or `"x"`, `"y"` in points) hovers an element, so a hover state snapshots with `--snap --at 1`. Also `down`, `up`, `tap`/`click`, `double-click`, `drag`, `wheel`, `scroll`, `secondary-click`, `"button": "middle"`.
- The rest: read `src/tooling/script.rs` for exact field names.

## `--window-script`

`--window-script FILE.json` takes the same events, but a real window replays them on the wall clock from its first drawn frame, and drops the system's pointer events meanwhile. It can't combine with any headless flag; `--scene` and `--window KEY` still apply. It prints `rune-script-window KEY`, `rune-mark NAME T ELAPSED` and `rune-script-done T ELAPSED`, or `rune-script-error T ELAPSED WHY` and stops (a missing key, an event a window doesn't replay, or no frame within 10 s). The window stays open after the script ends.

Events go in through `script_input`, as a headless run's do, so the OS path before them is untested. `menu`, `shortcut`, `reopen`, `open_url`, resizes and file dialogs are not replayed and end the script with an error.

Benches reads `config.toml` and `state.json` from `$XDG_CONFIG_HOME/benches` (else `~/.config/benches`), and a real window restores the saved projects and starts their tabs. Point both `HOME` and `XDG_CONFIG_HOME` at an empty scratch folder, with a `config.toml` whose only tab is the shell (the default first tab runs `claude` when it's installed) and an empty `.zshrc`:

```sh
HOME="$SCRATCH/home" XDG_CONFIG_HOME="$SCRATCH/home/.config" target/debug/benches --window-script "$SCRATCH/live.json"
```
