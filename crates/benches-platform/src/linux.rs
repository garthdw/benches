use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use async_io::Timer;
use futures_util::StreamExt;
use futures_util::future::{self, Either};
use zbus::zvariant::Value;
use zbus::{Proxy, connection};

use crate::dirs::{self, Env};
use crate::notify::Notification;
use crate::{Link, LinkModifier};

pub fn state_home(env: Env) -> Option<PathBuf> {
    dirs::xdg_home(env, "XDG_STATE_HOME", ".local/state")
}

pub fn log_dir(env: Env) -> Option<PathBuf> {
    dirs::state_dir(env)
}

pub fn ghostty_theme_dirs(env: Env) -> Vec<PathBuf> {
    data_dirs(env)
        .into_iter()
        .map(|dir| dir.join("ghostty/themes"))
        .collect()
}

/// `share/benches/terminfo` beside `bin`.
pub const TERMINFO_DIR: &str = "share/benches/terminfo";

/// `$XDG_DATA_HOME` and then `$XDG_DATA_DIRS`, most important first.
fn data_dirs(env: Env) -> Vec<PathBuf> {
    let data_dirs = env("XDG_DATA_DIRS")
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs::xdg_home(env, "XDG_DATA_HOME", ".local/share")
        .into_iter()
        .chain(std::env::split_paths(&data_dirs).filter_map(|dir| dirs::absolute(dir.into())))
        .collect()
}

/// Covers connecting, D-Bus activation of the service and the call itself.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

pub fn open(link: &Link) -> io::Result<()> {
    match link {
        Link::Url(url) => crate::run(xdg_open(url)),
        Link::File(path) => show_in_file_manager(path)
            .or_else(|_| crate::run(xdg_open(path.parent().unwrap_or(path)))),
    }
}

fn xdg_open(target: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(target);
    command
}

/// Fails with `TimedOut` unless `call` finishes within `RPC_TIMEOUT`.
async fn timeout<T>(call: impl Future<Output = zbus::Result<T>>) -> io::Result<T> {
    match future::select(pin!(call), Timer::after(RPC_TIMEOUT)).await {
        Either::Left((result, _)) => result.map_err(io::Error::other),
        Either::Right(_) => Err(io::ErrorKind::TimedOut.into()),
    }
}

fn show_in_file_manager(path: &Path) -> io::Result<()> {
    async_io::block_on(timeout(async {
        let connection = connection::Builder::session()?.build().await?;
        let manager = Proxy::new(
            &connection,
            "org.freedesktop.FileManager1",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1",
        )
        .await?;
        manager
            .call::<_, _, ()>("ShowItems", &(vec![file_uri(path)], ""))
            .await
    }))
}

fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

pub fn process_cwd(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

pub fn foreground_process_group(pid: i32) -> Option<i32> {
    tpgid(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Field 8 of `/proc/<pid>/stat`, or `None` without a terminal. The command name in field 2
/// can hold spaces and parentheses, so fields are counted from its last `)`.
fn tpgid(stat: &str) -> Option<i32> {
    let (_, fields) = stat.rsplit_once(')')?;
    let group = fields.split_whitespace().nth(5)?.parse().ok()?;
    (group > 0).then_some(group)
}

/// Opens the replica through its primary, as glibc's `openpty` does, so it's the right one even
/// where `/dev/pts` belongs to another devpts instance, as in some sandboxes. Kernels before
/// 4.13 lack `TIOCGPTPEER`, so those open it by name.
pub fn open_pty_replica(primary: BorrowedFd) -> io::Result<OwnedFd> {
    // SAFETY: `TIOCGPTPEER` takes open flags and returns a new descriptor.
    let descriptor = unsafe {
        libc::ioctl(
            primary.as_raw_fd(),
            libc::TIOCGPTPEER,
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if descriptor >= 0 {
        // SAFETY: `descriptor` is new and nothing else owns it.
        return Ok(unsafe { OwnedFd::from_raw_fd(descriptor) });
    }
    let error = io::Error::last_os_error();
    if !matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOTTY)) {
        return Err(error);
    }
    let mut name = [0u8; 128];
    // SAFETY: `ptsname_r` writes at most `name.len()` bytes, ending with a nul.
    let error =
        unsafe { libc::ptsname_r(primary.as_raw_fd(), name.as_mut_ptr().cast(), name.len()) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    crate::open_pty_replica_at(&name)
}

/// Linux has no system-wide locale setting beyond the variables `missing_locale` already reads.
pub fn system_locale() -> Option<String> {
    None
}

pub const FALLBACK_LOCALE: &str = "C.UTF-8";

/// Every POSIX system has it, unlike zsh.
pub const FALLBACK_SHELL: &str = "/bin/sh";

pub fn launched_without_terminal() -> bool {
    !io::stderr().is_terminal()
}

/// Ctrl, as the window manager usually takes Super.
pub const LINK_MODIFIER: LinkModifier = LinkModifier::Ctrl;

/// Linux keeps a nice value per thread, so this lowers only the calling one.
pub fn lower_thread_priority() -> io::Result<()> {
    // SAFETY: changes only the calling thread's nice value.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Linux has no per-app privacy permissions.
pub fn full_disk_access_settings() -> Option<Link> {
    None
}

/// X11 and Wayland have nothing like macOS's secure event input.
pub const SECURE_INPUT_SUPPORTED: bool = false;

pub fn set_secure_input(_enabled: bool) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Set once the notification thread is running; it drops the receiver if it gives up.
static POSTS: OnceLock<Sender<(u64, Notification)>> = OnceLock::new();

pub fn start_notifications(on_click: impl Fn(Option<u64>) + Send + 'static) {
    match spawn_notifications(connection::Builder::session, on_click) {
        Ok(posts) => {
            if POSTS.set(posts).is_err() {
                eprintln!("benches: notifications were already started");
            }
        }
        Err(error) => eprintln!("benches: start notifications: {error}"),
    }
}

/// Starts the notification thread on the bus that `bus` connects to, returning where to post.
fn spawn_notifications(
    bus: impl Fn() -> zbus::Result<connection::Builder<'static>> + Send + 'static,
    on_click: impl Fn(Option<u64>) + Send + 'static,
) -> io::Result<Sender<(u64, Notification)>> {
    let (posts, queued) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("benches-notifications".into())
        .spawn(move || {
            if let Err(error) = async_io::block_on(serve_notifications(bus, queued, on_click)) {
                eprintln!("benches: notifications: {error}");
            }
        })?;
    Ok(posts)
}

pub fn post_notification(session: u64, notification: &Notification) {
    if let Some(posts) = POSTS.get() {
        // Fails only once the notification thread has given up, which it logged.
        posts.try_send((session, notification.clone())).ok();
    }
}

/// Posts what arrives on `queued`, replacing each session's last notification, and reports
/// clicks on them. Notifications belonging to other apps are ignored.
async fn serve_notifications(
    bus: impl Fn() -> zbus::Result<connection::Builder<'static>>,
    queued: Receiver<(u64, Notification)>,
    on_click: impl Fn(Option<u64>),
) -> zbus::Result<()> {
    let connection = bus()?.build().await?;
    let server = Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    // Subscribed before posting, so no click is missed.
    let mut clicks = server.receive_signal("ActionInvoked").await?;
    let mut closes = server.receive_signal("NotificationClosed").await?;
    let capabilities: Vec<String> = server.call("GetCapabilities", &()).await?;
    let markup = capabilities
        .iter()
        .any(|capability| capability == "body-markup");
    let hints = HashMap::from([("sound-name", Value::from("message-new-instant"))]);
    // The server's id for each session's notification, which a new one replaces.
    let mut shown: HashMap<u64, u32> = HashMap::new();
    let mut queued = pin!(queued);
    loop {
        futures_util::select! {
            (session, notification) = queued.select_next_some() => {
                let replaces = shown.get(&session).copied().unwrap_or(0);
                let posted = server
                    .call::<_, _, u32>(
                        "Notify",
                        &notify_args(replaces, content(&notification, markup), &hints),
                    )
                    .await;
                match posted {
                    Ok(id) => {
                        shown.insert(session, id);
                    }
                    Err(error) => eprintln!("benches: post notification: {error}"),
                }
            }
            message = clicks.select_next_some() => {
                if let Ok((id, _action)) = message.body().deserialize::<(u32, String)>()
                    && let Some((&session, _)) = shown.iter().find(|(_, shown)| **shown == id)
                {
                    on_click(Some(session));
                }
            }
            message = closes.select_next_some() => {
                if let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() {
                    shown.retain(|_, shown| *shown != id);
                }
            }
            complete => return Ok(()),
        }
    }
}

type Hints<'a> = HashMap<&'a str, Value<'a>>;

/// `Notify`'s arguments: the app's name, the id it replaces, an icon, the summary and body, the
/// actions, the hints and an expiry, where -1 leaves it to the server.
fn notify_args<'a>(
    replaces: u32,
    (summary, body): (String, String),
    hints: &'a Hints<'a>,
) -> (
    &'static str,
    u32,
    &'static str,
    String,
    String,
    &'static [&'static str],
    &'a Hints<'a>,
    i32,
) {
    (
        "Benches",
        replaces,
        "",
        summary,
        body,
        &["default", "Show"],
        hints,
        -1,
    )
}

/// Freedesktop notifications have no subtitle, so the project leads the body.
fn content(notification: &Notification, markup: bool) -> (String, String) {
    let body = if notification.body.is_empty() {
        notification.subtitle.clone()
    } else {
        format!("{}\n{}", notification.subtitle, notification.body)
    };
    let body = if markup {
        body.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    } else {
        body
    };
    (notification.title.clone(), body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::tests::lookup;

    #[test]
    fn data_dirs_default_to_the_spec_and_skip_relative_entries() {
        let unset = [("HOME", "/home/me")];
        assert_eq!(
            data_dirs(&lookup(&unset)),
            ["/home/me/.local/share", "/usr/local/share", "/usr/share"].map(PathBuf::from)
        );
        let set = [
            ("HOME", "/home/me"),
            ("XDG_DATA_HOME", "/data"),
            ("XDG_DATA_DIRS", "/nix/share:relative:/usr/share"),
        ];
        assert_eq!(
            data_dirs(&lookup(&set)),
            ["/data", "/nix/share", "/usr/share"].map(PathBuf::from)
        );
        let empty = [("XDG_DATA_DIRS", "")];
        assert_eq!(
            data_dirs(&lookup(&empty)),
            ["/usr/local/share", "/usr/share"].map(PathBuf::from)
        );
    }

    #[test]
    fn state_and_log_follow_xdg_state_home() {
        let vars = [("HOME", "/home/me"), ("XDG_STATE_HOME", "/state")];
        assert_eq!(
            dirs::state_dir(&lookup(&vars)),
            Some(PathBuf::from("/state/benches"))
        );
        assert_eq!(
            dirs::log_file(&lookup(&vars)),
            Some(PathBuf::from("/state/benches/benches.log"))
        );
    }

    fn args(command: Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn xdg_open_gets_the_target_alone() {
        assert_eq!(args(xdg_open("https://a.test/x")), ["https://a.test/x"]);
    }

    #[test]
    fn a_revealed_file_is_named_by_an_escaped_uri() {
        assert_eq!(
            file_uri(Path::new("/tmp/a b/ü#1.app")),
            "file:///tmp/a%20b/%C3%BC%231.app"
        );
    }

    #[test]
    fn the_terminal_s_group_is_read_after_the_command_name() {
        let stat = "42 (a) b (c)) S 1 42 42 34816 4567 4194304 0";
        assert_eq!(tpgid(stat), Some(4567));
        assert_eq!(tpgid("42 (sh) S 1 42 42 0 -1 4194304 0"), None);
        assert_eq!(tpgid("42 (sh"), None);
    }

    #[test]
    fn a_process_s_cwd_and_terminal_are_read() {
        let pid = std::process::id() as i32;
        assert_eq!(process_cwd(pid), std::env::current_dir().ok());
        assert_eq!(process_cwd(-1), None);
        assert_eq!(foreground_process_group(-1), None);
    }

    #[test]
    fn notify_is_called_with_the_spec_s_signature() {
        use zbus::zvariant::Type;
        fn signature<T: Type>(_: &T) -> String {
            T::SIGNATURE.to_string()
        }
        let hints = Hints::new();
        let args = notify_args(0, (String::new(), String::new()), &hints);
        assert_eq!(signature(&args), "(susssasa{sv}i)");
    }

    #[test]
    fn the_project_leads_the_body_and_markup_is_escaped_where_shown() {
        let notification = |body: &str| Notification {
            title: "Done".into(),
            subtitle: "a&b".into(),
            body: body.into(),
        };
        assert_eq!(
            content(&notification(""), false),
            ("Done".into(), "a&b".into())
        );
        assert_eq!(
            content(&notification("<built>"), true),
            ("Done".into(), "a&amp;b\n&lt;built&gt;".into())
        );
    }
}
