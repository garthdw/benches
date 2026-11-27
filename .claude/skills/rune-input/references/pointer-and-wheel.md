# Pointer and wheel

## Pointer

- `El::on_drag(|s: &mut S, d: &mut Drag, tx|)`. `Drag`: `phase` (`Phase::{Down, Move, Up}`), `x, y, w, h, start_x, start_y` in element-local points, `dx, dy` in window points, `data` scratch, `clicks` (1, 2, 3...), `cancelled()` on `Up`.
- An `Up` is `cancelled()` when Escape ends the drag, the window loses focus, a `.modal` opens, or the element leaves the tree, starts exiting, turns inert or drops `on_drag`; a gone element's last handler hears it. A keyed element whose path shifts keeps its drag.
- `Drag::window_point()` gives window points, `Drag::screen_point()` desktop pixels (`None` headless and on Wayland). Primary only, unless `.pointer_buttons([PointerButton::Middle, ..])`. `tx.button()` says which. Secondary always goes to `on_secondary_press[_local]`, which is press only with no release event.
- `on_press` runs on release by default, inside the element. On an element that also has `on_drag`, a press that travelled past the click slop (4 points) runs no `on_press`, even if released back inside. `.press_on_down()` runs it as the primary mouse button goes down and makes that release run nothing (a key or accessibility Invoke still runs it on the click), as Rune's menu buttons and `rune-kit` tab strips act on press-down.
- `.on_pointer_move_local(|s, PointerEvent { x, y, window_x, window_y, .. }, tx|)`. Call `tx.live_only()` if the view reads nothing that changed. A move that changes nothing drawn and runs no handler requests no frame.
- `.on_hover_change(|s, inside: bool, tx|)` fires on enter and leave of the rect, descendants included, and is no press target; `.hover_delays(enter, leave)` delays it.
- Moves and hover go only to the topmost hit under the pointer and its ancestors. A hit is anything with a pointer handler (`on_hover_change` and `on_pointer_move` included), a field, focusable or `.hit()` element: one drawn over the terminal that isn't its descendant stops the terminal hearing moves and hover there. Handlerless elements, such as Benches' caret and preedit, block nothing.
- `.cursor(c)`: while pressed, the press target's cursor; otherwise the innermost hit target that sets one (on itself or an ancestor), so a child's cursor wins inside a parent that sets another.
- `tx.mods()` works in all of these. Multiply points by the scale for device pixels.

## Wheel

`El::on_wheel(|s, WheelEvent, tx| -> bool)`, return true to take it, false to pass it on. Once a handler takes a gesture's first delta it gets the rest, momentum included, wherever the pointer goes, until it declines or the gesture ends.

`WheelEvent { dx, dy, x, y, mods, phase, momentum, precise, notch, touch }` (`#[non_exhaustive]`). Deltas are points, and positive `dy` moves content down (scrolling toward older lines). Element-local `x, y`. `notch` marks whole mouse-wheel notches on every platform, every macOS line-delta event included. `touch` is a touchscreen's two-finger pan.

Ctrl+wheel (Ctrl and a two-finger scroll too) goes first to an `on_magnify` handler on the wheel chain, as `MagnifyEvent { wheel: true, .. }` with `delta` `1.2^notches - 1` for line-delta notches and `dy / 200` for precise deltas (wheel away from the user zooms in), on every platform. With no `on_magnify` there it is an ordinary `WheelEvent` with `mods.ctrl`, which is how a terminal with no magnify handler hears it for mouse reporting.

Who hears it: the chain starts at the topmost thing under the pointer that takes input, is a scroll view, or is drawn over others (an anchored overlay, a `raise()`d panel, a sticky element), and bubbles up through every ancestor, past scroll views, as a DOM `wheel` event does. So a wheel over a popover or raised panel never reaches the terminal's handler beneath it, even if the panel has no handler.

A delta no handler takes scrolls a viewport, 1:1 on macOS. A run of deltas starts on the innermost viewport there with room in its direction and keeps to it, even at its end, until the wheel rests (0.5 s) or the pointer moves 10 points; the next run can start on a viewport around it, never on one that only lies beneath (`runtime/scroll/x.rs`).
