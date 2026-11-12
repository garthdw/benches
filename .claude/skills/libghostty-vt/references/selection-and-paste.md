# Selection, copying and paste

## Selection

A `selection::gesture::Gesture` per terminal interprets pointer streams. Reusable events: `PressEvent`, `DragEvent`, `ReleaseEvent`, `AutoscrollTickEvent`, `DeepPressEvent`. All use surface-pixel positions and a `GridRef` from `terminal.grid_ref(point::Point::Viewport(point::Coordinate { x, y }))`.

- Press: `set_position(x, y)?`, `set_time(dur)?`, `set_repeat_interval(dur)?`, `set_repeat_distance(px)?`, then `apply(&mut gesture, &terminal, grid_ref)? -> Option<Selection>`. **Without `set_time` only single-click works.** Default behaviours: cell, word, line.
- Drag: `set_position`, `set_rectangle`, then `apply(gesture, terminal, grid_ref, Geometry { columns, cell_width, padding_left, screen_height })`.
- Release: `apply(gesture, terminal, Option<GridRef>)`, returns nothing.
- Autoscroll: poll `gesture.autoscroll(&terminal)? -> Autoscroll::{None, Up, Down}` and drive `AutoscrollTickEvent::apply(gesture, terminal, viewport: point::Coordinate, geometry)` on a timer.
- Install results with `terminal.set_selection(selection.as_ref())?`, which takes `&self`.
- Selections and `GridRef`s are only valid until the next mutating terminal call. Use `track_grid_ref` to outlive that.
- Dropping a `Gesture` leaks tracked refs until the terminal drops; `gesture.reset(&terminal)` reclaims them.
- Also: `select_all`, `select_line`, `select_word`, `select_output`.

## Copying

```rust
let Some(mut f) = Formatter::new_selection(&terminal)? else { return };
f.set_format(fmt::Format::Plain)?.set_unwrap(true)?;  // trim is already on, unwrap is off by default
let bytes = f.format_alloc(None)?;                    // Bytes derefs to [u8]
```

## Paste

```rust
let mimes = [effect::ClipboardMime::new(b"text/plain")];
terminal.paste(paste::Options::new().with_allow_unsafe(allow), &mimes,
    |_mime, out: &mut dyn Write| out.write_all(text))?;
```

Applies bracketed paste, control-byte stripping and newline conversion from terminal state, and writes through `on_pty_write`, which must be installed.

Without `with_allow_unsafe`, text that could inject commands returns `Error::Rejected` and writes nothing: the intended flow is confirm, then retry. Use `.with_source(paste::Source::...)` for non-clipboard text, such as IME commit.
