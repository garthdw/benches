//! The operating system services Benches uses, with one backend per supported OS.

use std::ffi::{CStr, OsStr};
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, PoisonError};

pub mod dirs;
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

/// Both ends of a pseudo-terminal: the primary end the terminal reads and writes, and the
/// replica end the shell gets as its controlling terminal.
#[derive(Debug)]
pub struct Pty {
    pub primary: OwnedFd,
    pub replica: OwnedFd,
}

/// Opens a PTY of `cols` by `rows` with both ends close-on-exec from the start, as `openpty`
/// can't, so a child that another thread forks meanwhile doesn't inherit them and keep the
/// terminal open after its session closes.
pub fn open_pty(cols: u16, rows: u16) -> io::Result<Pty> {
    let primary = {
        // macOS's `posix_openpt` sometimes fails when another thread calls it at the same time.
        static OPENING: Mutex<()> = Mutex::new(());
        let _opening = OPENING.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: `posix_openpt` takes only flags.
        let descriptor =
            unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `descriptor` is new and nothing else owns it.
        unsafe { OwnedFd::from_raw_fd(descriptor) }
    };
    // SAFETY: both take the primary descriptor, which stays open throughout.
    if unsafe { libc::grantpt(primary.as_raw_fd()) } < 0
        || unsafe { libc::unlockpt(primary.as_raw_fd()) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let replica = os::open_pty_replica(primary.as_fd())?;
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCSWINSZ` reads one `winsize`.
    if unsafe { libc::ioctl(replica.as_raw_fd(), libc::TIOCSWINSZ as _, &size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Pty { primary, replica })
}

/// Opens the replica named by `name`, the nul-terminated path a lookup on its primary filled in.
fn open_pty_replica_at(name: &[u8]) -> io::Result<OwnedFd> {
    let name = CStr::from_bytes_until_nul(name).map_err(io::Error::other)?;
    // std opens files close-on-exec.
    let replica = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(OsStr::from_bytes(name.to_bytes()))?;
    Ok(replica.into())
}

/// The user's locale as the system names it, such as `en_GB@rg=zazzzz`.
pub fn system_locale() -> Option<String> {
    os::system_locale()
}

/// The locale children get when the user's own isn't installed.
pub const FALLBACK_LOCALE: &str = os::FALLBACK_LOCALE;

/// The shell when neither `$SHELL` nor the password database names one that runs.
pub const FALLBACK_SHELL: &str = os::FALLBACK_SHELL;

/// Whether stderr ends up somewhere the user won't look, as when launched from Finder or a
/// desktop launcher.
pub fn launched_without_terminal() -> bool {
    os::launched_without_terminal()
}

/// The modifier held to hover and open links, by what it is on every keyboard rather than
/// what each OS labels it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkModifier {
    Ctrl,
    Super,
}

pub const LINK_MODIFIER: LinkModifier = os::LINK_MODIFIER;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_ends_of_a_pty_are_close_on_exec_from_the_start() {
        let pty = open_pty(80, 24).unwrap();
        for end in [&pty.primary, &pty.replica] {
            // SAFETY: `F_GETFD` only reads the descriptor's flags.
            let flags = unsafe { libc::fcntl(end.as_raw_fd(), libc::F_GETFD) };
            assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        }
    }

    #[test]
    fn a_pty_opens_at_the_size_asked_for() {
        let pty = open_pty(100, 30).unwrap();
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `TIOCGWINSZ` writes one `winsize`.
        let result =
            unsafe { libc::ioctl(pty.primary.as_raw_fd(), libc::TIOCGWINSZ as _, &mut size) };
        assert_eq!(result, 0);
        assert_eq!((size.ws_col, size.ws_row), (100, 30));
    }
}
