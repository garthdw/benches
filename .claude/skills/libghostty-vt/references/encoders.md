# Key, mouse and focus encoders

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

## Focus events

`focus::Event` has `.encode(&mut [u8]) -> Result<usize>`. Send only when `Mode::FOCUS_EVENT` is on.
