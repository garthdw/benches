# Terminal essentials

- `Terminal::new(cols, rows)`, `vt_write(&[u8])`, `resize(cols, rows, cell_w_px, cell_h_px)` (reflows), `reset()`.
- Setup chain: `set_scrollback_max_lines(Some(n))`, `set_terminfo_name`, `set_default_fg_color`, `set_default_bg_color`, `set_default_cursor_color`, `set_default_cursor_style`, `set_default_cursor_blink`, `set_default_color_palette`.
- State: `mode(Mode::X)`, `active_screen() -> Screen::{Primary, Alternate}`, `is_mouse_tracking()`, `viewport_active()`, `mouse_shape()`, `title()`, `pwd()`, `total_rows()`, `scrollback_rows()`.
- Modes: `mode::Mode::{BRACKETED_PASTE, ALT_SCROLL, FOCUS_EVENT, NORMAL_MOUSE, BUTTON_MOUSE, ANY_MOUSE, SGR_MOUSE, ALT_SCREEN_SAVE, ...}`.
- Callbacks (`on_*`): `pty_write, bell, render_hold, enquiry, xtversion, title_changed, pwd_changed, size, color_scheme, device_attributes, clipboard_write, clipboard_read, desktop_notification, progress_report, program_status, semantic_prompt, reset, resize_window, unknown_sequence`.
- `on_render_hold(|term, held|)`: synchronized output. Capture the finished frame when a hold begins, or a constantly redrawing program looks frozen. Holds can be ignored when live content matters, such as during scrolling or selection.

## Scrollback

`terminal.scroll_viewport(ScrollViewport::{Top, Bottom, Delta(isize), Row(usize), DeltaPrompt(isize)})`; negative `Delta` is up. `scrollbar()` has no change notification, so poll and diff per frame or write batch.

## Build

Builds Ghostty from source through Zig (0.17.x on `PATH`) at a commit pinned in `libghostty-vt-sys/build.rs`. `GHOSTTY_SOURCE_DIR` uses a local checkout.
