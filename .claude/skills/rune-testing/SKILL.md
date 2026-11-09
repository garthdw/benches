---
name: rune-testing
description: Testing and verifying Rune apps in agentd - the headless Runtime harness, --snap/--tree/--script flags, named scenes, and what headless runs cannot prove. Use when writing UI tests or checking a rendering or input change, instead of reading the Rune source.
---

# Testing Rune apps

Verified against Rune rev `268ac7c`. If the pin has moved, check anything you rely on.

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

Inspect with `find(key) -> Option<(f32, f32)>`, `pointer()`, `cursor()`, `pressing()`, `has_focus()`, `has_pending_input()`, `wake_deadline()`.

Function-pointer hooks can't capture, so count calls with a `static` atomic, as `main.rs` does.

## Headless CLI

Rune intercepts these flags on the app binary and renders with no window. The compositor still runs.

```sh
cargo run -- --snap "$SCRATCH/out.png" --scene fixture --size 960x640 --scale 2
```

| Flag | Use |
|---|---|
| `--snap out.png` | Render offscreen at `--at SECONDS` |
| `--scene NAME`, `--list-scenes` | Start from an `AppSpec::scenes` entry |
| `--crop KEY` | Crop `--snap` to an element's key |
| `--size WxH`, `--scale N`, `--theme dark\|light` | Viewport in points, device scale (default 2), appearance |
| `--script FILE.json` | Replay timestamped input |
| `--tree out.json` | Element tree with animated values |
| `--frames N --fps 60 --start S --out DIR` | Dump frames |
| `--strip out.png --crop x,y,w,h --every K` | Tile dumped frames |
| `--trace out.jsonl` | Per-frame rects and motion |
| `--pointer`, `--keys [CORNER]` | Draw presses or keycaps on dumped frames |

`--help` on the binary prints the full list.

`--script` takes a JSON array or `{"events": [...]}`. Each event has `t` (seconds). Forms confirmed:

- `{"focus": false}`, `{"menu": "item"}`, `{"reopen": true}`
- `{"physical": "alt+e", "repeat": false}`: press, release, then modifiers up
- `{"hold": "alt"}` until `{"hold": ""}`
- `{"type": "mark", "name": N}`, `{"type": "shortcut", "chord": "cmd+space"}`
- `paste` content takes `{"text", "html", "rtf", "png"}`
- Pointer events (`move`, `down`, `up`, `tap`, `drag`) and keys: read `src/tooling/script.rs` for exact field names.

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
