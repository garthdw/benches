# Pointer and wheel

## Pointer

- `El::on_drag(|s: &mut S, d: &mut Drag, tx|)`. `Drag`: `phase` (`Phase::{Down, Move, Up}`), `x, y, w, h, start_x, start_y` in element-local points, `dx, dy` in window points, `data` scratch, `clicks` (1, 2, 3...), `cancelled()` on `Up`.
- `Drag::window_point()` gives window points. Primary only, unless `.pointer_buttons([PointerButton::Middle, ..])`. `tx.button()` says which. Secondary always goes to `on_secondary_press[_local]`, which is press only with no release event.
- `on_press` runs on release by default. `.press_on_down()` runs it as the primary mouse button goes down and makes that release run nothing (a key or accessibility Invoke still runs it on the click), as Rune's menu buttons and `rune-kit` tab strips act on press-down.
- `.on_pointer_move_local(|s, PointerEvent { x, y, window_x, window_y, .. }, tx|)`. Call `tx.live_only()` if the view reads nothing that changed.
- `.on_hover_change(|s, inside: bool, tx|)` fires on enter and leave of the rect, descendants included, and takes no hits; `.hover_delays(enter, leave)` delays it.
- `tx.mods()` works in all of these. Multiply points by the scale for device pixels.

## Wheel

`El::on_wheel(|s, WheelEvent, tx| -> bool)`, return true to take it. Once a handler takes a gesture's first delta it gets the rest, momentum included, wherever the pointer goes.

`WheelEvent { dx, dy, x, y, mods, phase, momentum, precise, notch }` (`#[non_exhaustive]`). Deltas are points, and positive `dy` moves content down (scrolling toward older lines). Element-local `x, y`. `notch` marks whole mouse-wheel notches on Windows and Linux; it is always false on macOS.

Who hears it: the chain starts at the topmost thing under the pointer that takes input, is a scroll view, or is drawn over others (an anchored overlay, a `raise()`d panel, a sticky element), and walks up its ancestors to the nearest scroll view. So a wheel over a popover or raised panel never reaches the terminal's handler beneath it, even if the panel has no handler.

A delta no handler takes scrolls a viewport, 1:1 on macOS. A run of deltas starts on the innermost viewport there with room in its direction and keeps to it, even at its end, until the wheel rests (0.5 s) or the pointer moves 10 points; the next run can start on a viewport around it, never on one that only lies beneath (`runtime/scroll/x.rs`).
