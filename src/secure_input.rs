//! Secure keyboard entry: while on, other processes can't read keystrokes through event taps.

use benches_platform::{SECURE_INPUT_SUPPORTED, set_secure_input};

use crate::app::State;

/// The enable and disable calls are reference-counted for the whole process, so each window
/// keeps them balanced by only calling on a change and disabling on drop.
#[derive(Default)]
pub(crate) struct SecureInput {
    /// Set by the user and kept across tabs and focus changes.
    pub(crate) manual: bool,
    pub(crate) enabled: bool,
}

impl SecureInput {
    /// Returns whether `enabled` changed.
    pub(crate) fn set(&mut self, wanted: bool) -> bool {
        if !SECURE_INPUT_SUPPORTED || self.enabled == wanted {
            return false;
        }
        if let Err(error) = set_secure_input(wanted) {
            eprintln!("benches: secure event input: {error}");
            return false;
        }
        self.enabled = wanted;
        true
    }
}

impl Drop for SecureInput {
    fn drop(&mut self) {
        self.set(false);
    }
}

impl State {
    /// Secure input is on for the user's choice or a password prompt, only while this window has focus.
    fn secure_input_wanted(&self) -> bool {
        let password_prompt = self.config.auto_secure_input
            && self
                .session()
                .is_some_and(|session| session.grid().password_input);
        self.workspace.window_focused && (self.secure_input.manual || password_prompt)
    }

    pub(crate) fn sync_secure_input(&mut self) -> bool {
        let wanted = self.secure_input_wanted();
        self.secure_input.set(wanted)
    }
}

#[cfg(test)]
mod tests {
    use benches_core::grid::Grid;
    use rune::Tx;

    use super::*;
    use crate::app::{open_tab, run_command};
    use crate::keymap::Command;

    fn secure_input_state() -> State {
        let mut state = State::default();
        crate::scenes::show_scene(&mut state, b"");
        state.workspace.set_window_focused(true);
        state
    }

    #[test]
    fn a_password_prompt_turns_secure_input_on_until_it_ends() {
        let mut state = secure_input_state();
        let password = Grid {
            password_input: true,
            ..Grid::default()
        };
        state.session().unwrap().publish_fixture(password);
        assert!(state.secure_input_wanted());
        assert_eq!(state.sync_secure_input(), SECURE_INPUT_SUPPORTED);
        assert_eq!(state.secure_input.enabled, SECURE_INPUT_SUPPORTED);
        state.session().unwrap().publish_fixture(Grid::default());
        assert!(!state.secure_input_wanted());
        assert_eq!(state.sync_secure_input(), SECURE_INPUT_SUPPORTED);
        assert!(!state.secure_input.enabled);
    }

    #[test]
    fn auto_secure_input_can_be_turned_off() {
        let mut state = secure_input_state();
        state.config.auto_secure_input = false;
        let password = Grid {
            password_input: true,
            ..Grid::default()
        };
        state.session().unwrap().publish_fixture(password);
        assert!(!state.secure_input_wanted());
        assert!(!state.sync_secure_input());
        assert!(!state.secure_input.enabled);
    }

    #[test]
    fn the_manual_toggle_survives_tabs_but_not_losing_focus() {
        let mut state = secure_input_state();
        let mut tx = Tx::default();
        run_command(&mut state, Command::ToggleSecureInput, &mut tx);
        assert!(state.secure_input_wanted());
        assert_eq!(state.secure_input.enabled, SECURE_INPUT_SUPPORTED);
        open_tab(&mut state, "other", Some("cat"));
        assert!(state.secure_input_wanted());
        assert!(!state.sync_secure_input());
        assert_eq!(state.secure_input.enabled, SECURE_INPUT_SUPPORTED);
        state.workspace.set_window_focused(false);
        assert!(!state.secure_input_wanted());
        assert_eq!(state.sync_secure_input(), SECURE_INPUT_SUPPORTED);
        assert!(!state.secure_input.enabled);
        state.workspace.set_window_focused(true);
        assert!(state.secure_input_wanted());
        assert_eq!(state.sync_secure_input(), SECURE_INPUT_SUPPORTED);
        run_command(&mut state, Command::ToggleSecureInput, &mut tx);
        assert!(!state.secure_input_wanted());
        assert!(!state.secure_input.enabled);
    }
}
