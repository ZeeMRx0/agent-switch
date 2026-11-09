//! agent-switch menu bar app. It hosts the usage watcher (one per machine), shows each surface's account and
//! usage, switches accounts in a click, and turns "switch needed" events into notifications.
//!
//! Threads: the main thread owns the tray, the menu and the notification center; the `watcher` thread
//! (see `worker.rs`) runs the watcher and every action that changes accounts. They talk through
//! `UserEvent`s and `worker::Cmd`s only.

mod account_row;
mod add_account;
mod icon;
mod login_item;
mod menu;
mod notify;
mod worker;

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use agent_switch_core::config::Config;
use agent_switch_core::paths::Paths;
use agent_switch_core::state::{now_secs, State};
use muda::{MenuEvent, MenuId};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::menu::{Action, Ctx, Entry, ForceOffer};
use crate::notify::{Note, Notifier};
use crate::worker::{Handle, Report, StartError};

const REFRESH_EVERY: Duration = Duration::from_secs(5);
const STOP_WAIT: Duration = Duration::from_secs(15);
const LOG_ROTATE_BYTES: u64 = 1 << 20;
const USAGE: &str = "usage: agent-switch-menubar [--print-menu | --notify-test | --help | --version]\n\
  (no option)   run the menu bar app and the usage watcher\n\
  --print-menu  print the menu for the current config and state, then exit\n\
  --notify-test post one test notification with its buttons, log what happens for 20 s, then exit";

enum UserEvent {
    Menu(MenuId),
    Refresh,
    Report(Report),
    /// A notification button: the request identifier and the button identifier.
    Button { request_id: String, button: String },
    Exit,
}

/// Print to stderr and append to `~/Library/Logs/agent-switch/menubar.log` (0600). The file is the only
/// place to look when the app was started from Finder or as a login item.
pub fn log_line(message: &str) {
    eprintln!("agent-switch-menubar: {message}");
    let Ok(paths) = Paths::from_env() else { return };
    let dir = paths.logs_dir();
    if agent_switch_core::fsutil::ensure_private_dir(&dir).is_err() {
        return;
    }
    let file = dir.join("menubar.log");
    if std::fs::metadata(&file).is_ok_and(|m| m.len() >= LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&file, dir.join("menubar.log.1"));
    }
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&file) {
        let _ = writeln!(f, "{} {message}", now_secs());
    }
}

fn main() {
    let arg = std::env::args().nth(1);
    let code = match arg.as_deref() {
        None => run_app(),
        Some("--print-menu") => print_menu(),
        Some("--notify-test") => run_notify_test(),
        Some("--version") => {
            println!("agent-switch-menubar {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            0
        }
        Some(other) => {
            eprintln!("agent-switch-menubar: unknown option `{other}`\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

/// The menu for what is on disk now.
fn current_menu(paths: &Paths, watcher_running: bool, notice: Option<&str>, force_offer: Option<ForceOffer>) -> Vec<Entry> {
    let ctx = Ctx { now: now_secs(), login: login_item::status(), watcher_running, notice: notice.map(str::to_string), force_offer };
    match (Config::load(paths), State::load(paths)) {
        (Ok(config), Ok(state)) => menu::build(&config, &state, &ctx),
        (Err(e), _) | (_, Err(e)) => menu::error_menu(&e.to_string(), &ctx),
    }
}

fn print_menu() -> i32 {
    match Paths::from_env() {
        Ok(paths) => {
            print!("{}", menu::to_text(&current_menu(&paths, true, None, None)));
            0
        }
        Err(e) => {
            eprintln!("agent-switch-menubar: {e}");
            1
        }
    }
}

fn new_event_loop() -> EventLoop<UserEvent> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    // A menu bar app has no Dock icon and no main window.
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop
}

fn run_app() -> i32 {
    let paths = match Paths::from_env() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agent-switch-menubar: {e}");
            return 1;
        }
    };
    let event_loop = new_event_loop();
    let proxy = event_loop.create_proxy();

    // The watcher thread takes the singleton lock before anything is drawn, so a second copy leaves quietly.
    let report_proxy = std::sync::Mutex::new(proxy.clone());
    let worker = worker::spawn(move |r| {
        if let Ok(p) = report_proxy.lock() {
            let _ = p.send_event(UserEvent::Report(r));
        }
    });
    let worker = match worker {
        Ok(w) => w,
        Err(StartError::Busy(why)) => {
            eprintln!("agent-switch-menubar: already running ({why})");
            return 1;
        }
        Err(StartError::Failed(why)) => {
            eprintln!("agent-switch-menubar: cannot start: {why}");
            return 1;
        }
    };

    let menu_proxy = std::sync::Mutex::new(proxy.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Ok(p) = menu_proxy.lock() {
            let _ = p.send_event(UserEvent::Menu(e.id));
        }
    }));
    spawn_ticker(proxy.clone());

    let mut app = App { paths, proxy, worker: Some(worker), tray: None, notifier: None, actions: HashMap::new(), shown: Vec::new(), force_rebuild: false, last_notice: None, force_offer: None, watcher_running: true, counter: 0 };
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => app.start(),
            Event::UserEvent(UserEvent::Refresh) => app.refresh(),
            Event::UserEvent(UserEvent::Menu(id)) => {
                // The native check item flips on every click, so the menu is rebuilt after any click, even on
                // an item that has no action.
                app.force_rebuild = true;
                app.last_notice = None;
                app.force_offer = None; // any click, including "Switch anyway" itself, uses up the offer
                match app.actions.get(&id).cloned() {
                    Some(action) => app.dispatch(action, flow),
                    None => app.refresh(),
                }
            }
            Event::UserEvent(UserEvent::Report(r)) => app.report(r),
            Event::UserEvent(UserEvent::Button { request_id, button }) => app.button(&request_id, &button),
            Event::UserEvent(UserEvent::Exit) => *flow = ControlFlow::Exit,
            _ => {}
        }
    })
}

fn spawn_ticker(proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(REFRESH_EVERY);
        if proxy.send_event(UserEvent::Refresh).is_err() {
            break;
        }
    });
}

fn button_forwarder(proxy: EventLoopProxy<UserEvent>) -> impl Fn(String, String) + 'static {
    move |request_id, button| {
        let _ = proxy.send_event(UserEvent::Button { request_id, button });
    }
}

struct App {
    paths: Paths,
    proxy: EventLoopProxy<UserEvent>,
    worker: Option<Handle>,
    tray: Option<TrayIcon>,
    notifier: Option<Notifier>,
    actions: HashMap<MenuId, Action>,
    shown: Vec<Entry>,
    /// Set by a click: the next refresh rebuilds even when the model is unchanged.
    force_rebuild: bool,
    /// The latest action result, kept in the menu because notifications may be denied.
    last_notice: Option<String>,
    /// A refused busy switch the menu can force; the menu drops it after `menu::FORCE_OFFER_SECS`.
    force_offer: Option<ForceOffer>,
    watcher_running: bool,
    counter: u64,
}

impl App {
    fn start(&mut self) {
        match icon::icon() {
            Ok(icon) => match TrayIconBuilder::new().with_icon(icon).with_icon_as_template(true).with_tooltip("agent-switch").build() {
                Ok(tray) => self.tray = Some(tray),
                Err(e) => log_line(&format!("cannot create the menu bar item: {e}")),
            },
            Err(e) => log_line(&format!("cannot build the icon: {e}")),
        }
        match Notifier::init(button_forwarder(self.proxy.clone())) {
            Ok(n) => self.notifier = Some(n),
            Err(why) => log_line(&why),
        }
        self.refresh();
    }

    /// Rebuild the menu when what it shows has changed, or when a click asked for it.
    fn refresh(&mut self) {
        let entries = current_menu(&self.paths, self.watcher_running, self.last_notice.as_deref(), self.force_offer.clone());
        if !menu::should_rebuild(&entries, &self.shown, self.force_rebuild) {
            return;
        }
        match menu::render(&entries) {
            Ok(rendered) => {
                if let Some(tray) = &self.tray {
                    tray.set_menu(Some(Box::new(rendered.menu)));
                }
                self.actions = rendered.actions;
                self.shown = entries;
                self.force_rebuild = false;
            }
            Err(e) => log_line(&format!("cannot build the menu: {e}")),
        }
    }

    fn dispatch(&mut self, action: Action, flow: &mut ControlFlow) {
        match action {
            Action::Quit => {
                if let Some(worker) = self.worker.take() {
                    if !worker.stop(STOP_WAIT) {
                        log_line("the watcher was still busy after 15 s; quitting anyway");
                    }
                }
                *flow = ControlFlow::Exit;
            }
            Action::SetLoginItem(on) => {
                if let Err(why) = login_item::set(on) {
                    self.notice("Start at login", &why);
                }
                self.refresh();
            }
            other => {
                if let Some(worker) = &self.worker {
                    worker.send(other);
                }
                self.refresh();
            }
        }
    }

    fn report(&mut self, report: Report) {
        match report {
            Report::Event(event) => match Config::load(&self.paths) {
                Ok(config) => {
                    if let Some(note) = notify::note_for(&event, &config) {
                        self.post(&note);
                    }
                }
                Err(e) => log_line(&format!("cannot read the config for a notification: {e}")),
            },
            Report::Notice { title, body } => self.notice(&title, &body),
            Report::Busy { kind, account, titles } => {
                self.force_offer = Some(ForceOffer { kind, account, titles, since: now_secs() });
                self.force_rebuild = true;
            }
            Report::Changed => self.refresh(),
            Report::Exited(reason) => {
                if reason != "stopped" {
                    log_line(&format!("the watcher thread ended: {reason}"));
                    self.watcher_running = false;
                    self.notice("agent-switch stopped watching", &reason);
                    self.refresh();
                }
            }
        }
    }

    /// A pressed notification button. Only "Relaunch now" / "Switch now" acts; the worker re-checks that
    /// the event is still current before it does anything.
    fn button(&mut self, request_id: &str, button: &str) {
        if button != notify::ACTION_RELAUNCH {
            return;
        }
        match (notify::parse_relaunch_id(request_id), &self.worker) {
            (Some((kind, event_id)), Some(worker)) => worker.send(Action::Relaunch { kind, event_id }),
            _ => log_line(&format!("ignored a button press on `{request_id}`")),
        }
    }

    fn notice(&mut self, title: &str, body: &str) {
        self.last_notice = Some(if title == "agent-switch" { body.to_string() } else { format!("{title}: {body}") });
        self.counter += 1;
        let note = Note::info(format!("{}-{}", now_secs(), self.counter), title, body);
        self.post(&note);
    }

    fn post(&self, note: &Note) {
        match &self.notifier {
            Some(n) => n.post(note),
            None => log_line(&format!("{}: {}", note.title, note.body)),
        }
    }
}

/// `--notify-test`: ask for permission, post one notification that carries the relaunch buttons, and log
/// what the system reports. A button press is logged and does nothing else.
fn run_notify_test() -> i32 {
    let event_loop = new_event_loop();
    let proxy = event_loop.create_proxy();
    let mut notifier: Option<Notifier> = None;
    let thread_proxy = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(20));
        let _ = thread_proxy.send_event(UserEvent::Exit);
    });
    let post_proxy = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        let _ = post_proxy.send_event(UserEvent::Refresh);
    });
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => match Notifier::init(button_forwarder(proxy.clone())) {
                Ok(n) => notifier = Some(n),
                Err(why) => {
                    log_line(&why);
                    *flow = ControlFlow::Exit;
                }
            },
            Event::UserEvent(UserEvent::Refresh) => {
                if let Some(n) = &notifier {
                    n.post(&Note {
                        id: notify::relaunch_id(agent_switch_core::config::SurfaceKind::ClaudeDesktop, "notify-test"),
                        title: "Relaunch Claude Desktop as Team? (test)".into(),
                        body: "This is a test of agent-switch notifications. The buttons do nothing.".into(),
                        buttons: notify::Buttons::Relaunch,
                    });
                    log_line("posted the test notification");
                }
            }
            Event::UserEvent(UserEvent::Button { request_id, button }) => log_line(&format!("button `{button}` pressed on `{request_id}`")),
            Event::UserEvent(UserEvent::Exit) => *flow = ControlFlow::Exit,
            _ => {}
        }
    })
}
