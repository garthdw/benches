---
name: libghostty-vt
description: libghostty-vt Rust bindings as used by agentd - threading, render state, cells and graphemes, key and mouse encoders, selection gestures, formatting, paste, scrollback, callbacks. Use when changing session.rs or anything that talks to Terminal, instead of reading the bindings source.
---

# libghostty-vt

Verified against libghostty-rs rev `768add9` (the pin in `Cargo.toml`). The API is marked unstable upstream, so if the pin has moved, check anything you rely on.
Source: `~/.cargo/git/checkouts/libghostty-rs-28fee7453bdb2b25/768add9/crates/libghostty-vt/src`.

## Threading

Every type is `!Send` and `!Sync`. agentd keeps `Terminal` and everything tied to it on one actor thread per session and talks to it over channels. Callbacks run on the thread that called `vt_write`, `reset` or `resize`, so they need no locking.

## Terminal essentials

- `Terminal::new(cols, rows)`, `vt_write(&[u8])`, `resize(cols, rows, cell_w_px, cell_h_px)` (reflows), `reset()`.
- Setup chain: `set_scrollback_max_lines(Some(n))`, `set_terminfo_name`, `set_default_fg_color`, `set_default_bg_color`, `set_default_cursor_color`, `set_default_cursor_style`, `set_default_cursor_blink`, `set_default_color_palette`.
- State: `mode(Mode::X)`, `active_screen() -> Screen::{Primary, Alternate}`, `is_mouse_tracking()`, `viewport_active()`, `mouse_shape()`, `title()`, `pwd()`, `total_rows()`, `scrollback_rows()`.
- Modes: `Mode::{BRACKETED_PASTE, ALT_SCROLL, FOCUS_EVENT, NORMAL_MOUSE, BUTTON_MOUSE, ANY_MOUSE, SGR_MOUSE, ALT_SCREEN_SAVE, ...}`.
- Callbacks (`on_*`): `pty_write, bell, render_hold, enquiry, xtversion, title_changed, pwd_changed, size, color_scheme, device_attributes, clipboard_write, clipboard_read, desktop_notification, progress_report, program_status, semantic_prompt, reset, resize_window, unknown_sequence`.
- `on_render_hold(|term, held|)`: synchronized output. Capture the finished frame when a hold begins, or a constantly redrawing program looks frozen. Holds can be ignored when live content matters, such as during scrolling or selection.

## Render state: reading the screen

`RenderState`, `RowIterator` and `CellIterator` are created once and reused.

```rust
let snapshot = state.update(terminal)?;
if matches!(snapshot.dirty()?, Dirty::Clean) { return None; }
let colors = snapshot.colors()?;  // background, foreground, cursor: Option, palette
let mut rows = row_iter.update(&snapshot)?;
while let Some(row) = rows.next() {
    let mut cells = cell_iter.update(row)?;
    while let Some(cell) = cells.next() { /* ... */ }
}
snapshot.clean()?;
```

- Snapshot: `dirty() cols() rows() colors() cursor() cursor_visible() cursor_blinking() cursor_visual_style() cursor_viewport() cursor_password_input() set_dirty(Dirty)`.
- `Dirty::{Clean, Partial, Full}`.
- `CursorViewport { x, y, at_wide_tail }`, `None` when off-viewport. `CursorVisualStyle::{Bar, Block, Underline, BlockHollow}`.
- Row: `dirty() id() viewport_y() raw_row() selection() next_dirty()`.
- Cell: `fg_color() -> Option`, `bg_color() -> Option`, `style()`, `has_styling()`, `is_selected()`, `raw_cell()`.
- Graphemes: `graphemes_len()`, `graphemes() -> Vec<char>`, `graphemes_utf8(&mut String)`, `graphemes_buf(&mut [char])` (M1 relies on it filling the base codepoint in a one-element buffer even for longer clusters).
- Width: `cell.raw_cell()?.wide()?` gives `CellWide::{Narrow, Wide, SpacerTail, SpacerHead}`. Don't render spacers.
- `Style { fg_color, bg_color, underline_color, bold, italic, faint, blink, inverse, invisible, strikethrough, overline, underline }`, `Underline::{None, Single, Double, Curly, Dotted, Dashed}`.
- Inverse and selection are left to the renderer: `session.rs` swaps colours itself.
- Not confirmed: whether `set_selection` alone marks the render state dirty. Test it. `Snapshot::set_dirty(Dirty::Full)` is available if not.

`unicode::codepoint_width(char)` and `unicode::grapheme_width(&[char]) -> (consumed, width)` match the terminal's own layout, which is useful for IME preedit.

## Keys

`key::Encoder` and `key::Event`, both reused.

```rust
event.set_action(key::Action::Press).set_key(k).set_mods(m).set_consumed_mods(c)
     .set_unshifted_codepoint(u).set_utf8(text);
encoder.set_options_from_terminal(&terminal).encode_to_vec(&event, &mut out)?;
```

- `set_options_from_terminal` **resets** `macos_option_as_alt` to false. Call `set_macos_option_as_alt(OptionAsAlt::{False, True, Left, Right})` after it.
- `set_utf8`: the unmodified text before Ctrl or Meta transforms. Never C0 controls or macOS function-key PUA; pass `None` and let the logical key decide.
- `set_composing(bool)` exists for composition sequences.

## Mouse

`mouse::Encoder` and `mouse::Event`.

```rust
encoder.set_options_from_terminal(&t).set_size(EncoderSize {
    screen_width, screen_height, cell_width, cell_height,   // cell sizes must be non-zero
    padding_top, padding_bottom, padding_right, padding_left,
})?.set_any_button_pressed(held).set_track_last_cell(true);
event.set_action(Action::Press /* Release, Motion */).set_button(Some(Button::Left)).set_mods(m)
     .set_position(Position { x, y });  // surface pixels
encoder.encode_to_vec(&event, &mut out)?;
```

`Button::{Left, Right, Middle, Four, Five, Six, ...}`; Four and Five are wheel up and down. Motion needs no button. Check the `Action` variant name for motion in `mouse.rs` when first used.

## Selection

A `selection::gesture::Gesture` per terminal interprets pointer streams. Reusable events: `PressEvent`, `DragEvent`, `ReleaseEvent`, `AutoscrollTickEvent`, `DeepPressEvent`. All use surface-pixel positions and a `GridRef` from `terminal.grid_ref(Point::Viewport(PointCoordinate { x, y }))`.

- Press: `set_position(x, y)?`, `set_time(dur)?`, `set_repeat_interval(dur)?`, `set_repeat_distance(px)?`, then `apply(&mut gesture, &terminal, grid_ref)? -> Option<Selection>`. **Without `set_time` only single-click works.** Default behaviours: cell, word, line.
- Drag: `set_position`, `set_rectangle`, then `apply(gesture, terminal, grid_ref, Geometry { columns, cell_width, padding_left, screen_height })`.
- Release: `apply(gesture, terminal, Option<GridRef>)`, returns nothing.
- Autoscroll: poll `gesture.autoscroll(&terminal)? -> Autoscroll::{None, Up, Down}` and drive `AutoscrollTickEvent::apply(gesture, terminal, viewport: PointCoordinate, geometry)` on a timer.
- Install results with `terminal.set_selection(selection.as_ref())?`, which takes `&self`.
- Selections and `GridRef`s are only valid until the next mutating terminal call. Use `track_grid_ref` to outlive that.
- Dropping a `Gesture` leaks tracked refs until the terminal drops; `gesture.reset(&terminal)` reclaims them.
- Also: `select_all`, `select_line`, `select_word`, `select_output`.

Copying:

```rust
let Some(mut f) = Formatter::new_selection(&terminal)? else { return };
f.set_format(fmt::Format::Plain)?.set_unwrap(true)?;  // trim is already on, unwrap is off by default
let bytes = f.format_alloc(None)?;                    // Bytes derefs to [u8]
```

## Paste

```rust
let mimes = [ClipboardMime::new(b"text/plain")];
terminal.paste(paste::Options::new().with_allow_unsafe(allow), &mimes,
    |_mime, out: &mut dyn Write| out.write_all(text))?;
```

Applies bracketed paste, control-byte stripping and newline conversion from terminal state, and writes through `on_pty_write`, which must be installed.

Without `with_allow_unsafe`, text that could inject commands returns `Error::Rejected` and writes nothing: the intended flow is confirm, then retry. Use `.with_source(paste::Source::...)` for non-clipboard text, such as IME commit.

## Scrollback

`terminal.scroll_viewport(ScrollViewport::{Top, Bottom, Delta(isize), Row(usize), DeltaPrompt(isize)})`; negative `Delta` is up. `scrollbar()` has no change notification, so poll and diff per frame or write batch.

## Focus events

`focus::Event` has `.encode(&mut [u8]) -> Result<usize>`. Send only when `Mode::FOCUS_EVENT` is on.

## Build

Builds Ghostty from source through Zig (0.17.x on `PATH`) at a commit pinned in `libghostty-vt-sys/build.rs`. `GHOSTTY_SOURCE_DIR` uses a local checkout.

## File map (under `crates/`)

| Topic | Path |
|---|---|
| Terminal, modes, callbacks | `libghostty-vt/src/terminal.rs` |
| Render iterators | `libghostty-vt/src/render.rs` |
| Cells, `GridRef`, `Screen` | `libghostty-vt/src/screen.rs` |
| Selection | `libghostty-vt/src/selection.rs`, `selection/gesture.rs` |
| Encoders | `libghostty-vt/src/key.rs`, `mouse.rs`, `focus.rs` |
| Paste, formatting | `libghostty-vt/src/paste.rs`, `fmt.rs` |
| Raw FFI with docs | `libghostty-vt-sys/src/bindings.rs` |
| Complete small terminal | `../example/ghostling_rs/src/main.rs` |
