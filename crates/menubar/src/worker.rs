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

/// An adapter that may quit and reopen its app, because the user clicked for it.
fn consenting_surface(kind: SurfaceKind) -> Box<dyn Surface> {
    match kind {
        SurfaceKind::ClaudeDesktop => Box::new(ClaudeDesktop::production().with_restart()),
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
                let wakes_watcher = matches!(action, Action::SetAuto(true));
                let outcome = catch_unwind(AssertUnwindSafe(|| job.handle(action)));
                match outcome {
                    Ok(Ok(Some(body))) => report(Report::Notice { title: "agent-switch".into(), body }),
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => report(Report::Notice { title: "agent-switch could not finish".into(), body: e.to_string() }),
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

struct Job<'a> {
    env: &'a Env<'a>,
    consent: &'a Cell<bool>,
    watcher: Watcher<'a>,
}

impl Job<'_> {
    /// Do what a click asked for. `Ok(Some(text))` is a message for the user.
    fn handle(&mut self, action: Action) -> Result<Option<String>> {
        let paths = self.env.paths;
        match action {
            Action::Bind(kind, binding) => {
                let config = switch::bind(paths, kind, binding.clone())?;
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
            Action::SetAuto(on) => {
                Config::update(paths, |config| {
                    config.auto.enabled = on;
                    Ok(())
                })?;
                Ok(None)
            }
            Action::SetCodexMode(mode) => {
                Config::update(paths, |config| {
                    config.auto.codex_cli_mode = mode;
                    Ok(())
                })?;
                Ok(None)
            }
            Action::RelaunchApp(kind) => {
                let config = Config::load(paths)?;
                let id = bound_account(&config, kind)?;
                let surface = consenting_surface(kind);
                switch::activate(self.env, &config, surface.as_ref(), &id)?;
                surface.restart(self.env)?;
                Ok(Some(format!("{} is on {}.", surface_name(kind), label_of(&config, &id))))
            }
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
    fn auto_switch_and_codex_mode_are_saved_to_the_config() {
        let h = home();
        with_job(&h, |job| {
            job.handle(Action::SetAuto(true)).unwrap();
            job.handle(Action::SetCodexMode(agent_switch_core::config::CodexCliMode::Notify)).unwrap();
        });
        let c = Config::load(&h.paths).unwrap();
        assert!(c.auto.enabled);
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
    fn binding_a_surface_to_an_account_of_the_wrong_provider_is_refused() {
        let h = home();
        with_job(&h, |job| {
            assert!(job.handle(Action::Bind(SurfaceKind::CodexCli, Binding::parse("a"))).is_err());
        });
        assert_eq!(Config::load(&h.paths).unwrap().surfaces.codex_cli, Binding::Unmanaged);
    }
}
