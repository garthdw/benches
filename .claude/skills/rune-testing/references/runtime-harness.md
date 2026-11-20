# Unit tests with `Runtime`

No window or GPU is needed.

```rust
let env = Env { system_dark: true, scale: 2.0 };
let mut rt = Runtime::new(State::default(), view, (960.0, 640.0), env);
rt.on_key = Some(|_, _, _| true);   // Runtime::new does not wire the AppSpec hooks
rt.frame(0.0);                      // frame(dt) advances the clock by dt, not to dt
let (x, y) = rt.find("chrome").unwrap();
rt.pointer_down(x, y);
rt.pointer_up(x, y);
rt.frame(1.0 / 60.0);               // input is queued: run a frame before asserting
rt.key(KeyEvent { key: Key::Tab, mods: Mods::default() });
rt.frame(1.0 / 60.0);
```

`Runtime<S>` is a thin `#[repr(C)]` wrapper over the non-generic `RuntimeCore`. Only `state`, `poll` and `on_key` are public fields after it. `App::on_focus`, `on_modifiers`, `on_visibility` and `on_ime` go into crate-private `hooks`, filled only by `Runtime::configure`, which is `pub(crate)`. So with `Runtime::new`, `window_focus(false)` and modifier changes through `physical_input` reach no app hook. Benches tests call the hook directly: `runtime.update(|state, tx| on_window_focus(state, false, tx))`, `runtime.update(|state, _| modifiers_changed(state, mods))`, or `on_ime(&mut state, &ime, &mut tx)`. The public `App::headless_runtime(size, env)` does apply every hook, but takes its state from the `App`'s `init` and runs `setup`, so `test_support::runtime_with` doesn't use it.

Driving input (points, window space):

- Pointer: `pointer_move, pointer_down, pointer_up, pointer_leave, button_down(x, y, PointerButton), button_up`.
- Wheel: `wheel(x, y, dy)`, or `wheel_event(WheelEvent::new(dx, dy).at(x, y).mods(m).phase(p).precise(true))`.
- Keys: `key, repeat_key, key_up, physical_input(PhysicalInput)`.
- Text: `ime(Ime), finish_composition(), paste(text), paste_content(content)`. `finish_composition` is what window deactivation does: an open composition becomes text (see `rune-input`).
- Gestures: `magnify_event, rotate_event, smart_magnify`.
- Window: `window_focus(bool)` reaches `El::on_window_focus` handlers, but not `App::on_focus` (above).
- State from outside a handler: `update(|state, tx| …)` runs now and the next frame commits it; after mutating `rt.state` directly, call `invalidate()`.
- Clipboard: a test runtime never touches the OS clipboard; a `tx.copy` is left in `rt.copy_request`. `let clipboard = rune::clipboard::memory(); rt.use_clipboard(&clipboard);` makes copies land there (`clipboard.text()`) and answers reads from `clipboard.set_text(…)`.

Inspect with:

- `find(key) -> Option<(f32, f32)>`: element centre in points, `None` once exiting.
- `presented(key) -> Option<([x, y, w, h], opacity)>`: drawn rect as of the last frame, exiting elements included.
- `pointer()`, `cursor()`, `pressing()`, `has_focus()`, `has_pending_input()`, `wake_deadline()`.
- `stats.commits`: view re-runs. Unchanged across a `frame` means a paint-only frame. `live_pending()` is true while a live source has an update waiting.
- `file_dialog()` shows the pending `Tx::open_file`/`save_file`/`open_folder` request; `resolve_file_dialog(Some(path))` or `None` (cancel) answers it, and the callback runs on the next frame. Errors if none is open.

`pointer_move` then one `frame` is enough for hover state. `El::hover_delays` elements also need frames past their delay.

Function-pointer hooks can't capture, so count calls with a `static` atomic, as the `app.rs` tests do.
