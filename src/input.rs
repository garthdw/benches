use libghostty_vt::key::{Key as Vt, Mods as VtMods};
use rune::{Key, KeyEvent, Mods, PhysicalKeyCode as Code};

#[derive(Debug, PartialEq)]
pub struct KeyInput {
    pub key: Vt,
    pub mods: VtMods,
    pub consumed: VtMods,
    pub text: Option<String>,
    pub unshifted: char,
}

fn vt_mods(m: Mods) -> VtMods {
    let mut out = VtMods::empty();
    out.set(VtMods::SHIFT, m.shift);
    out.set(VtMods::ALT, m.alt);
    out.set(VtMods::CTRL, m.ctrl);
    out.set(VtMods::SUPER, m.logo);
    out
}

fn vt_key(code: Code) -> Vt {
    match code {
        Code::KeyA => Vt::A, Code::KeyB => Vt::B, Code::KeyC => Vt::C, Code::KeyD => Vt::D,
        Code::KeyE => Vt::E, Code::KeyF => Vt::F, Code::KeyG => Vt::G, Code::KeyH => Vt::H,
        Code::KeyI => Vt::I, Code::KeyJ => Vt::J, Code::KeyK => Vt::K, Code::KeyL => Vt::L,
        Code::KeyM => Vt::M, Code::KeyN => Vt::N, Code::KeyO => Vt::O, Code::KeyP => Vt::P,
        Code::KeyQ => Vt::Q, Code::KeyR => Vt::R, Code::KeyS => Vt::S, Code::KeyT => Vt::T,
        Code::KeyU => Vt::U, Code::KeyV => Vt::V, Code::KeyW => Vt::W, Code::KeyX => Vt::X,
        Code::KeyY => Vt::Y, Code::KeyZ => Vt::Z,
        Code::Digit0 => Vt::Digit0, Code::Digit1 => Vt::Digit1, Code::Digit2 => Vt::Digit2,
        Code::Digit3 => Vt::Digit3, Code::Digit4 => Vt::Digit4, Code::Digit5 => Vt::Digit5,
        Code::Digit6 => Vt::Digit6, Code::Digit7 => Vt::Digit7, Code::Digit8 => Vt::Digit8,
        Code::Digit9 => Vt::Digit9,
        Code::Space => Vt::Space, Code::Minus => Vt::Minus, Code::Equal => Vt::Equal,
        Code::BracketLeft => Vt::BracketLeft, Code::BracketRight => Vt::BracketRight,
        Code::Backslash => Vt::Backslash, Code::Semicolon => Vt::Semicolon, Code::Quote => Vt::Quote,
        Code::Comma => Vt::Comma, Code::Period => Vt::Period, Code::Slash => Vt::Slash,
        Code::Backquote => Vt::Backquote,
        _ => Vt::Unidentified,
    }
}

/// `physical` and `base` are `None` for synthetic keys, which still type their text.
pub fn map_text(text: &str, mods: Mods, physical: Option<Code>, base: Option<char>) -> KeyInput {
    let mut consumed = VtMods::empty();
    if mods.shift && base.is_some() {
        consumed |= VtMods::SHIFT;
    }
    if mods.alt && !text.is_ascii() {
        consumed |= VtMods::ALT;
    }
    KeyInput {
        key: physical.map_or(Vt::Unidentified, vt_key),
        mods: vt_mods(mods),
        consumed,
        text: (!mods.ctrl).then(|| text.to_owned()),
        unshifted: base.unwrap_or('\0'),
    }
}

/// `None` for keys the terminal shouldn't receive, such as ⌘ chords.
pub fn map_key(event: &KeyEvent, physical: Option<Code>, base: Option<char>) -> Option<KeyInput> {
    let named = match &event.key {
        Key::Text(text) => return Some(map_text(text, event.mods, physical, base)),
        Key::Enter => Vt::Enter,
        Key::Backspace => Vt::Backspace,
        Key::Delete => Vt::Delete,
        Key::Left => Vt::ArrowLeft,
        Key::Right => Vt::ArrowRight,
        Key::Up => Vt::ArrowUp,
        Key::Down => Vt::ArrowDown,
        Key::Home => Vt::Home,
        Key::End => Vt::End,
        Key::PageUp => Vt::PageUp,
        Key::PageDown => Vt::PageDown,
        Key::Insert => Vt::Insert,
        Key::Escape => Vt::Escape,
        Key::Tab => Vt::Tab,
        Key::Function(n) => *[
            Vt::F1, Vt::F2, Vt::F3, Vt::F4, Vt::F5, Vt::F6, Vt::F7, Vt::F8, Vt::F9, Vt::F10, Vt::F11, Vt::F12,
        ].get(usize::from(*n).checked_sub(1)?)?,
        _ => return None,
    };
    Some(KeyInput { key: named, mods: vt_mods(event.mods), consumed: VtMods::empty(), text: None, unshifted: '\0' })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_is_the_physical_c_key_without_text() {
        let input = map_text("c", Mods { ctrl: true, ..Mods::default() }, Some(Code::KeyC), Some('c'));
        assert_eq!((input.key, input.text), (Vt::C, None));
        assert!(input.mods.contains(VtMods::CTRL));
    }

    #[test]
    fn a_non_us_layout_keeps_the_physical_key_and_its_own_base() {
        let input = map_text("я", Mods::default(), Some(Code::KeyZ), Some('я'));
        assert_eq!((input.key, input.unshifted), (Vt::Z, 'я'));
    }

    #[test]
    fn shifted_symbols_consume_shift() {
        let input = map_text("!", Mods { shift: true, ..Mods::default() }, Some(Code::Digit1), Some('1'));
        assert_eq!((input.key, input.unshifted), (Vt::Digit1, '1'));
        assert_eq!(input.text.as_deref(), Some("!"));
        assert!(input.consumed.contains(VtMods::SHIFT));
    }

    #[test]
    fn synthetic_text_still_reaches_the_terminal() {
        let input = map_text("é", Mods::default(), None, None);
        assert_eq!((input.key, input.text.as_deref()), (Vt::Unidentified, Some("é")));
    }

    #[test]
    fn tab_and_escape_are_named_but_command_chords_are_not_for_the_terminal() {
        let named = |key| map_key(&KeyEvent { key, mods: Mods::default() }, None, None).map(|i| i.key);
        assert_eq!(named(Key::Tab), Some(Vt::Tab));
        assert_eq!(named(Key::Escape), Some(Vt::Escape));
        assert_eq!(named(Key::Command('w')), None);
    }
}
