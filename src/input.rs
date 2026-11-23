use libghostty_vt::key::{Key as Vt, Mods as VtMods};
use libghostty_vt::mouse;
use rune::{Key, KeyEvent, KeyPlatform, Mods, Phase, PhysicalKeyCode as Code, Tx};

use crate::app::State;
use crate::session::Session;
use benches_core::grid::Grid;
use benches_core::terminal_input::{KeyInput, PointerInput, PointerPhase};

pub fn vt_mods(mods: Mods) -> VtMods {
    let mut out = VtMods::empty();
    out.set(VtMods::SHIFT, mods.shift);
    out.set(VtMods::ALT, mods.alt);
    out.set(VtMods::CTRL, mods.ctrl);
    out.set(VtMods::SUPER, mods.logo);
    out
}

fn vt_key(code: Code) -> Vt {
    match code {
        Code::KeyA => Vt::A,
        Code::KeyB => Vt::B,
        Code::KeyC => Vt::C,
        Code::KeyD => Vt::D,
        Code::KeyE => Vt::E,
        Code::KeyF => Vt::F,
        Code::KeyG => Vt::G,
        Code::KeyH => Vt::H,
        Code::KeyI => Vt::I,
        Code::KeyJ => Vt::J,
        Code::KeyK => Vt::K,
        Code::KeyL => Vt::L,
        Code::KeyM => Vt::M,
        Code::KeyN => Vt::N,
        Code::KeyO => Vt::O,
        Code::KeyP => Vt::P,
        Code::KeyQ => Vt::Q,
        Code::KeyR => Vt::R,
        Code::KeyS => Vt::S,
        Code::KeyT => Vt::T,
        Code::KeyU => Vt::U,
        Code::KeyV => Vt::V,
        Code::KeyW => Vt::W,
        Code::KeyX => Vt::X,
        Code::KeyY => Vt::Y,
        Code::KeyZ => Vt::Z,
        Code::Digit0 => Vt::Digit0,
        Code::Digit1 => Vt::Digit1,
        Code::Digit2 => Vt::Digit2,
        Code::Digit3 => Vt::Digit3,
        Code::Digit4 => Vt::Digit4,
        Code::Digit5 => Vt::Digit5,
        Code::Digit6 => Vt::Digit6,
        Code::Digit7 => Vt::Digit7,
        Code::Digit8 => Vt::Digit8,
        Code::Digit9 => Vt::Digit9,
        Code::Space => Vt::Space,
        Code::Minus => Vt::Minus,
        Code::Equal => Vt::Equal,
        Code::BracketLeft => Vt::BracketLeft,
        Code::BracketRight => Vt::BracketRight,
        Code::Backslash => Vt::Backslash,
        Code::Semicolon => Vt::Semicolon,
        Code::Quote => Vt::Quote,
        Code::Comma => Vt::Comma,
        Code::Period => Vt::Period,
        Code::Slash => Vt::Slash,
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
    KeyInput {
        key: physical.map_or(Vt::Unidentified, vt_key),
        mods: vt_mods(mods),
        consumed,
        text: (!mods.ctrl && !mods.logo).then(|| text.to_owned()),
        unshifted: base.unwrap_or('\0'),
    }
}

/// Ghostty's default macOS "natural text editing" keybinds, which send fixed bytes instead
/// of an encoded key.
pub(crate) fn line_editing(event: &KeyEvent, platform: KeyPlatform) -> Option<KeyInput> {
    if platform != KeyPlatform::Mac {
        return None;
    }
    let Mods {
        shift,
        cmd,
        alt,
        ctrl,
        ..
    } = event.mods;
    let text = match (&event.key, cmd, alt, shift || ctrl) {
        (Key::Backspace, true, false, false) => "\x15",
        (Key::Left, true, false, false) => "\x01",
        (Key::Right, true, false, false) => "\x05",
        (Key::Left, false, true, false) => "\x1bb",
        (Key::Right, false, true, false) => "\x1bf",
        _ => return None,
    };
    Some(KeyInput {
        key: Vt::Unidentified,
        mods: VtMods::empty(),
        consumed: VtMods::empty(),
        text: Some(text.to_owned()),
        unshifted: '\0',
    })
}

/// `None` for keys with no terminal meaning. The app's own shortcuts are taken before this,
/// so any ⌘ chord that gets here goes to the terminal with super set and the encoder decides.
pub fn map_key(event: &KeyEvent, physical: Option<Code>, base: Option<char>) -> Option<KeyInput> {
    if let Some(input) = line_editing(event, KeyPlatform::current()) {
        return Some(input);
    }
    let named = match &event.key {
        Key::Text(text) => return Some(map_text(text, event.mods, physical, base)),
        Key::Command(character) => {
            return Some(map_text(&character.to_string(), event.mods, physical, base));
        }
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
            Vt::F1,
            Vt::F2,
            Vt::F3,
            Vt::F4,
            Vt::F5,
            Vt::F6,
            Vt::F7,
            Vt::F8,
            Vt::F9,
            Vt::F10,
            Vt::F11,
            Vt::F12,
        ]
        .get(usize::from(*n).checked_sub(1)?)?,
        _ => return None,
    };
    Some(KeyInput {
        key: named,
        mods: vt_mods(event.mods),
        consumed: VtMods::empty(),
        text: None,
        unshifted: '\0',
    })
}

pub(crate) fn send_pointer(
    session: Option<&Session>,
    phase: PointerPhase,
    button: Option<mouse::Button>,
    x: f32,
    y: f32,
    mods: Mods,
) {
    if let Some(session) = session {
        session.pointer(PointerInput {
            phase,
            button,
            x,
            y,
            mods: vt_mods(mods),
        });
    }
}

/// The cursor cell in points within the terminal surface, or `None` before the first layout.
pub(crate) fn caret_rect(grid: &Grid) -> Option<[f32; 4]> {
    let (x, y) = grid.cursor?;
    let metrics = grid.metrics;
    if metrics.cell_width == 0 {
        return None;
    }
    let origin = metrics.origin([0.0; 4]);
    Some(metrics.to_points(metrics.cell_rect(origin, x, y)))
}

pub(crate) fn pointer_phase(phase: Phase) -> PointerPhase {
    match phase {
        Phase::Down => PointerPhase::Press,
        Phase::Move => PointerPhase::Drag,
        Phase::Up => PointerPhase::Release,
    }
}

/// Pressing the drawer's terminal gives it the keys, and pressing the tab's takes them back.
/// The overlay (`None`) always has them while shown.
pub(crate) fn focus_on_press(
    state: &mut State,
    drawer: Option<bool>,
    phase: PointerPhase,
    tx: &mut Tx,
) {
    if phase == PointerPhase::Press
        && let Some(drawer) = drawer
        && state
            .workspace
            .project()
            .is_some_and(|project| project.drawer_focused != drawer)
    {
        state.workspace.focus_drawer(drawer);
    } else {
        tx.live_only();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use benches_core::grid::new_terminal;
    use benches_core::terminal_input::TerminalInput;

    #[test]
    fn ctrl_c_is_the_physical_c_key_without_text() {
        let input = map_text(
            "c",
            Mods {
                ctrl: true,
                ..Mods::default()
            },
            Some(Code::KeyC),
            Some('c'),
        );
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
        let input = map_text(
            "!",
            Mods {
                shift: true,
                ..Mods::default()
            },
            Some(Code::Digit1),
            Some('1'),
        );
        assert_eq!((input.key, input.unshifted), (Vt::Digit1, '1'));
        assert_eq!(input.text.as_deref(), Some("!"));
        assert!(input.consumed.contains(VtMods::SHIFT));
    }

    #[test]
    fn option_stays_a_modifier_rather_than_producing_text() {
        let input = map_text(
            "∫",
            Mods {
                alt: true,
                ..Mods::default()
            },
            Some(Code::KeyB),
            Some('b'),
        );
        assert!(input.mods.contains(VtMods::ALT));
        assert!(!input.consumed.contains(VtMods::ALT));
    }

    #[test]
    fn synthetic_text_still_reaches_the_terminal() {
        let input = map_text("é", Mods::default(), None, None);
        assert_eq!(
            (input.key, input.text.as_deref()),
            (Vt::Unidentified, Some("é"))
        );
    }

    #[test]
    fn tab_and_escape_are_named() {
        let named = |key| {
            map_key(
                &KeyEvent {
                    key,
                    mods: Mods::default(),
                },
                None,
                None,
            )
            .map(|input| input.key)
        };
        assert_eq!(named(Key::Tab), Some(Vt::Tab));
        assert_eq!(named(Key::Escape), Some(Vt::Escape));
    }

    fn command_key(key: Key, shift: bool) -> KeyInput {
        let mods = Mods {
            cmd: true,
            logo: true,
            shift,
            ..Mods::default()
        };
        map_key(&KeyEvent { key, mods }, Some(Code::KeyK), Some('k')).unwrap()
    }

    #[test]
    fn a_command_chord_reaches_the_terminal_with_super_and_no_text() {
        let input = command_key(Key::Command('k'), false);
        assert_eq!((input.key, input.text, input.unshifted), (Vt::K, None, 'k'));
        assert!(input.mods.contains(VtMods::SUPER));
    }

    #[test]
    fn command_space_carries_no_text() {
        assert_eq!(command_key(Key::Text(" ".into()), false).text, None);
    }

    #[test]
    fn a_shifted_command_chord_keeps_shift_as_a_modifier() {
        let input = command_key(Key::Command('K'), true);
        assert!(input.mods.contains(VtMods::SUPER | VtMods::SHIFT));
    }

    fn line_editing_text(key: Key, mods: Mods, platform: KeyPlatform) -> Option<String> {
        line_editing(&KeyEvent { key, mods }, platform)?.text
    }

    #[test]
    fn command_chords_edit_the_line_like_ghostty_on_macos() {
        let cmd = Mods {
            cmd: true,
            logo: true,
            ..Mods::default()
        };
        let alt = Mods {
            alt: true,
            ..Mods::default()
        };
        let mac = |key, mods| line_editing_text(key, mods, KeyPlatform::Mac);
        assert_eq!(mac(Key::Backspace, cmd).as_deref(), Some("\x15"));
        assert_eq!(mac(Key::Left, cmd).as_deref(), Some("\x01"));
        assert_eq!(mac(Key::Right, cmd).as_deref(), Some("\x05"));
        assert_eq!(mac(Key::Left, alt).as_deref(), Some("\x1bb"));
        assert_eq!(mac(Key::Right, alt).as_deref(), Some("\x1bf"));
        assert_eq!(mac(Key::Left, Mods::default()), None);
        assert_eq!(line_editing_text(Key::Left, alt, KeyPlatform::Linux), None);
    }

    #[test]
    fn option_b_is_escape_b() {
        let mut terminal = new_terminal(20, 5).unwrap();
        let option = Mods {
            alt: true,
            ..Mods::default()
        };
        // Option+B types "∫" on a Mac; Alt+B types "b" elsewhere.
        let text = if KeyPlatform::current() == KeyPlatform::Mac {
            "∫"
        } else {
            "b"
        };
        let typed = map_text(text, option, Some(Code::KeyB), Some('b'));
        assert_eq!(
            TerminalInput::new()
                .unwrap()
                .key(&mut terminal, typed)
                .unwrap(),
            b"\x1bb"
        );
    }

    #[test]
    fn an_unbound_command_chord_types_nothing_in_legacy_mode() {
        let mut terminal = new_terminal(20, 5).unwrap();
        let bytes = TerminalInput::new()
            .unwrap()
            .key(&mut terminal, command_key(Key::Command('k'), false))
            .unwrap();
        assert_eq!(bytes, b"");
    }

    #[test]
    fn an_unbound_command_chord_reports_super_under_the_kitty_protocol() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[>1u");
        let bytes = TerminalInput::new()
            .unwrap()
            .key(&mut terminal, command_key(Key::Command('k'), false))
            .unwrap();
        assert_eq!(bytes, b"\x1b[107;9u");
    }
}
