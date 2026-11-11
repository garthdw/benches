---
name: rune-testing
description: Testing and verifying Rune apps in agentd - the headless Runtime harness, --snap/--tree/--script flags, named scenes, and what headless runs cannot prove. Use when writing UI tests or checking a rendering or input change, instead of reading the Rune source.
---

# Testing Rune apps

Verified against Rune rev `ba5e7c8`. If the pin has moved, check anything you rely on.

## Unit tests with `Runtime`

No window or GPU is needed.

```rust
let env = Env { system_dark: true, scale: 2.0 };
let mut rt = Runtime::new(State::default(), view, (960.0, 640.0), env);
rt.on_key = Some(|_, _, _| true);   // Runtime::new does not wire the AppSpec hooks
rt.frame(0.0);
let (x, y) = rt.find("chrome").unwrap();
rt.pointer_down(x, y);
rt.pointer_up(x, y);
rt.frame(1.0 / 60.0);               // input is queued: run a frame before asserting
rt.key(KeyEvent { key: Key::Tab, mods: Mods::default() });
rt.frame(1.0 / 60.0);
```

Driving input (points, window space):

- Pointer: `pointer_move, pointer_down, pointer_up, pointer_leave, button_down(x, y, PointerButton), button_up`.
- Wheel: `wheel(x, y, dy)`, or `wheel_event(WheelEvent::new(dx, dy).at(x, y).mods(m).phase(p).precise(true))`.
- Keys: `key, repeat_key, key_up, physical_input(PhysicalInput)`.
- Text: `ime(Ime), cancel_composition(), paste(text), paste_content(content)`.
- Gestures: `magnify_event, rotate_event, smart_magnify`.

Inspect with:

- `find(key) -> Option<(f32, f32)>`: element centre in points, `None` once exiting.
- `presented(key) -> Option<([x, y, w, h], opacity)>`: drawn rect as of the last frame, exiting elements included.
- `pointer()`, `cursor()`, `pressing()`, `has_focus()`, `has_pending_input()`, `wake_deadline()`.
- `stats.commits`: view re-runs. Unchanged across a `frame` means a paint-only frame. `live_pending()` is true while a live source has an update waiting.
- `file_dialog()` shows the pending `Tx::open_file`/`save_file`/`open_folder` request; `resolve_file_dialog(Some(path))` or `None` (cancel) answers it, and the callback runs on the next frame. Errors if none is open.

`pointer_move` then one `frame` is enough for hover state. `El::hover_delays` elements also need frames past their delay.

Function-pointer hooks can't capture, so count calls with a `static` atomic, as `main.rs` does.

## Headless CLI

Rune intercepts these flags on the app binary and renders with no window. The compositor still runs, on the same native Metal backend as the window; `RUNE_GPU_BACKEND=wgpu` forces wgpu-hal. At `ba5e7c8` the `fixture`, `unicode` and `tray` scenes snap byte-identical on both.

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

`--script` takes a JSON array or `{"events": [...]}`. Each event has `t` (seconds). Forms confirmed:

- `{"focus": false}`, `{"menu": "item"}`, `{"reopen": true}`, `{"open_url": "myapp://..."}`
- `{"physical": "alt+e", "repeat": false}`: press, release, then modifiers up
- `{"hold": "alt"}` until `{"hold": ""}`
- `{"type": "mark", "name": N}`, `{"type": "shortcut", "chord": "cmd+space"}`
- `paste` content takes `{"text", "html", "rtf", "png"}`
- Pointer: `{"t": 0.5, "type": "move", "key": "project-1"}` (or `"x"`, `"y"` in points) hovers an element, so a hover state snapshots with `--snap --at 1`. Also `down`, `up`, `tap`/`click`, `double-click`, `drag`, `wheel`, `scroll`, `secondary-click`, `"button": "middle"`.
- Keys and the rest: read `src/tooling/script.rs` for exact field names.

## agentd specifics

- Rendering is checked by adding a scene that builds `Session::fixture(Grid::from_vt(bytes, cols, rows))`, then `--snap` and viewing the PNG. Fixture sessions have no process, and input goes nowhere.
- Real-process behaviour is tested through `Session::spawn` with a polling `wait_for_line`.
- Write snapshots to the session scratchpad, not the repo.

## What headless cannot prove

Say so explicitly when a change depends on any of these:

- Native menu key equivalents, and whether ⌘C or ⌘V reach `on_key`.
- Real IME behaviour: winit's `interpretKeyEvents`, dead keys, candidate window placement.
- OS clipboard round trips (headless uses a memory clipboard).
- Live resize feel, trackpad momentum, display-link pacing.
- Wall-clock timing against a real PTY, since headless time is virtual.

For these, run the real app and state what was and wasn't checked.

## File map (under `crates/rune/`)

| Topic | Path |
|---|---|
| Flags | `src/tooling/args.rs` |
| Script parsing | `src/tooling/script.rs`, `src/tooling/key_names.rs` |
| Runtime input | `src/runtime/input/{pointer,keys,gesture,physical}.rs` |
| Example tests | `src/runtime/tests/app_ime.rs`, `src/runtime/tests/input_clipboard.rs` |
