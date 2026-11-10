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

/// Set once the notification centre is usable, which needs the app bundle.
static ENABLED: AtomicBool = AtomicBool::new(false);

type OnClick = Box<dyn Fn(Option<u64>)>;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AgentdNotificationDelegate"]
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

/// Starts listening for clicks, which `on_click` gets with the session the notification was
/// for, or `None` when it came from an earlier run. Does nothing outside the app bundle, where
/// the notification centre raises an exception.
pub fn start(on_click: impl Fn(Option<u64>) + 'static) {
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        eprintln!("agentd: notifications need the app bundle; not posting any");
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
    ENABLED.store(true, Ordering::Relaxed);
}

/// Replaces the notification already shown for `session`, so a session has at most one.
pub fn post(session: u64, notification: &Notification) {
    if !ENABLED.load(Ordering::Relaxed) {
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
        eprintln!("agentd: {context}: {}", error.localizedDescription());
    }
}

#[derive(Debug, PartialEq)]
pub struct Notification {
    pub title: String,
    pub subtitle: String,
    pub body: String,
}

impl Notification {
    /// The program's own title and body when it sent them (OSC 9 or 777), otherwise the tab's name.
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

    #[test]
    fn a_notification_names_the_tab_unless_the_program_gave_a_title() {
        assert_eq!(
            Notification::new("agentd", "claude", String::new(), String::new()),
            Notification {
                title: "claude".into(),
                subtitle: "agentd".into(),
                body: String::new(),
            }
        );
        assert_eq!(
            Notification::new("agentd", "claude", "Done".into(), "Built".into()),
            Notification {
                title: "Done".into(),
                subtitle: "agentd".into(),
                body: "Built".into(),
            }
        );
    }

    #[test]
    fn only_this_run_s_notifications_name_a_session() {
        assert_eq!(session_for(&identifier(42)), Some(42));
        assert_eq!(session_for(&format!("{}.42", std::process::id() + 1)), None);
        assert_eq!(session_for("42"), None);
    }
}
