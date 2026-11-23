use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use crate::dirs::{self, Env};
use crate::notify::Notification;
use crate::{Link, LinkModifier};

/// The config folder, as macOS has no XDG state directory and existing installs keep their
/// saved layout there.
pub fn state_home(env: Env) -> Option<PathBuf> {
    dirs::config_home(env)
}

pub fn log_dir(env: Env) -> Option<PathBuf> {
    Some(dirs::home(env)?.join("Library/Logs"))
}

pub fn ghostty_theme_dirs(_env: Env) -> Vec<PathBuf> {
    vec![PathBuf::from(
        "/Applications/Ghostty.app/Contents/Resources/ghostty/themes",
    )]
}

/// In the app bundle's resources, beside `Contents/MacOS`.
pub const TERMINFO_DIR: &str = "Resources/terminfo";

pub fn open(link: &Link) -> io::Result<()> {
    crate::run(open_command(link))
}

fn open_command(link: &Link) -> Command {
    let mut open = Command::new("/usr/bin/open");
    match link {
        Link::Url(url) => open.arg(url),
        Link::File(path) => open.arg("-R").arg(path),
    };
    open
}

/// The process's `proc_pidinfo` of `flavor`, which must fill a `T`.
fn pid_info<T>(pid: i32, flavor: libc::c_int) -> Option<T> {
    let mut info = std::mem::MaybeUninit::<T>::zeroed();
    let size = std::mem::size_of::<T>() as i32;
    // SAFETY: `info` is a writable buffer of `size` bytes.
    let written = unsafe { libc::proc_pidinfo(pid, flavor, 0, info.as_mut_ptr().cast(), size) };
    // SAFETY: the kernel filled the whole struct, and zeroed bytes are valid for it anyway.
    (written == size).then(|| unsafe { info.assume_init() })
}

pub fn process_cwd(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let info = pid_info::<libc::proc_vnodepathinfo>(pid, libc::PROC_PIDVNODEPATHINFO)?;
    let path = info.pvi_cdir.vip_path;
    // SAFETY: `vip_path` is a flat 1024-byte buffer split into rows by libc.
    let bytes: &[u8; 1024] = unsafe { &*path.as_ptr().cast() };
    let path = std::ffi::CStr::from_bytes_until_nul(bytes).ok()?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
}

pub fn foreground_process_group(pid: i32) -> Option<i32> {
    let info = pid_info::<libc::proc_bsdinfo>(pid, libc::PROC_PIDTBSDINFO)?;
    i32::try_from(info.e_tpgid).ok()
}

pub fn system_locale() -> Option<String> {
    let identifier = objc2_core_foundation::CFLocale::current()?.identifier()?;
    Some(identifier.to_string())
}

pub const FALLBACK_LOCALE: &str = "en_US.UTF-8";

pub const FALLBACK_SHELL: &str = "/bin/zsh";

/// Whether this is the bundled app rather than a `cargo run` binary, which answers to macOS as
/// the terminal that started it.
fn in_bundle() -> bool {
    std::env::current_exe().is_ok_and(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"))
}

pub fn launched_without_terminal() -> bool {
    in_bundle()
}

/// ⌘, as for the app's own shortcuts.
pub const LINK_MODIFIER: LinkModifier = LinkModifier::Super;

/// Only the bundled app has permissions of its own to grant.
pub fn full_disk_access_settings() -> Option<Link> {
    if !in_bundle() {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    // Every Mac has this file, and only Full Disk Access lets a process read it.
    let database = PathBuf::from(home).join("Library/Application Support/com.apple.TCC/TCC.db");
    match std::fs::File::open(database) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Some(Link::Url(
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles".to_owned(),
        )),
        _ => None,
    }
}

/// Utility QoS, which keeps the thread off the performance cores while others need them.
pub fn lower_thread_priority() -> io::Result<()> {
    // SAFETY: changes only the calling thread's QoS class.
    match unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) } {
        0 => Ok(()),
        error => Err(io::Error::from_raw_os_error(error)),
    }
}

pub const SECURE_INPUT_SUPPORTED: bool = true;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
}

pub fn set_secure_input(enabled: bool) -> io::Result<()> {
    // SAFETY: plain Carbon calls with no arguments.
    let status = unsafe {
        if enabled {
            EnableSecureEventInput()
        } else {
            DisableSecureEventInput()
        }
    };
    if status != 0 {
        return Err(io::Error::other(format!("status {status}")));
    }
    Ok(())
}

/// Set once the notification centre is usable, which needs the app bundle.
static NOTIFICATIONS_ENABLED: AtomicBool = AtomicBool::new(false);

type OnClick = Box<dyn Fn(Option<u64>) + Send>;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "BenchesNotificationDelegate"]
    #[ivars = OnClick]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &block2::DynBlock<dyn Fn()>,
        ) {
            let identifier = response.notification().request().identifier();
            (self.ivars())(session_for(&identifier.to_string()));
            completion_handler.call(());
        }
    }
);

/// Does nothing outside the app bundle, where the notification centre raises an exception.
pub fn start_notifications(on_click: impl Fn(Option<u64>) + Send + 'static) {
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        eprintln!("benches: notifications need the app bundle; not posting any");
        return;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let delegate = Delegate::alloc().set_ivars(Box::new(on_click) as OnClick);
    // SAFETY: NSObject's `init` on a freshly allocated instance with its ivars set.
    let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
    center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // The centre only holds its delegate weakly, and it lives as long as the app.
    std::mem::forget(delegate);
    let handler = RcBlock::new(|_granted: Bool, error: *mut NSError| {
        log_error("ask to notify", error);
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &handler,
    );
    NOTIFICATIONS_ENABLED.store(true, Ordering::Relaxed);
}

pub fn post_notification(session: u64, notification: &Notification) {
    if !NOTIFICATIONS_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&notification.title));
    content.setSubtitle(&NSString::from_str(&notification.subtitle));
    content.setBody(&NSString::from_str(&notification.body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&identifier(session)),
        &content,
        None,
    );
    let handler = RcBlock::new(|error: *mut NSError| log_error("post notification", error));
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&handler));
}

fn log_error(context: &str, error: *mut NSError) {
    // SAFETY: the completion handler's error is null or a live NSError for its duration.
    if let Some(error) = unsafe { error.as_ref() } {
        eprintln!("benches: {context}: {}", error.localizedDescription());
    }
}

/// Session ids restart with each run, so the process id keeps an old notification from
/// revealing whichever session now has its id.
fn identifier(session: u64) -> String {
    format!("{}.{session}", std::process::id())
}

fn session_for(identifier: &str) -> Option<u64> {
    let (process, session) = identifier.split_once('.')?;
    (process.parse() == Ok(std::process::id()))
        .then(|| session.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::tests::lookup;

    #[test]
    fn state_stays_in_the_config_folder_and_the_log_in_library_logs() {
        let vars = [("HOME", "/home/me"), ("XDG_STATE_HOME", "/state")];
        assert_eq!(
            dirs::state_dir(&lookup(&vars)),
            Some(PathBuf::from("/home/me/.config/benches"))
        );
        assert_eq!(
            dirs::log_file(&lookup(&vars)),
            Some(PathBuf::from("/home/me/Library/Logs/benches.log"))
        );
    }

    fn args(link: Link) -> Vec<String> {
        open_command(&link)
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_url_opens_plainly() {
        assert_eq!(
            args(Link::Url("https://a.test/x".to_owned())),
            ["https://a.test/x"]
        );
    }

    #[test]
    fn a_file_is_revealed_not_opened() {
        assert_eq!(
            args(Link::File("/tmp/a b.app".into())),
            ["-R", "/tmp/a b.app"]
        );
    }

    #[test]
    fn only_this_run_s_notifications_name_a_session() {
        assert_eq!(session_for(&identifier(42)), Some(42));
        assert_eq!(session_for(&format!("{}.42", std::process::id() + 1)), None);
        assert_eq!(session_for("42"), None);
    }

    #[test]
    fn a_process_s_cwd_and_terminal_are_read() {
        let pid = std::process::id() as i32;
        assert_eq!(process_cwd(pid), std::env::current_dir().ok());
        assert_eq!(process_cwd(-1), None);
        assert_eq!(foreground_process_group(-1), None);
    }
}
