# Headless CLI

Rune intercepts these flags on the app binary and renders with no window. The compositor still runs, on the same native Metal backend as the window; `RUNE_GPU_BACKEND=wgpu` forces wgpu-hal. At `7dfaa8f` the `fixture`, `unicode` and `tray` scenes snap byte-identical on both.

```sh
cargo run -- --snap "$SCRATCH/out.png" --scene fixture --size 960x640 --scale 2
```

| Flag | Use |
|---|---|
| `--snap out.png` | Render offscreen at `--at SECONDS` |
| `--scene NAME`, `--list-scenes` | Start from an `AppSpec::scenes` entry |
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
- `paste` content takes `{"text", "html", "rtf", "png"}`
- `{"type": "ime", "commit": "…"}` or `{"type": "ime", "preedit": "…", "cursor": [start, end]}`; an empty preedit ends the composition
- Pointer: `{"t": 0.5, "type": "move", "key": "project-1"}` (or `"x"`, `"y"` in points) hovers an element, so a hover state snapshots with `--snap --at 1`. Also `down`, `up`, `tap`/`click`, `double-click`, `drag`, `wheel`, `scroll`, `secondary-click`, `"button": "middle"`.
- Keys and the rest: read `src/tooling/script.rs` for exact field names.
