//! Secure keyboard entry: while on, other processes can't read keystrokes through event taps.

use benches_platform::{SECURE_INPUT_SUPPORTED, set_secure_input};

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
