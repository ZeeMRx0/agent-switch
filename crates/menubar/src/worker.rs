//! The background thread that hosts the watcher and runs every action that changes accounts.
//!
//! Only this thread touches the vault, the config and the state files for writing, so a menu click, a
//! notification button and an automatic switch never overlap. The UI thread sends `Cmd`s in and receives
//! `Report`s back; it never calls into the core directly.
//!
//! Quitting or restarting an app needs the user's consent. The watcher's adapter factory hands out adapters
//! that can quit an app only while `consent` is set, and `consent` is set only around a call that answers a
//! click (`Action::Relaunch`). An automatic pass therefore cannot quit Claude or ChatGPT.

use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use agent_switch_core::adapters::chatgpt_desktop::ChatgptDesktop;
use agent_switch_core::adapters::claude_desktop::{self, ClaudeDesktop};
use agent_switch_core::adapters::surface_for;
use agent_switch_core::config::{Binding, Config, SurfaceKind};
use agent_switch_core::macos_apps::SystemApps;
use agent_switch_core::paths::Paths;
use agent_switch_core::signing::detect_keychain;
use agent_switch_core::state::now_secs;
use agent_switch_core::surface::{Env, Surface};
use agent_switch_core::usage::LiveApi;
use agent_switch_core::vault::Vault;
use agent_switch_core::watcher::{Event, Relaunch, Watcher};
use agent_switch_core::{switch, Error, Result};

use crate::menu::{label_of, surface_name, Action};

pub enum Cmd {
    Do(Action),
    Stop,
}

/// What the worker tells the UI thread.
#[derive(Debug)]
pub enum Report {
    /// A watcher event, already written to the event log.
    Event(Event),
    /// Something the user should read: the result of a click, or why it did nothing.
    Notice { title: String, body: String },
    /// A Desktop switch was refused because these sessions are mid-reply; the menu offers "Switch anyway".
    Busy { kind: SurfaceKind, account: Option<String>, titles: String },
    /// Config or state may have changed.
    Changed,
    /// The thread ended; the text says why.
    Exited(String),
}

pub enum StartError {
    /// Another process holds the watcher lock.
    Busy(String),
    Failed(String),
}

pub struct Handle {
    tx: Sender<Cmd>,
    join: JoinHandle<()>,
}

impl Handle {
    pub fn send(&self, action: Action) {
        let _ = self.tx.send(Cmd::Do(action));
    }

    /// Ask the thread to finish its current job and stop; wait up to `wait` for it.
    pub fn stop(self, wait: Duration) -> bool {
        let _ = self.tx.send(Cmd::Stop);
        let deadline = std::time::Instant::now() + wait;
        while !self.join.is_finished() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.join.join();
        true
    }
}

/// Start the thread and wait until it holds the watcher lock (or learns that it cannot).
pub fn spawn(report: impl Fn(Report) + Send + Sync + 'static) -> std::result::Result<Handle, StartError> {
    let report: Arc<dyn Fn(Report) + Send + Sync> = Arc::new(report);
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let join = std::thread::Builder::new()
        .name("watcher".into())
        .spawn(move || run(report, rx, ready_tx))
        .map_err(|e| StartError::Failed(format!("cannot start the watcher thread: {e}")))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Handle { tx, join }),
        Ok(Err(start)) => {
            let _ = join.join();
            Err(start)
        }
        Err(_) => Err(StartError::Failed("the watcher thread ended before it started".into())),
    }
}

/// Claude Desktop, allowed to quit because the user clicked. `force` holds the session titles the user
/// agreed to end; a different busy session still refuses the switch.
fn claude_desktop_surface(force: Option<&str>) -> ClaudeDesktop {
    let desktop = ClaudeDesktop::production().with_restart();
    match force {
        Some(titles) => desktop.with_force_if_only(titles),
        None => desktop,
    }
}

/// An adapter that may quit and reopen its app, because the user clicked for it.
fn consenting_surface(kind: SurfaceKind) -> Box<dyn Surface> {
    match kind {
        SurfaceKind::ClaudeDesktop => Box::new(claude_desktop_surface(None)),
        // Other programs that use ~/.codex (an IDE extension, for example) are never stopped from here.
        SurfaceKind::ChatgptDesktop => Box::new(ChatgptDesktop::production().with_quit(Box::new(|_| false))),
        other => surface_for(other),
    }
}

fn run(report: Arc<dyn Fn(Report) + Send + Sync>, rx: mpsc::Receiver<Cmd>, ready: Sender<std::result::Result<(), StartError>>) {
    let setup = || -> Result<(Paths, Vault)> {
        let paths = Paths::from_env()?;
        let exe = std::env::current_exe().map_err(|e| Error::io("find the running executable", e))?;
        Ok((paths, Vault::new(Arc::from(detect_keychain(&exe)))))
    };
    let (paths, vault) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(StartError::Failed(e.to_string())));
            return;
        }
    };
    let env = Env { paths: &paths, vault: &vault };
    let api = LiveApi::production();
    let consent = Cell::new(false);
    let factory = |kind: SurfaceKind| -> Box<dyn Surface> {
        if consent.get() {
            consenting_surface(kind)
        } else {
            surface_for(kind)
        }
    };
    let mut watcher = match Watcher::start(&env, &api, &factory) {
        Ok(w) => w,
        Err(Error::LockBusy(m)) => {
            let _ = ready.send(Err(StartError::Busy(m)));
            return;
        }
        Err(e) => {
            let _ = ready.send(Err(StartError::Failed(e.to_string())));
            return;
        }
    };
    {
        let report = report.clone();
        watcher.on_event(move |e| report(Report::Event(e.clone())));
    }
    let _ = ready.send(Ok(()));

    let mut job = Job { env: &env, consent: &consent, watcher };
    let mut next_tick = 0u64;
    let reason = loop {
        let now = now_secs();
        if now >= next_tick {
            let result = catch_unwind(AssertUnwindSafe(|| job.watcher.tick(now)));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => crate::log_line(&format!("watcher pass failed: {e}")),
                Err(_) => break "the watcher pass panicked".to_string(),
            }
            next_tick = now_secs() + job.watcher.wait_secs(now_secs());
            report(Report::Changed);
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Cmd::Do(action)) => {
                let wakes_watcher = matches!(action, Action::SetSurfaceAuto(_, true));
                let forceable = forceable_kind(&action);
                let outcome = catch_unwind(AssertUnwindSafe(|| job.handle(action)));
                match outcome {
                    Ok(Ok(Some(body))) => report(Report::Notice { title: "agent-switch".into(), body }),
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => {
                        let text = e.to_string();
                        if let (Some((kind, account)), Some(titles)) = (forceable, claude_desktop::session_mirror::busy_titles_in(&text)) {
                            report(Report::Busy { kind, account, titles });
                        }
                        report(Report::Notice { title: "agent-switch could not finish".into(), body: text });
                    }
                    Err(_) => break "an action panicked".to_string(),
                }
                if wakes_watcher {
                    next_tick = 0;
                }
                report(Report::Changed);
            }
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break "stopped".to_string(),
            Err(RecvTimeoutError::Timeout) => {}
        }
    };
    report(Report::Exited(reason));
}

/// The surface (and, for an account click, the account) of a click whose busy refusal can be forced from
/// the menu. A refused account click is rolled back to the previous binding, so the offer must name the
/// account for "Switch anyway" to switch to it.
fn forceable_kind(action: &Action) -> Option<(SurfaceKind, Option<String>)> {
    match action {
        Action::RelaunchApp(kind @ SurfaceKind::ClaudeDesktop) => Some((*kind, None)),
        Action::SwitchApp(kind @ SurfaceKind::ClaudeDesktop, id) => Some((*kind, Some(id.clone()))),
        Action::ForceRelaunch { kind: kind @ SurfaceKind::ClaudeDesktop, account, .. } => Some((*kind, account.clone())),
        _ => None,
    }
}

struct Job<'a> {
    env: &'a Env<'a>,
    consent: &'a Cell<bool>,
    watcher: Watcher<'a>,
}

/// What a pick leaves once the switch is over. The priority follows the binding: a switch that went through
/// makes `id` the surface's first priority, and so does a failure that leaves the binding on `id` (the app
/// did not come up signed in, or putting the previous binding back failed). A binding that was put back
/// leaves the priority alone.
fn settle_pick(paths: &Paths, kind: SurfaceKind, id: &str, label: &str, switched: Result<Option<String>>) -> Result<Option<String>> {
    let cause = match switched {
        Ok(mirrored) => {
            switch::promote(paths, kind, id)?;
            return Ok(mirrored);
        }
        Err(cause) => cause,
    };
    let stays = Config::load(paths).is_ok_and(|c| c.surfaces.get(kind).account_id() == Some(id));
    let promoted = if stays { switch::promote(paths, kind, id).err().map_or_else(String::new, |e| format!(" {e}")) } else { String::new() };
    let text = match cause {
        // A failed switch already says what happened to the binding.
        Error::Invalid(text) if text.starts_with("Switching ") => text,
        other => format!("{} is bound to {label}, but switching failed: {other}", surface_name(kind)),
    };
    Err(Error::Invalid(format!("{text}{promoted}")))
}

impl Job<'_> {
    /// Point the app at `id` and restart it. The caller holds the user's consent for the restart.
    /// `force` holds the session titles the user agreed to end (Claude Desktop). When `rollback` is set and
    /// the switch is refused or fails before the app is touched, the binding goes back to it.
    /// Returns what session mirroring did (Claude Desktop only), for the message to the user.
    fn switch_and_restart(&self, config: &Config, kind: SurfaceKind, id: &str, force: Option<&str>, rollback: Option<&Binding>) -> Result<Option<String>> {
        let env = self.env;
        let activate = |surface: &dyn Surface| switch::activate(env, config, surface, id).map_err(|e| self.restore_binding(config, kind, id, rollback, e));
        if kind == SurfaceKind::ClaudeDesktop {
            let desktop = claude_desktop_surface(force);
            activate(&desktop)?;
            desktop.restart(env)?;
            return Ok(desktop.mirror_summary().or_else(|| desktop.mirror_warning().map(|w| format!("warning: {w}"))));
        }
        let surface = consenting_surface(kind);
        activate(surface.as_ref())?;
        surface.restart(env)?;
        Ok(None)
    }

    /// A failed switch must not leave the binding on an account the app never moved to. Returns the error
    /// to show, which says what happened to the binding.
    fn restore_binding(&self, config: &Config, kind: SurfaceKind, id: &str, rollback: Option<&Binding>, cause: Error) -> Error {
        let Some(previous) = rollback else { return cause };
        let label = label_of(config, id);
        let reason = cause.to_string();
        let reason = reason.trim_end_matches('.');
        let outcome = match switch::bind(self.env.paths, kind, previous.clone()) {
            Ok(restored) => {
                let was = previous.account_id().map_or_else(|| "unmanaged".to_string(), |p| label_of(&restored, p));
                format!("The binding was put back on {was}.")
            }
            Err(e) => format!("The binding stays on {label} because putting the previous one back failed: {e}."),
        };
        Error::Invalid(format!("Switching {} to {label} failed: {reason}. {outcome}", surface_name(kind)))
    }

    /// Bind `id` and switch the running app to it; a refusal restores the previous binding. Only a switch that
    /// went through makes `id` the surface's first priority, so a rollback has no priority to restore.
    fn switch_to(&self, kind: SurfaceKind, id: &str, force: Option<&str>) -> Result<Option<String>> {
        let paths = self.env.paths;
        let previous = Config::load(paths)?.surfaces.get(kind).clone();
        let config = switch::bind(paths, kind, Binding::Account(id.to_string()))?;
        let label = label_of(&config, id);
        let mirrored = settle_pick(paths, kind, id, &label, self.switch_and_restart(&config, kind, id, force, Some(&previous)))?;
        Ok(Some(on_account(kind, &label, mirrored)))
    }

    fn relaunch_bound(&self, kind: SurfaceKind, force: Option<&str>) -> Result<Option<String>> {
        let config = Config::load(self.env.paths)?;
        let id = bound_account(&config, kind)?;
        let mirrored = self.switch_and_restart(&config, kind, &id, force, None)?;
        Ok(Some(on_account(kind, &label_of(&config, &id), mirrored)))
    }

    /// Do what a click asked for. `Ok(Some(text))` is a message for the user.
    fn handle(&mut self, action: Action) -> Result<Option<String>> {
        let paths = self.env.paths;
        match action {
            Action::Bind(kind, binding) => {
                // The user's pick: bound and made the first priority, even if the activation below fails.
                let config = switch::choose(paths, kind, binding.clone())?;
                let Some(id) = binding.account_id() else {
                    return Ok(Some(format!("{} is unmanaged now; agent-switch will not touch it.", surface_name(kind))));
                };
                let label = label_of(&config, id);
                if kind.is_desktop() {
                    // Switching a Desktop app means quitting it, which only a Relaunch click may do.
                    return Ok(Some(format!("{} is bound to {label}. Choose Relaunch to switch the running app.", surface_name(kind))));
                }
                switch::activate(self.env, &config, surface_for(kind).as_ref(), id)
                    .map_err(|e| Error::Invalid(format!("{} is bound to {label}, but switching failed: {e}", surface_name(kind))))?;
                Ok(Some(format!("{} now uses {label}.", surface_name(kind))))
            }
            Action::SetSurfaceAuto(kind, on) => {
                Config::update(paths, |config| {
                    config.auto.set_enabled_for(kind, on);
                    Ok(())
                })?;
                Ok(None)
            }
            Action::MovePriority { kind, account, up } => {
                Config::update(paths, |config| config.move_priority(kind, &account, up))?;
                Ok(None)
            }
            Action::SetCodexMode(mode) => {
                Config::update(paths, |config| {
                    config.auto.codex_cli_mode = mode;
                    Ok(())
                })?;
                Ok(None)
            }
            Action::RelaunchApp(kind) => self.relaunch_bound(kind, None),
            Action::ForceRelaunch { kind, account: None, titles } => self.relaunch_bound(kind, Some(&titles)),
            Action::ForceRelaunch { kind, account: Some(id), titles } => self.switch_to(kind, &id, Some(&titles)),
            Action::SwitchApp(kind, id) => self.switch_to(kind, &id, None),
            Action::OpenClaude => {
                let config = Config::load(paths)?;
                let id = bound_account(&config, SurfaceKind::ClaudeDesktop)?;
                let label = label_of(&config, &id);
                let opened = claude_desktop::open_account_profile(&SystemApps, paths, &id)?;
                Ok(Some(if opened { format!("Opened Claude on {label}.") } else { format!("Claude is already open on {label}.") }))
            }
            Action::Undo(kind) => {
                let config = Config::load(paths)?;
                let restored = switch::undo(self.env, &config, surface_for(kind).as_ref())?;
                Ok(Some(format!("{} is back on {}.", surface_name(kind), label_of(&config, &restored))))
            }
            Action::Relaunch { kind, event_id } => {
                self.consent.set(true);
                let result = self.watcher.relaunch(kind, &event_id, now_secs());
                self.consent.set(false);
                let config = Config::load(paths)?;
                Ok(Some(match result? {
                    Relaunch::Done { to, .. } => format!("{} is on {}.", surface_name(kind), label_of(&config, &to)),
                    Relaunch::Stale(why) => format!("Nothing changed: {why}."),
                }))
            }
            Action::SetLoginItem(_) | Action::Quit => Ok(None), // handled on the UI thread
        }
    }
}

/// "Claude Desktop is on Team." plus the session-mirroring result when there is one.
fn on_account(kind: SurfaceKind, label: &str, mirrored: Option<String>) -> String {
    match mirrored {
        Some(m) => format!("{} is on {label}. {m}", surface_name(kind)),
        None => format!("{} is on {label}.", surface_name(kind)),
    }
}

fn bound_account(config: &Config, kind: SurfaceKind) -> Result<String> {
    match config.surfaces.get(kind) {
        Binding::Account(id) => Ok(id.clone()),
        Binding::Unmanaged => Err(Error::Invalid(format!("{} is unmanaged; choose an account for it first", surface_name(kind)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_switch_core::config::{Account, Provider};
    use agent_switch_core::keychain::MemoryKeychain;
    use agent_switch_core::usage::LiveApi;

    /// A real home in a temp folder with fake keychain; the worker's pure handlers run against it.
    struct Home {
        _dir: tempfile::TempDir,
        paths: Paths,
        vault: Vault,
    }

    fn home() -> Home {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(dir.path());
        let mut config = Config::default();
        for (id, provider) in [("a", Provider::Claude), ("b", Provider::Claude)] {
            config.add_account(Account { id: id.into(), provider, label: id.to_uppercase(), email: None, uuid: None }).unwrap();
        }
        config.save(&paths).unwrap();
        Home { _dir: dir, paths, vault: Vault::new(Arc::new(MemoryKeychain::new())) }
    }

    fn with_job<T>(h: &Home, body: impl FnOnce(&mut Job) -> T) -> T {
        let env = Env { paths: &h.paths, vault: &h.vault };
        let api = LiveApi::production();
        let consent = Cell::new(false);
        let factory = |k: SurfaceKind| surface_for(k);
        let watcher = Watcher::new(&env, &api, &factory);
        let mut job = Job { env: &env, consent: &consent, watcher };
        body(&mut job)
    }

    #[test]
    fn surface_auto_switch_priority_and_codex_mode_are_saved_to_the_config() {
        let h = home();
        with_job(&h, |job| {
            job.handle(Action::SetSurfaceAuto(SurfaceKind::ClaudeCli, true)).unwrap();
            job.handle(Action::MovePriority { kind: SurfaceKind::ClaudeCli, account: "b".into(), up: true }).unwrap();
            job.handle(Action::SetCodexMode(agent_switch_core::config::CodexCliMode::Notify)).unwrap();
            // An account that is not in the list (here: the wrong provider's surface) changes nothing.
            assert!(job.handle(Action::MovePriority { kind: SurfaceKind::CodexCli, account: "a".into(), up: true }).is_err());
        });
        let c = Config::load(&h.paths).unwrap();
        assert!(c.auto.enabled_for(SurfaceKind::ClaudeCli));
        assert!(!c.auto.enabled_for(SurfaceKind::ClaudeDesktop), "the other surfaces keep their value");
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["b", "a"]);
        assert_eq!(c.priority(SurfaceKind::ClaudeDesktop), ["a", "b"], "each surface has its own order");
        assert_eq!(c.auto.codex_cli_mode, agent_switch_core::config::CodexCliMode::Notify);
    }

    #[test]
    fn choosing_unmanaged_and_a_desktop_account_only_change_the_binding() {
        let h = home();
        with_job(&h, |job| {
            let said = job.handle(Action::Bind(SurfaceKind::ClaudeDesktop, Binding::parse("b"))).unwrap().unwrap();
            assert!(said.contains("bound to B") && said.contains("Relaunch"), "{said}");
            let said = job.handle(Action::Bind(SurfaceKind::ClaudeCli, Binding::Unmanaged)).unwrap().unwrap();
            assert!(said.contains("unmanaged"), "{said}");
        });
        let c = Config::load(&h.paths).unwrap();
        assert_eq!(c.surfaces.claude_desktop, Binding::parse("b"));
        assert_eq!(c.priority.claude_desktop, ["b", "a"], "a pick is the first priority");
    }

    #[test]
    fn picking_a_cli_account_makes_it_the_first_priority_even_if_activation_fails() {
        let h = home();
        with_job(&h, |job| {
            // No saved login in the fake keychain, so the activation fails; the choice stays.
            assert!(job.handle(Action::Bind(SurfaceKind::ClaudeCli, Binding::parse("b"))).is_err());
        });
        let c = Config::load(&h.paths).unwrap();
        assert_eq!(c.surfaces.claude_cli, Binding::parse("b"));
        assert_eq!(c.priority.claude_cli, ["b", "a"]);
    }

    #[test]
    fn a_desktop_pick_that_switched_but_did_not_come_up_keeps_the_binding_and_is_the_first_priority() {
        let h = home();
        switch::bind(&h.paths, SurfaceKind::ClaudeDesktop, Binding::parse("b")).unwrap();
        let came_up_late = Err(Error::Invalid("Claude did not come up signed in".into()));
        let said = settle_pick(&h.paths, SurfaceKind::ClaudeDesktop, "b", "B", came_up_late).unwrap_err().to_string();
        assert!(said.starts_with("Claude Desktop is bound to B, but switching failed"), "{said}");
        assert_eq!(Config::load(&h.paths).unwrap().priority.claude_desktop, ["b", "a"]);

        // A refused switch put the binding back, so nothing moves up.
        let refused = Err(Error::Invalid("Switching Claude Desktop to A failed: no saved login. The binding was put back on B.".into()));
        assert!(settle_pick(&h.paths, SurfaceKind::ClaudeDesktop, "a", "A", refused).is_err());
        assert_eq!(Config::load(&h.paths).unwrap().priority.claude_desktop, ["b", "a"]);
    }

    #[test]
    fn a_desktop_pick_whose_binding_could_not_be_put_back_is_the_first_priority() {
        let h = home();
        switch::bind(&h.paths, SurfaceKind::ClaudeDesktop, Binding::parse("b")).unwrap();
        let stuck = Err(Error::Invalid(
            "Switching Claude Desktop to B failed: no saved login. The binding stays on B because putting the previous one back failed: locked.".into(),
        ));
        let said = settle_pick(&h.paths, SurfaceKind::ClaudeDesktop, "b", "B", stuck).unwrap_err().to_string();
        assert!(said.starts_with("Switching Claude Desktop to B failed"), "{said}");
        assert_eq!(Config::load(&h.paths).unwrap().priority.claude_desktop, ["b", "a"]);
    }

    #[test]
    fn relaunch_and_open_need_a_bound_app_and_a_click_for_a_stale_event_changes_nothing() {
        let h = home();
        with_job(&h, |job| {
            assert!(job.handle(Action::RelaunchApp(SurfaceKind::ChatgptDesktop)).unwrap_err().to_string().contains("unmanaged"));
            assert!(job.handle(Action::OpenClaude).unwrap_err().to_string().contains("unmanaged"));
            let said = job.handle(Action::Relaunch { kind: SurfaceKind::ClaudeDesktop, event_id: "gone".into() }).unwrap().unwrap();
            assert!(said.starts_with("Nothing changed:") && said.contains("no longer current"), "{said}");
            assert!(!job.consent.get(), "consent is given only during the click");
        });
    }

    #[test]
    fn switching_a_desktop_app_to_an_account_of_the_wrong_provider_is_refused_before_anything_restarts() {
        let h = home();
        with_job(&h, |job| {
            assert!(job.handle(Action::SwitchApp(SurfaceKind::ChatgptDesktop, "a".into())).is_err());
            assert!(job.handle(Action::SwitchApp(SurfaceKind::ClaudeDesktop, "missing".into())).is_err());
        });
        let c = Config::load(&h.paths).unwrap();
        assert_eq!(c.surfaces.chatgpt_desktop, Binding::Unmanaged);
        assert_eq!(c.surfaces.claude_desktop, Binding::Unmanaged);
    }

    #[test]
    fn binding_a_surface_to_an_account_of_the_wrong_provider_is_refused() {
        let h = home();
        with_job(&h, |job| {
            assert!(job.handle(Action::Bind(SurfaceKind::CodexCli, Binding::parse("a"))).is_err());
        });
        assert_eq!(Config::load(&h.paths).unwrap().surfaces.codex_cli, Binding::Unmanaged);
    }

    #[test]
    fn force_relaunch_uses_forced_surface() {
        assert!(claude_desktop_surface(Some("Fix login")).is_forced());
        assert!(!claude_desktop_surface(None).is_forced(), "a plain relaunch never ends work in progress");
        let h = home();
        with_job(&h, |job| {
            // It needs a bound account, like Relaunch, and only a busy refusal of Claude Desktop is forceable.
            let force = Action::ForceRelaunch { kind: SurfaceKind::ClaudeDesktop, account: None, titles: "Fix login".into() };
            assert!(job.handle(force).unwrap_err().to_string().contains("unmanaged"));
        });
        let desktop = SurfaceKind::ClaudeDesktop;
        assert_eq!(forceable_kind(&Action::RelaunchApp(desktop)), Some((desktop, None)));
        assert_eq!(forceable_kind(&Action::SwitchApp(desktop, "a".into())), Some((desktop, Some("a".into()))));
        let again = Action::ForceRelaunch { kind: desktop, account: Some("a".into()), titles: "t".into() };
        assert_eq!(forceable_kind(&again), Some((desktop, Some("a".into()))));
        assert_eq!(forceable_kind(&Action::RelaunchApp(SurfaceKind::ChatgptDesktop)), None);
        assert_eq!(forceable_kind(&Action::Relaunch { kind: desktop, event_id: "e".into() }), None);
    }

    #[test]
    fn a_failed_account_click_puts_the_previous_binding_back() {
        let h = home();
        with_job(&h, |job| {
            // No Desktop profile exists for `a` or `b`, so the switch fails before anything is quit.
            job.handle(Action::Bind(SurfaceKind::ClaudeDesktop, Binding::parse("b"))).unwrap();
            let err = job.handle(Action::SwitchApp(SurfaceKind::ClaudeDesktop, "a".into())).unwrap_err().to_string();
            assert!(err.contains("Switching Claude Desktop to A failed") && err.contains("put back on B"), "{err}");
            let c = Config::load(&h.paths).unwrap();
            assert_eq!(c.surfaces.claude_desktop, Binding::parse("b"));
            assert_eq!(c.priority(SurfaceKind::ClaudeDesktop), ["b", "a"], "a failed pick is not promoted");
        });
        let h = home();
        with_job(&h, |job| {
            let err = job.handle(Action::SwitchApp(SurfaceKind::ClaudeDesktop, "a".into())).unwrap_err().to_string();
            assert!(err.contains("put back on unmanaged"), "{err}");
            let c = Config::load(&h.paths).unwrap();
            assert_eq!(c.surfaces.claude_desktop, Binding::Unmanaged);
            assert!(c.priority.claude_desktop.is_empty(), "a failed pick writes no priority");
        });
    }

    #[test]
    fn a_session_summary_follows_the_switch_message() {
        assert_eq!(on_account(SurfaceKind::ClaudeDesktop, "Team", Some("sessions: 2 mirrored, 0 backed up".into())), "Claude Desktop is on Team. sessions: 2 mirrored, 0 backed up");
        assert_eq!(on_account(SurfaceKind::ChatgptDesktop, "Team", None), "ChatGPT is on Team.");
    }
}
