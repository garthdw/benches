---
name: rune-testing
description: Testing and verifying Rune apps in Benches - the headless Runtime harness, --snap/--tree/--script flags, named scenes, and what headless runs cannot prove. Use when writing UI tests or checking a rendering or input change, instead of reading the Rune source.
---

# Testing Rune apps

Verified against Rune rev `7dfaa8f`. If the pin has moved, check anything you rely on.

## Benches specifics

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

## References

| Task | Reference |
|---|---|
| Unit tests: `Runtime::new`, driving pointer, wheel, key and IME input, inspecting elements | [references/runtime-harness.md](references/runtime-harness.md) |
| `--snap`, `--scene`, `--tree`, frame dumps and `--script` event forms | [references/headless-cli.md](references/headless-cli.md) |

## File map (under `crates/rune/`)

| Topic | Path |
|---|---|
| Flags | `src/tooling/args.rs` |
| Script parsing | `src/tooling/script.rs`, `src/tooling/key_names.rs` |
| Runtime input | `src/runtime/input/{pointer,keys,gesture,physical}.rs` |
| Example tests | `src/runtime/tests/app_ime.rs`, `src/runtime/tests/input_clipboard.rs` |
