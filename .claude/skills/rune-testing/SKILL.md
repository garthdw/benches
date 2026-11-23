---
name: rune-testing
description: Testing and verifying Rune apps in Benches - the headless Runtime harness, --snap/--tree/--script/--chrome flags, named scenes, --window-script replays in a real window, and what headless runs cannot prove. Use when writing UI tests or checking a rendering or input change, instead of reading the Rune source.
---

# Testing Rune apps

Verified against Rune rev `b5c4fa6`. If the pin has moved, check anything you rely on.

## Benches specifics

- Rendering is checked by adding a scene that builds `Session::fixture(Grid::from_vt(bytes, cols, rows))`, then `--snap` and viewing the PNG. Fixture sessions have no process, and input goes nowhere.
- Real-process behaviour is tested through `Session::spawn` with a polling `wait_for_line`.
- App tests build their runtime with `src/test_support.rs`: `runtime_with`, `workspace_runtime`, `palette_runtime`, `press`, `press_command`, `click`, `hover`, `settle`, `frame_until`, `stub_shell`, `wait_until`. Core's `test_support` (feature `test-support`) is terminal-level (`scrolled_terminal`, `pointer_at`, `wheel_by`) and has no `Runtime`.
- Write snapshots to the session scratchpad, not the repo.

## What headless cannot prove

Say so explicitly when a change depends on any of these:

- Native menu key equivalents, and whether ⌘C or ⌘V reach `on_key`.
- Real IME behaviour: winit's `interpretKeyEvents`, dead keys, candidate window placement.
- OS clipboard round trips (headless runs never touch the OS clipboard; a test can opt into `rune::clipboard::memory()`).
- Live resize feel, trackpad momentum, display-link pacing.
- Wall-clock timing against a real PTY, since headless time is virtual.

For these, run the real app and state what was and wasn't checked.

`--window-script` replays script input in a real window on the wall clock, so it can exercise real PTY timing and frame pacing. It injects events where the system's would arrive, so it proves nothing about menu key equivalents, IME, the OS clipboard or the system pointer path. A real window reads the real config and saved layout, and restores projects whose tabs may run `claude`: isolate it (see [references/headless-cli.md](references/headless-cli.md)).

## References

| Task | Reference |
|---|---|
| Unit tests: `Runtime::new`, driving pointer, wheel, key and IME input, inspecting elements | [references/runtime-harness.md](references/runtime-harness.md) |
| `--snap`, `--scene`, `--chrome`, `--tree`, frame dumps, `--script` event forms and `--window-script` | [references/headless-cli.md](references/headless-cli.md) |

## File map (under `crates/rune/`)

| Topic | Path |
|---|---|
| Flags | `src/tooling/args.rs` |
| Headless run | `src/tooling/headless.rs`, `src/tooling/headless/{app,frame}.rs` |
| Script parsing | `src/tooling/script.rs`, `src/tooling/key_names.rs` (names from `src/input/key_table.rs`) |
| Script replay | `src/tooling.rs` (`script_point`, `script_input`), `src/window/host/window_script.rs` |
| Public `Runtime` methods | `src/runtime/api.rs`, `src/runtime/input/api.rs`, forwarding to the non-generic `RuntimeCore` |
| App hooks on a runtime | `src/runtime/driver.rs` (`RuntimeHooks`), `src/app/config.rs` (`Runtime::configure`) |
| Runtime input | `src/runtime/input/{pointer,keys,gesture,physical}.rs` |
| Example tests, public API | `tests/integration/*.rs` (one `integration` binary), e.g. `hover_delays.rs`, `selection_copy_breaks.rs` |
| Example tests, in-crate | `src/runtime/tests/app_ime.rs`, `src/runtime/tests/input_clipboard.rs`; they set the crate-private `rt.hooks`, which Benches can't |
