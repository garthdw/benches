# Building views

```rust
col().w_full().h_full().fill(t.bg)
    .child(row().w_full().pl(cx.titlebar_inset() + 8.0).pr(8.0).py(6.0).child(chrome))
    .child(effect(TERMINAL_MARK).key("terminal").w_full().grow(1.0).min_h(0.0))
```

- Constructors: `col()`, `row()`, `el()`, `text(..)`, `effect(mark)`, `text_input(&TextEdit, TextStyle)`.
- Builders: `.key .role .label .child .children .w .h .size .w_full .h_full .grow .min_h .px .py .pl .pr .radius .fill .cursor(Cursor::Text) .absolute(left, top) .pin(l, t, r, b) .scroll(ScrollSpec{..}) .clip_soft(r)`.
- Theme: `rune::tok::theme(dark)` has `.bg`, `.text`, `.control_off` and more.
- `Cx`: `titlebar_inset() chrome_insets() env() system_dark() accent() caret(key) layout_size`.
- Handlers on `El`: `on_press`, `on_press_tx`, `on_double_press`, `on_drag`, `on_key`, `on_drop_files`, `on_paste`, `on_secondary_press[_local]`, `on_pointer_move[_local]`, `on_hover_change`, `on_wheel`, `on_layout[_tx]`, `on_drawn_layout[_tx]`. Handlers that take `S` are closures: `|s: &mut State, ..|`.

Performance rules from Rune's own guide: use `Cx::layout_size` rather than whole rects, so scrolling doesn't rebuild the view; use `virtual_rows` or `memo` for long lists; bound every cache in entries and bytes.
