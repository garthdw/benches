# Pointer and wheel

## Pointer

- `El::on_drag(|s: &mut S, d: &mut Drag, tx|)`. `Drag`: `phase` (`Phase::{Down, Move, Up}`), `x, y, w, h, start_x, start_y` in element-local points, `dx, dy` in window points, `data` scratch, `clicks` (1, 2, 3...), `cancelled()` on `Up`.
- `Drag::window_point()` gives window points. Primary only, unless `.pointer_buttons([PointerButton::Middle, ..])`. `tx.button()` says which. Secondary always goes to `on_secondary_press[_local]`, which is press only with no release event.
- `.on_pointer_move_local(|s, PointerEvent { x, y, window_x, window_y, .. }, tx|)`. Call `tx.live_only()` if the view reads nothing that changed.
- `.on_hover_change(|s, inside: bool, tx|)` fires on enter and leave of the rect, descendants included, and takes no hits; `.hover_delays(enter, leave)` delays it.
- `tx.mods()` works in all of these. Multiply points by the scale for device pixels.

## Wheel

`El::on_wheel(|s, WheelEvent, tx| -> bool)`, return true to take it. Once a handler takes a gesture's first delta it gets the rest, momentum included.

`WheelEvent { dx, dy, x, y, mods, phase, momentum, precise }`. Deltas are points, and positive `dy` moves content down (scrolling toward older lines). Element-local `x, y`.
