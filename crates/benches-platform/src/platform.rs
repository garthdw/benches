//! The operating system services Benches uses, with one backend per supported OS.

use std::io;
use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("benches-platform supports Linux and macOS only");

/// What clicking a link opens. A file is shown in the file manager rather than opened, because
/// opening would launch an `.app` or `.command` that program output merely named.
#[derive(Debug, PartialEq)]
pub enum Link {
    Url(String),
    File(PathBuf),
}

/// Blocks until the opener has run, so call it off the UI thread.
pub fn open(link: &Link) -> io::Result<()> {
    os::open(link)
}

fn run(mut command: Command) -> io::Result<()> {
    let status = command.status()?;
    if !status.success() {
        let program = command.get_program().to_string_lossy().into_owned();
        return Err(io::Error::other(format!("{program} {status}")));
    }
    Ok(())
}

/// A process's working directory, which follows the shell's `cd`.
pub fn process_cwd(pid: i32) -> Option<PathBuf> {
    os::process_cwd(pid)
}

/// The foreground process group of the terminal that `pid` controls.
pub fn foreground_process_group(pid: i32) -> Option<i32> {
    os::foreground_process_group(pid)
}

/// The user's locale as the system names it, such as `en_GB@rg=zazzzz`.
pub fn system_locale() -> Option<String> {
    os::system_locale()
}

/// The locale children get when the user's own isn't installed.
pub const FALLBACK_LOCALE: &str = os::FALLBACK_LOCALE;

/// Lowers the calling thread's priority, for background work that mustn't slow the UI.
pub fn lower_thread_priority() -> io::Result<()> {
    os::lower_thread_priority()
}

/// Where to grant Full Disk Access, while this process lacks it. Programs started from the app
/// share its privacy permissions, so without it some can't read even their own data.
pub fn full_disk_access_settings() -> Option<Link> {
    os::full_disk_access_settings()
}

/// Whether `set_secure_input` can keep other processes from reading keystrokes.
pub const SECURE_INPUT_SUPPORTED: bool = os::SECURE_INPUT_SUPPORTED;

/// Turns secure keyboard entry on or off. The OS counts these calls for the whole process, so
/// keep them balanced.
pub fn set_secure_input(enabled: bool) -> io::Result<()> {
    os::set_secure_input(enabled)
}

pub mod notify {
    use crate::os;

    #[derive(Clone, Debug, PartialEq)]
    pub struct Notification {
        pub title: String,
        pub subtitle: String,
        pub body: String,
    }

    impl Notification {
        /// The program's own title and body when it sent them (OSC 9 or 777), otherwise the
        /// tab's name.
        pub fn new(project: &str, tab: &str, title: String, body: String) -> Self {
            Self {
                title: if title.is_empty() {
                    tab.to_owned()
                } else {
                    title
                },
                subtitle: project.to_owned(),
                body,
            }
        }
    }

    /// Starts listening for clicks, which `on_click` gets, on any thread, with the session the
    /// notification was for, or `None` when it came from an earlier run. Until this runs, and
    /// wherever the system can't show notifications, posting does nothing.
    pub fn start(on_click: impl Fn(Option<u64>) + Send + 'static) {
        os::start_notifications(on_click);
    }

    /// Replaces the notification already shown for `session`, so a session has at most one.
    pub fn post(session: u64, notification: &Notification) {
        os::post_notification(session, notification);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_notification_names_the_tab_unless_the_program_gave_a_title() {
            assert_eq!(
                Notification::new("benches", "claude", String::new(), String::new()),
                Notification {
                    title: "claude".into(),
                    subtitle: "benches".into(),
                    body: String::new(),
                }
            );
            assert_eq!(
                Notification::new("benches", "claude", "Done".into(), "Built".into()),
                Notification {
                    title: "Done".into(),
                    subtitle: "benches".into(),
                    body: "Built".into(),
                }
            );
        }
    }
}
