use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use zbus::{Connection, OwnedVariant, Proxy, Str};

use crate::Link;
use crate::notify::Notification;

/// Covers connecting, D-Bus activation of the service and the call itself.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// zbus needs a runtime, so each D-Bus user runs one on its own thread.
fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

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

fn show_in_file_manager(path: &Path) -> io::Result<()> {
    runtime()?.block_on(async {
        let call = async {
            let connection = Connection::session().await?;
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
        };
        tokio::time::timeout(RPC_TIMEOUT, call)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)
    })
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

/// Linux has no system-wide locale setting beyond the variables `missing_locale` already reads.
pub fn system_locale() -> Option<String> {
    None
}

pub const FALLBACK_LOCALE: &str = "C.UTF-8";

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
static POSTS: OnceLock<UnboundedSender<(u64, Notification)>> = OnceLock::new();

pub fn start_notifications(on_click: impl Fn(Option<u64>) + Send + 'static) {
    let (posts, queued) = unbounded_channel();
    let worker = std::thread::Builder::new()
        .name("benches-notifications".into())
        .spawn(move || {
            let served = runtime().and_then(|runtime| {
                runtime
                    .block_on(serve_notifications(queued, on_click))
                    .map_err(io::Error::other)
            });
            if let Err(error) = served {
                eprintln!("benches: notifications: {error}");
            }
        });
    if let Err(error) = worker {
        eprintln!("benches: start notifications: {error}");
        return;
    }
    if POSTS.set(posts).is_err() {
        eprintln!("benches: notifications were already started");
    }
}

pub fn post_notification(session: u64, notification: &Notification) {
    if let Some(posts) = POSTS.get() {
        // Fails only once the notification thread has given up, which it logged.
        posts.send((session, notification.clone())).ok();
    }
}

/// Posts what arrives on `queued`, replacing each session's last notification, and reports
/// clicks on them. Notifications belonging to other apps are ignored.
async fn serve_notifications(
    mut queued: UnboundedReceiver<(u64, Notification)>,
    on_click: impl Fn(Option<u64>),
) -> zbus::Result<()> {
    let connection = Connection::session().await?;
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
    let hints = HashMap::from([(
        "sound-name",
        OwnedVariant::from(Str::from_static("message-new-instant")),
    )]);
    // The server's id for each session's notification, which a new one replaces.
    let mut shown: HashMap<u64, u32> = HashMap::new();
    loop {
        tokio::select! {
            Some((session, notification)) = queued.recv() => {
                let replaces = shown.get(&session).copied().unwrap_or(0);
                let (summary, body) = content(&notification, markup);
                let posted = server
                    .call::<_, _, u32>(
                        "Notify",
                        &("Benches", replaces, "", summary, body, ["default", "Show"], &hints, -1),
                    )
                    .await;
                match posted {
                    Ok(id) => {
                        shown.insert(session, id);
                    }
                    Err(error) => eprintln!("benches: post notification: {error}"),
                }
            }
            Some(message) = clicks.next() => {
                if let Ok((id, _action)) = message.body().deserialize::<(u32, String)>()
                    && let Some((&session, _)) = shown.iter().find(|(_, shown)| **shown == id)
                {
                    on_click(Some(session));
                }
            }
            Some(message) = closes.next() => {
                if let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() {
                    shown.retain(|_, shown| *shown != id);
                }
            }
            else => return Ok(()),
        }
    }
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
