# Render state: reading the screen

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
- Graphemes: `graphemes_len()`, `graphemes() -> Vec<char>`, `graphemes_utf8(&mut String)`, `graphemes_buf(&mut [char])` (`read_cell` in `grid.rs` relies on it filling the base codepoint in a one-element buffer even for longer clusters).
- Width: `cell.raw_cell()?.wide()?` gives `CellWide::{Narrow, Wide, SpacerTail, SpacerHead}`. Don't render spacers.
- `Style { fg_color, bg_color, underline_color, bold, italic, faint, blink, inverse, invisible, strikethrough, overline, underline }`, `Underline::{None, Single, Double, Curly, Dotted, Dashed}`.
- Inverse and selection are left to the renderer: `session.rs` swaps colours itself.
- Not confirmed: whether `set_selection` alone marks the render state dirty. Test it. `Snapshot::set_dirty(Dirty::Full)` is available if not.

`unicode::codepoint_width(char)` and `unicode::grapheme_width(&[char]) -> (consumed, width)` match the terminal's own layout, which is useful for IME preedit.
