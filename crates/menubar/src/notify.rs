//! Notifications through `UNUserNotificationCenter`.
//!
//! `note_for` turns a watcher event into the text to show; it is plain Rust and tested without a screen.
//! `Notifier` posts that text. Every string reaches macOS as an `NSString` value and travels back only
//! inside a request identifier we parse ourselves (`parse_relaunch_id`); no text is ever pasted into
//! AppleScript or shell source.

use agent_switch_core::config::{CodexCliMode, Config, SurfaceKind};
use agent_switch_core::watcher::{Event, EventKind};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_foundation::{NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSSet, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction, UNNotificationActionOptions,
    UNNotificationCategory, UNNotificationCategoryOptions, UNNotificationPresentationOptions, UNNotificationRequest,
    UNNotificationResponse, UNNotificationSettings, UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

use crate::menu::{label_of, surface_name};

pub const ACTION_RELAUNCH: &str = "relaunch";
const CATEGORY_DESKTOP: &str = "desktop-switch-needed";
const CATEGORY_CLI: &str = "cli-switch-needed";
const RELAUNCH_PREFIX: &str = "relaunch:";

/// Which buttons a notification carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buttons {
    None,
    /// [Relaunch now] [Later]
    Relaunch,
    /// [Switch now] [Later]
    SwitchCli,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: String,
    pub buttons: Buttons,
}

impl Note {
    pub fn info(id: impl Into<String>, title: impl Into<String>, body: impl Into<String>) -> Note {
        Note { id: format!("info:{}", id.into()), title: title.into(), body: body.into(), buttons: Buttons::None }
    }
}

/// The request identifier of a switch-needed notification: it carries the surface and the event id back.
pub fn relaunch_id(kind: SurfaceKind, event_id: &str) -> String {
    format!("{RELAUNCH_PREFIX}{}:{event_id}", kind.name())
}

pub fn parse_relaunch_id(id: &str) -> Option<(SurfaceKind, String)> {
    let rest = id.strip_prefix(RELAUNCH_PREFIX)?;
    let (surface, event_id) = rest.split_once(':')?;
    (!event_id.is_empty()).then(|| surface.parse().ok().map(|k| (k, event_id.to_string()))).flatten()
}

fn percent_text(pct: Option<f64>) -> String {
    pct.map_or_else(|| "at its limit".to_string(), |p| format!("at {p:.0}%"))
}

/// The notification for a watcher event, or `None` for events that need none.
pub fn note_for(event: &Event, config: &Config) -> Option<Note> {
    let kind: Option<SurfaceKind> = event.surface.as_deref().and_then(|s| s.parse().ok());
    let surface = kind.map_or("", surface_name);
    let label = |id: &Option<String>| id.as_deref().map(|i| label_of(config, i)).unwrap_or_else(|| "the account".into());
    let (from, to) = (label(&event.from), label(&event.to));
    let key = format!("{}-{:?}-{}", event.ts, event.kind, surface);
    match event.kind {
        EventKind::Switched => {
            let mut body = format!("{from} is {}. Switched {surface} to {to}.", percent_text(event.pct));
            if kind == Some(SurfaceKind::CodexCli) {
                body.push_str(match config.auto.codex_cli_mode {
                    CodexCliMode::Force => " Running Codex sessions may need a new sign-in.",
                    _ => " Running Codex sessions keep the old account until restarted.",
                });
            }
            Some(Note::info(key, "Account switched", body))
        }
        EventKind::SwitchNeeded => {
            let kind = kind?;
            let event_id = event.id.as_deref()?;
            let (title, buttons, action) = if kind.is_desktop() {
                (format!("Relaunch {surface} as {to}?"), Buttons::Relaunch, "Relaunching quits and reopens the app.")
            } else {
                (format!("Switch {surface} to {to}?"), Buttons::SwitchCli, "Running sessions keep the old account until restarted.")
            };
            let body = format!("{from} is {}. {action}", percent_text(event.pct));
            Some(Note { id: relaunch_id(kind, event_id), title, body, buttons })
        }
        EventKind::SwitchFailed => Some(Note::info(key, format!("Could not switch {surface}"), event.detail.clone().unwrap_or_default())),
        EventKind::NoCandidate => Some(Note::info(key, "No account has room", format!("{from} on {surface} is near its limit and no other account has room."))),
        EventKind::AccountDead => Some(Note::info(key, "Sign-in needed", format!("{to} needs a new sign-in: its saved login was rejected."))),
        EventKind::RefreshHalted => Some(Note::info(key, "Login refresh paused", event.detail.clone().unwrap_or_default())),
        EventKind::AutoPaused => Some(Note::info(key, "Auto-switch paused", event.detail.clone().unwrap_or_default())),
        EventKind::AutoResumed | EventKind::Rejoined => None,
    }
}

/// True when this process runs from a `.app` bundle with a bundle identifier. `UNUserNotificationCenter`
/// and `SMAppService` raise an exception in a bare executable, so both are skipped there.
pub fn running_in_bundle() -> bool {
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app")
}

struct DelegateIvars {
    /// Called with the request identifier and the identifier of the button that was pressed.
    on_response: Box<dyn Fn(String, String)>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AgentSwitchNotificationDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        // A menu bar app counts as the foreground app, so ask for the banner explicitly.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            handler.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse, handler: &block2::DynBlock<dyn Fn()>) {
            let action = response.actionIdentifier().to_string();
            let request_id = response.notification().request().identifier().to_string();
            (self.ivars().on_response)(request_id, action);
            handler.call(());
        }
    }
);

impl Delegate {
    fn new(on_response: Box<dyn Fn(String, String)>) -> Retained<Delegate> {
        let this = Self::alloc().set_ivars(DelegateIvars { on_response });
        // SAFETY: `NSObject` implements `init`.
        unsafe { msg_send![super(this), init] }
    }
}

pub struct Notifier {
    center: Retained<UNUserNotificationCenter>,
    // The center keeps only a weak reference to its delegate.
    _delegate: Retained<Delegate>,
}

fn ns(text: &str) -> Retained<NSString> {
    NSString::from_str(text)
}

fn category(id: &str, now_title: &str) -> Retained<UNNotificationCategory> {
    let now = UNNotificationAction::actionWithIdentifier_title_options(&ns(ACTION_RELAUNCH), &ns(now_title), UNNotificationActionOptions::empty());
    let later = UNNotificationAction::actionWithIdentifier_title_options(&ns("later"), &ns("Later"), UNNotificationActionOptions::empty());
    UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &ns(id),
        &NSArray::from_retained_slice(&[now, later]),
        &NSArray::new(),
        UNNotificationCategoryOptions::empty(),
    )
}

fn describe(error: *mut NSError) -> String {
    // SAFETY: the system passes either null or a valid `NSError` for the duration of the callback.
    match unsafe { error.as_ref() } {
        Some(e) => e.localizedDescription().to_string(),
        None => "no error".into(),
    }
}

impl Notifier {
    /// Set up the center, register the buttons and ask for permission. `on_response` runs on a system
    /// thread when a button is pressed.
    pub fn init(on_response: impl Fn(String, String) + 'static) -> Result<Notifier, String> {
        if !running_in_bundle() {
            return Err("not running from the app bundle, so notifications are off (use scripts/bundle-app.sh)".into());
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate = Delegate::new(Box::new(on_response));
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        center.setNotificationCategories(&NSSet::from_retained_slice(&[category(CATEGORY_DESKTOP, "Relaunch now"), category(CATEGORY_CLI, "Switch now")]));

        let granted = RcBlock::new(|granted: Bool, error: *mut NSError| {
            crate::log_line(&format!("notification permission: granted={} ({})", granted.as_bool(), describe(error)));
        });
        center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &granted);

        let settings = RcBlock::new(|s: std::ptr::NonNull<UNNotificationSettings>| {
            // SAFETY: the system passes a valid settings object for the duration of the callback.
            let status = unsafe { s.as_ref() }.authorizationStatus().0;
            crate::log_line(&format!("notification authorization status: {status} (0 undecided, 1 denied, 2 allowed, 3 provisional)"));
        });
        center.getNotificationSettingsWithCompletionHandler(&settings);
        Ok(Notifier { center, _delegate: delegate })
    }

    pub fn post(&self, note: &Note) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&ns(&note.title));
        content.setBody(&ns(&note.body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        match note.buttons {
            Buttons::None => {}
            Buttons::Relaunch => content.setCategoryIdentifier(&ns(CATEGORY_DESKTOP)),
            Buttons::SwitchCli => content.setCategoryIdentifier(&ns(CATEGORY_CLI)),
        }
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&ns(&note.id), &content, None);
        let id = note.id.clone();
        let done = RcBlock::new(move |error: *mut NSError| {
            if !error.is_null() {
                crate::log_line(&format!("notification `{id}` was not delivered: {}", describe(error)));
            }
        });
        self.center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_switch_core::config::{Account, Provider};

    fn config() -> Config {
        let mut c = Config::default();
        for (id, label) in [("personal", "Personal"), ("team", "Team")] {
            c.add_account(Account { id: id.into(), provider: Provider::Claude, label: label.into(), email: None, uuid: None }).unwrap();
        }
        c
    }

    fn event(kind: EventKind) -> Event {
        Event { ts: 100, kind, surface: None, from: None, to: None, id: None, expires_at: None, detail: None, pct: None }
    }

    #[test]
    fn a_cli_switch_at_91_percent_reads_like_the_spec() {
        let e = Event { surface: Some("claude-cli".into()), from: Some("personal".into()), to: Some("team".into()), pct: Some(91.0), ..event(EventKind::Switched) };
        let n = note_for(&e, &config()).unwrap();
        assert_eq!(n.body, "Personal is at 91%. Switched Claude CLI to Team.");
        assert_eq!(n.buttons, Buttons::None);
    }

    #[test]
    fn a_codex_cli_switch_says_what_happens_to_running_sessions() {
        let mut c = config();
        let e = Event { surface: Some("codex-cli".into()), from: Some("personal".into()), to: Some("team".into()), pct: Some(95.0), ..event(EventKind::Switched) };
        assert!(note_for(&e, &c).unwrap().body.ends_with("keep the old account until restarted."));
        c.auto.codex_cli_mode = CodexCliMode::Force;
        assert!(note_for(&e, &c).unwrap().body.ends_with("may need a new sign-in."));
    }

    #[test]
    fn a_desktop_event_asks_to_relaunch_and_its_id_round_trips_to_the_click() {
        let e = Event {
            surface: Some("claude-desktop".into()),
            from: Some("personal".into()),
            to: Some("team".into()),
            id: Some("6b49c0-claude-desktop-team".into()),
            expires_at: Some(1000),
            pct: Some(92.0),
            ..event(EventKind::SwitchNeeded)
        };
        let n = note_for(&e, &config()).unwrap();
        assert_eq!(n.title, "Relaunch Claude Desktop as Team?");
        assert_eq!(n.buttons, Buttons::Relaunch);
        assert_eq!(parse_relaunch_id(&n.id), Some((SurfaceKind::ClaudeDesktop, "6b49c0-claude-desktop-team".to_string())));
    }

    #[test]
    fn codex_notify_mode_offers_a_switch_not_a_relaunch() {
        let e = Event {
            surface: Some("codex-cli".into()),
            from: Some("personal".into()),
            to: Some("team".into()),
            id: Some("e1".into()),
            ..event(EventKind::SwitchNeeded)
        };
        let n = note_for(&e, &config()).unwrap();
        assert_eq!((n.title.as_str(), n.buttons), ("Switch Codex CLI to Team?", Buttons::SwitchCli));
    }

    #[test]
    fn identifiers_that_are_not_ours_do_not_parse() {
        for bad in ["info:1", "relaunch:", "relaunch:claude-desktop:", "relaunch:nope:e1", "relaunch:claude-desktop", ""] {
            assert_eq!(parse_relaunch_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_workspace_label_with_quotes_and_applescript_stays_literal_data() {
        let evil = r#"Team" & do shell script "touch /tmp/pwned" & ""#;
        let mut c = Config::default();
        c.add_account(Account { id: "personal".into(), provider: Provider::Claude, label: "Personal".into(), email: None, uuid: None }).unwrap();
        c.add_account(Account { id: "team".into(), provider: Provider::Claude, label: evil.into(), email: None, uuid: None }).unwrap();
        let e = Event {
            surface: Some("claude-desktop".into()),
            from: Some("personal".into()),
            to: Some("team".into()),
            id: Some("e1".into()),
            ..event(EventKind::SwitchNeeded)
        };
        let n = note_for(&e, &c).unwrap();
        assert!(n.title.contains(r#"Team" & do shell script "touch /tmp/pwned""#), "{}", n.title);
        // The click identifier carries the surface and event id, never the label.
        assert!(!n.id.contains("shell script"));
    }

    #[test]
    fn quiet_events_make_no_notification() {
        for k in [EventKind::AutoResumed, EventKind::Rejoined] {
            assert!(note_for(&event(k), &config()).is_none());
        }
        // A switch-needed event without its id cannot be answered, so it is not shown with buttons.
        let e = Event { surface: Some("claude-desktop".into()), ..event(EventKind::SwitchNeeded) };
        assert!(note_for(&e, &config()).is_none());
    }
}
