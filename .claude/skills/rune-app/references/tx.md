# Tx

- Timers: `tx.wake_after(Duration)`, `tx.wake_at(clock_secs)`, `tx.clock()`. Wakes schedule a frame, not a view commit, and all due ones collapse into one poll. `wake_after(Duration::ZERO)` from `poll` asks for the next frame (paced by the display link on macOS; unpaced or hidden, it waits about a frame), so a repeating poll can't spin. A timed state stores its due time and `poll` checks `clock >= due`. `tx.request_poll()` runs `poll` again within this frame (from a handler or `poll`), or on the next turn from outside a frame.
- `tx.live_only()`: this handler changed only live values, so skip the view re-run. Debug builds check the claim.
- `tx.window_visible()`: false while the window is hidden, minimised or covered whole. Each change wakes `poll` once.
- `tx.instant()`: land this change without animation. `tx.focus(key, ring)`: focus an element once on screen; `tx.return_focus(key, ring)` hands focus back to a menu or popover's trigger; `tx.blur()`: drop focus after the view rebuilds, so keys go to `on_key` again; `tx.by_keys()`: a key (or Return/Space on a control) brought this change; `tx.focused()` works in every handler.
- `tx.scroll_key_center_at(key, center_y)`: put the keyed element's (or `TextHighlight`'s) midpoint `center_y` points below the viewport top, snapped to device pixels, instantly.
- `tx.close_window()` runs `on_close`; `tx.quit()` runs every window's `on_quit` and no `on_close`; `tx.relaunch()`: quit, then start again with the same args.
- Input accessors and clipboard are in `rune-input`.
