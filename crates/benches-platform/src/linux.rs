use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::pin::{Pin, pin};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use async_io::Timer;
use futures_util::StreamExt;
use futures_util::future::{self, Either};
use futures_util::stream::Fuse;
use zbus::proxy::{OwnerChangedStream, SignalStream};
use zbus::zvariant::Value;
use zbus::{Connection, Proxy, connection};

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

/// Covers connecting, D-Bus activation of the service and the call itself. Tests wait it out, so
/// theirs is short.
const RPC_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(10)
};

pub fn open(link: &Link) -> io::Result<()> {
    match link {
        Link::Url(url) => crate::run(xdg_open(url)),
        Link::File(path) => show_in_file_manager(path).or_else(|error| {
            eprintln!(
                "benches: show {} in the file manager: {error}; opening its folder",
                path.display()
            );
            crate::run(xdg_open(path.parent().unwrap_or(path)))
        }),
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

/// Set once the notification thread is running.
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
        .spawn(move || async_io::block_on(serve_notifications(bus, queued, on_click)))?;
    Ok(posts)
}

pub fn post_notification(session: u64, notification: &Notification) {
    if let Some(posts) = POSTS.get() {
        // Fails only if the notification thread has died, which a panic message reports.
        posts.try_send((session, notification.clone())).ok();
    }
}

/// Posts what arrives on `queued`, replacing each session's last notification, and reports
/// clicks on them. Notifications belonging to other apps are ignored. It connects when a post
/// arrives and it has no connection, so it outlives restarts of the bus and the server.
async fn serve_notifications(
    bus: impl Fn() -> zbus::Result<connection::Builder<'static>>,
    queued: Receiver<(u64, Notification)>,
    on_click: impl Fn(Option<u64>),
) {
    let mut queued = pin!(queued);
    let mut backoff = Backoff::default();
    let mut post = queued.next().await;
    // Without a server, or while backing off, the post is dropped.
    while let Some((session, notification)) = post.take() {
        if backoff.ready(Instant::now()) {
            match Server::connect(&bus).await {
                Ok(mut server) => {
                    backoff = Backoff::default();
                    server.post(session, &notification).await;
                    post = server.serve(&mut queued, &on_click).await;
                }
                Err(error) => {
                    eprintln!("benches: connect to the notification server: {error}");
                    backoff.failed(Instant::now());
                }
            }
        }
        if post.is_none() {
            post = queued.next().await;
        }
    }
}

/// The notification server, over one connection to the bus.
struct Server {
    proxy: Proxy<'static>,
    clicks: SignalStream<'static>,
    closes: SignalStream<'static>,
    owners: Fuse<OwnerChangedStream<'static>>,
    markup: bool,
    hints: Hints<'static>,
    /// The server's id for each session's notification, which a new one replaces.
    shown: HashMap<u64, u32>,
}

impl Server {
    async fn connect(
        bus: impl Fn() -> zbus::Result<connection::Builder<'static>>,
    ) -> io::Result<Self> {
        timeout(async {
            let connection = bus()?.build().await?;
            let proxy = Proxy::new(
                &connection,
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            )
            .await?;
            // Subscribed before posting, so no click is missed.
            let clicks = proxy.receive_signal("ActionInvoked").await?;
            let closes = proxy.receive_signal("NotificationClosed").await?;
            let capabilities: Vec<String> = proxy.call("GetCapabilities", &()).await?;
            // After `GetCapabilities`, which may start the server.
            let owners = proxy.receive_owner_changed().await?.fuse();
            Ok(Self {
                markup: capabilities
                    .iter()
                    .any(|capability| capability == "body-markup"),
                proxy,
                clicks,
                closes,
                owners,
                hints: Hints::from([("sound-name", Value::from("message-new-instant"))]),
                shown: HashMap::new(),
            })
        })
        .await
    }

    /// Posts and reports clicks until the connection closes, another server takes over, or the
    /// posts stop. Returns a post that found the connection closed, for the next one.
    async fn serve(
        &mut self,
        queued: &mut Pin<&mut Receiver<(u64, Notification)>>,
        on_click: &impl Fn(Option<u64>),
    ) -> Option<(u64, Notification)> {
        loop {
            futures_util::select! {
                post = queued.next() => match post {
                    Some(post) if self.proxy.connection().is_closed() => return Some(post),
                    Some((session, notification)) => self.post(session, &notification).await,
                    None => return None,
                },
                message = self.clicks.next() => match message {
                    Some(message) => {
                        if let Some(session) = self.clicked(&message) {
                            on_click(Some(session));
                        }
                    }
                    None => return None,
                },
                message = self.closes.next() => match message {
                    Some(message) => self.closed(&message),
                    None => return None,
                },
                // A new server numbers its notifications afresh, so `shown` would be wrong.
                _ = self.owners.next() => return None,
            }
        }
    }

    /// A server that never answers drops this post rather than holding up the next.
    async fn post(&mut self, session: u64, notification: &Notification) {
        let replaces = self.shown.get(&session).copied().unwrap_or(0);
        let posted = timeout(self.proxy.call::<_, _, u32>(
            "Notify",
            &notify_args(replaces, content(notification, self.markup), &self.hints),
        ))
        .await;
        match posted {
            Ok(id) => {
                self.shown.insert(session, id);
            }
            Err(error) => eprintln!("benches: post notification: {error}"),
        }
    }

    /// The session whose notification an `ActionInvoked` signal is for, if it is one of ours.
    fn clicked(&self, message: &zbus::Message) -> Option<u64> {
        let (id, _action) = message.body().deserialize::<(u32, String)>().ok()?;
        self.shown
            .iter()
            .find(|(_, shown)| **shown == id)
            .map(|(&session, _)| session)
    }

    fn closed(&mut self, message: &zbus::Message) {
        if let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() {
            self.shown.retain(|_, shown| *shown != id);
        }
    }
}

/// Spaces out attempts to connect while they fail, so posts to an unreachable server don't
/// each wait out `RPC_TIMEOUT`.
struct Backoff {
    delay: Duration,
    next_attempt: Option<Instant>,
}

impl Backoff {
    const FIRST: Duration = Duration::from_secs(1);
    const LONGEST: Duration = Duration::from_secs(60);

    fn ready(&self, now: Instant) -> bool {
        self.next_attempt.is_none_or(|next| now >= next)
    }

    fn failed(&mut self, now: Instant) {
        self.next_attempt = Some(now + self.delay);
        self.delay = (self.delay * 2).min(Self::LONGEST);
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            delay: Self::FIRST,
            next_attempt: None,
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

    #[test]
    fn failed_connections_wait_longer_each_time_up_to_a_minute() {
        let start = Instant::now();
        let mut backoff = Backoff::default();
        assert!(backoff.ready(start));
        backoff.failed(start);
        assert!(!backoff.ready(start + Duration::from_millis(999)));
        assert!(backoff.ready(start + Duration::from_secs(1)));
        backoff.failed(start);
        assert!(!backoff.ready(start + Duration::from_millis(1999)));
        for _ in 0..10 {
            backoff.failed(start);
        }
        assert!(backoff.ready(start + Duration::from_secs(60)));
    }

    /// A bus at `socket`, as `spawn_notifications` takes it.
    fn bus_at(
        socket: &Path,
    ) -> impl Fn() -> zbus::Result<connection::Builder<'static>> + Send + 'static {
        let address = format!("unix:path={}", socket.display());
        move || connection::Builder::address(address.as_str())
    }

    #[test]
    fn connecting_to_a_bus_that_never_answers_times_out() {
        let dir = tempfile::tempdir().expect("a temporary folder");
        let socket = dir.path().join("bus");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("binds");
        let connected = async_io::block_on(Server::connect(bus_at(&socket)));
        assert_eq!(
            connected.err().map(|error| error.kind()),
            Some(io::ErrorKind::TimedOut)
        );
    }

    /// D-Bus tests run against private `dbus-daemon`s, only when `BENCHES_DBUS_TESTS` is set:
    /// `BENCHES_DBUS_TESTS=1 cargo test -p benches-platform dbus`.
    mod dbus {
        use std::io::{BufRead, BufReader};
        use std::process::{Child, Stdio};
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::mpsc;

        use tempfile::TempDir;
        use zbus::object_server::SignalEmitter;
        use zbus::zvariant::OwnedValue;

        use super::*;

        fn enabled() -> bool {
            let enabled = std::env::var_os("BENCHES_DBUS_TESTS").is_some()
                && Command::new("dbus-daemon")
                    .arg("--version")
                    .output()
                    .is_ok_and(|output| output.status.success());
            if !enabled {
                eprintln!("skipped: needs BENCHES_DBUS_TESTS=1 and dbus-daemon");
            }
            enabled
        }

        /// A session bus of its own at a fixed socket, so it can be restarted at the same address.
        struct Bus {
            dir: TempDir,
            daemon: Child,
        }

        impl Bus {
            fn start() -> Self {
                let dir = tempfile::tempdir().expect("a temporary folder");
                let daemon = Self::daemon(&dir.path().join("bus"));
                Self { dir, daemon }
            }

            fn daemon(socket: &Path) -> Child {
                let mut daemon = Command::new("dbus-daemon")
                    .args(["--session", "--nofork", "--print-address"])
                    .arg(format!("--address=unix:path={}", socket.display()))
                    .stdout(Stdio::piped())
                    .spawn()
                    .expect("dbus-daemon starts");
                // It prints its address once it listens.
                BufReader::new(daemon.stdout.take().expect("piped stdout"))
                    .read_line(&mut String::new())
                    .expect("dbus-daemon prints its address");
                daemon
            }

            fn socket(&self) -> PathBuf {
                self.dir.path().join("bus")
            }

            fn restart(&mut self) {
                self.stop();
                std::fs::remove_file(self.socket()).ok();
                self.daemon = Self::daemon(&self.socket());
            }

            fn stop(&mut self) {
                self.daemon.kill().ok();
                self.daemon.wait().ok();
            }

            /// Serves a `Stub` that reports what each notification replaces and its body on
            /// `notified`.
            fn serve(&self, notified: mpsc::Sender<(u32, String)>) -> zbus::Connection {
                async_io::block_on(async {
                    bus_at(&self.socket())()?
                        .name("org.freedesktop.Notifications")?
                        .serve_at(
                            "/org/freedesktop/Notifications",
                            Stub {
                                notified,
                                last_id: AtomicU32::new(0),
                            },
                        )?
                        .build()
                        .await
                })
                .expect("the stub server starts")
            }

            /// Starts the notification thread on this bus, returning where to post and the
            /// sessions whose notifications were clicked.
            fn notifications(&self) -> (Sender<(u64, Notification)>, mpsc::Receiver<Option<u64>>) {
                let (clicked, clicks) = mpsc::channel();
                let posts = spawn_notifications(bus_at(&self.socket()), move |session| {
                    clicked.send(session).ok();
                })
                .expect("the notification thread starts");
                (posts, clicks)
            }
        }

        impl Drop for Bus {
            fn drop(&mut self) {
                self.stop();
            }
        }

        /// Reports what each notification replaces and its body, clicks it at once, and never
        /// answers one whose body ends in "hang".
        struct Stub {
            notified: mpsc::Sender<(u32, String)>,
            last_id: AtomicU32,
        }

        #[zbus::interface(name = "org.freedesktop.Notifications")]
        impl Stub {
            fn get_capabilities(&self) -> Vec<String> {
                Vec::new()
            }

            #[allow(clippy::too_many_arguments)]
            async fn notify(
                &self,
                #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
                _app: String,
                replaces: u32,
                _icon: String,
                _summary: String,
                body: String,
                _actions: Vec<String>,
                _hints: HashMap<String, OwnedValue>,
                _timeout: i32,
            ) -> u32 {
                if body.ends_with("hang") {
                    std::future::pending::<()>().await;
                }
                let id = if replaces == 0 {
                    self.last_id.fetch_add(1, Ordering::Relaxed) + 1
                } else {
                    replaces
                };
                self.notified.send((replaces, body)).ok();
                Self::action_invoked(&emitter, id, "default").await.ok();
                id
            }

            #[zbus(signal)]
            async fn action_invoked(
                emitter: &SignalEmitter<'_>,
                id: u32,
                action: &str,
            ) -> zbus::Result<()>;
        }

        fn notification(body: &str) -> Notification {
            Notification {
                title: "Done".into(),
                subtitle: "proj".into(),
                body: body.into(),
            }
        }

        const WAIT: Duration = Duration::from_secs(5);

        /// The body of the next notification posted.
        fn next_body(notified: &mpsc::Receiver<(u32, String)>) -> Option<String> {
            notified.recv_timeout(WAIT).ok().map(|(_, body)| body)
        }

        #[test]
        fn a_post_the_server_never_answers_does_not_hold_up_later_ones() {
            if !enabled() {
                return;
            }
            let bus = Bus::start();
            let (notified, bodies) = mpsc::channel();
            let _stub = bus.serve(notified);
            let (posts, clicks) = bus.notifications();

            posts.try_send((1, notification("hang"))).ok();
            posts.try_send((2, notification("after"))).ok();
            assert_eq!(next_body(&bodies).as_deref(), Some("proj\nafter"));
            assert_eq!(clicks.recv_timeout(WAIT), Ok(Some(2)));
        }

        #[test]
        fn posts_and_clicks_resume_once_the_bus_is_back() {
            if !enabled() {
                return;
            }
            let mut bus = Bus::start();
            let (notified, bodies) = mpsc::channel();
            let stub = bus.serve(notified.clone());
            let (posts, clicks) = bus.notifications();
            posts.try_send((1, notification("before"))).ok();
            assert_eq!(next_body(&bodies).as_deref(), Some("proj\nbefore"));
            assert_eq!(clicks.recv_timeout(WAIT), Ok(Some(1)));

            drop(stub);
            bus.restart();
            let _stub = bus.serve(notified);
            // Posts sent before the thread notices the old connection is gone may be lost.
            let resumed = (0..10).any(|_| {
                posts.try_send((2, notification("after"))).ok();
                bodies
                    .recv_timeout(RPC_TIMEOUT * 2)
                    .ok()
                    .map(|(_, body)| body)
                    .as_deref()
                    == Some("proj\nafter")
            });
            assert!(resumed, "no post reached the restarted bus");
            assert_eq!(clicks.recv_timeout(WAIT), Ok(Some(2)));
        }

        #[test]
        fn a_restarted_server_s_ids_are_not_mistaken_for_ours() {
            if !enabled() {
                return;
            }
            let bus = Bus::start();
            let (notified, posted) = mpsc::channel();
            let stub = bus.serve(notified.clone());
            let (posts, clicks) = bus.notifications();
            posts.try_send((1, notification("before"))).ok();
            assert_eq!(posted.recv_timeout(WAIT), Ok((0, "proj\nbefore".into())));
            assert_eq!(clicks.recv_timeout(WAIT), Ok(Some(1)));

            drop(stub);
            let _stub = bus.serve(notified);
            // The new server numbers its notifications afresh, so replacing the old one's id
            // would replace some other app's notification.
            let fresh = (0..10).any(|_| {
                posts.try_send((1, notification("after"))).ok();
                posted.recv_timeout(WAIT) == Ok((0, "proj\nafter".into()))
            });
            assert!(fresh, "every post replaced the old server's id");
        }
    }
}
