//! Secure keyboard entry: while on, other processes can't read keystrokes through event taps.

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
}

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
        if self.enabled == wanted {
            return false;
        }
        // SAFETY: plain Carbon calls with no arguments, balanced by `enabled`.
        let status = unsafe {
            if wanted {
                EnableSecureEventInput()
            } else {
                DisableSecureEventInput()
            }
        };
        if status != 0 {
            eprintln!("agentd: secure event input: status {status}");
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
