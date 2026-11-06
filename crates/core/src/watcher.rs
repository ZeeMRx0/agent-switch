//! The watcher: one pass (`tick`) polls the accounts that are due, then, with auto-switch on for at least one
//! surface, asks the policy what to do and does it. `run` repeats the pass until told to stop.
//!
//! * CLI surfaces switch at once: the candidate is freshened (refreshed when its login expires within ten
//!   minutes), read again and checked against the headroom rule, and only then activated.
//! * Desktop surfaces (and the Codex CLI in `notify` mode) get a switch-needed event, stored as a pending
//!   record with an id and a 15 minute lifetime. Nothing is quit until `relaunch` runs for that id.
//! * Bound to the same account, the Codex CLI splits into its own home first. A rejoin record is written
//!   before the switch, so a restart in between still knows the CLI is split; `State::record_switch` clears
//!   it when the app reaches the same account.
//! * The event log holds no secrets and only this process writes it; `start` takes the singleton lock.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Account, Config, Provider, SurfaceKind};
use crate::error::{Error, Result};
use crate::fsutil::ensure_private_dir;
use crate::locks::WatcherLock;
use crate::paths::Paths;
use crate::policy::{self, Decision, Hold, Outcome};
use crate::state::{now_secs, Pending, State, UsageState, REASON_REJOIN, REASON_SWITCH_NEEDED};
use crate::surface::Env;
use crate::switch;
use crate::token_refresh::{self, FreshenError, FRESHEN_WITHIN_S};
use crate::usage::{self, ProviderApi, SurfaceFactory};

/// How long a switch-needed event stays valid.
pub const EVENT_TTL_S: u64 = 15 * 60;
const LOG_ROTATE_BYTES: u64 = 1 << 20;
/// The same "no candidate" notice is not repeated more often than this.
const NO_CANDIDATE_REPEAT_S: u64 = 30 * 60;
const MAX_SLEEP_S: u64 = 60;
const MIN_SLEEP_S: u64 = 5;
/// Wait after the 1st, 2nd, 3rd and later failed switch to the same target while the live login is the same:
/// 1 minute, 5 minutes, 30 minutes, then 6 hours.
const FAILURE_BACKOFF_S: [u64; 4] = [60, 5 * 60, 30 * 60, 6 * 3600];

/// A switch attempt that failed. The key is (surface, target, the live login's identity): a different
/// target or a changed live login starts afresh.
struct Failure {
    kind: SurfaceKind,
    target: String,
    live: String,
    count: u32,
    retry_at: u64,
    last_error: String,
}

// ---------------------------------------------------------------------------------------------
// Event log
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Switched,
    SwitchNeeded,
    SwitchFailed,
    NoCandidate,
    AccountDead,
    RefreshHalted,
    AutoPaused,
    AutoResumed,
    Rejoined,
}

/// One line of `events.jsonl`. Ids, account names and short messages only; never a token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub ts: u64,
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The worst window of `from`, in percent, when the event was caused by a reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct: Option<f64>,
    /// The switch (or switch-needed event) returns the surface to a higher-priority account.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub back: bool,
}

impl Event {
    fn new(ts: u64, kind: EventKind) -> Event {
        Event { ts, kind, surface: None, from: None, to: None, id: None, expires_at: None, detail: None, pct: None, back: false }
    }
    fn pct(mut self, pct: Option<f64>) -> Event {
        self.pct = pct;
        self
    }
    fn surface(mut self, k: SurfaceKind) -> Event {
        self.surface = Some(k.name().to_string());
        self
    }
    fn from(mut self, f: &str) -> Event {
        self.from = Some(f.to_string());
        self
    }
    fn to(mut self, t: &str) -> Event {
        self.to = Some(t.to_string());
        self
    }
    fn detail(mut self, d: impl Into<String>) -> Event {
        self.detail = Some(d.into());
        self
    }
}

pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn new(paths: &Paths) -> EventLog {
        EventLog { path: paths.events_file() }
    }

    /// Append one line. The file is 0600 in a private folder and moves to `events.jsonl.1` past 1 MiB.
    pub fn append(&self, event: &Event) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            ensure_private_dir(dir)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= LOG_ROTATE_BYTES) {
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            std::fs::rename(&self.path, PathBuf::from(old)).map_err(|e| Error::io("rotate the event log", e))?;
        }
        let mut line = serde_json::to_string(event).map_err(|e| Error::Invalid(e.to_string()))?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|e| Error::io(format!("open {}", self.path.display()), e))?;
        f.write_all(line.as_bytes()).map_err(|e| Error::io("write the event log", e))
    }
}

// ---------------------------------------------------------------------------------------------
// Watcher
// ---------------------------------------------------------------------------------------------

/// What a click on a switch-needed event led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relaunch {
    Done { from: String, to: String },
    /// The event is outdated or no longer sensible; nothing was changed (except dropping the event).
    Stale(String),
}

type EventSink<'a> = Box<dyn Fn(&Event) + 'a>;

pub struct Watcher<'a> {
    env: &'a Env<'a>,
    api: &'a dyn ProviderApi,
    surfaces: SurfaceFactory<'a>,
    log: EventLog,
    _lock: Option<WatcherLock>,
    paused: Vec<Provider>,
    last_no_candidate: Vec<(SurfaceKind, u64)>,
    failures: Vec<Failure>,
    sink: Option<EventSink<'a>>,
}

/// What `prepare` found out about a candidate.
enum Prepared {
    Ready,
    /// Not a failure: the candidate simply has no room (or no reading).
    NotConfirmed,
    Failed { kind: EventKind, detail: String },
}

impl<'a> Watcher<'a> {
    /// The singleton watcher: fails with `LockBusy` while another one runs.
    pub fn start(env: &'a Env<'a>, api: &'a dyn ProviderApi, surfaces: SurfaceFactory<'a>) -> Result<Watcher<'a>> {
        let lock = WatcherLock::try_acquire(env.paths)?;
        let mut w = Watcher::new(env, api, surfaces);
        w._lock = Some(lock);
        Ok(w)
    }

    /// A watcher without the singleton lock, for callers that already hold it and for tests.
    pub fn new(env: &'a Env<'a>, api: &'a dyn ProviderApi, surfaces: SurfaceFactory<'a>) -> Watcher<'a> {
        Watcher { env, api, surfaces, log: EventLog::new(env.paths), _lock: None, paused: Vec::new(), last_no_candidate: Vec::new(), failures: Vec::new(), sink: None }
    }

    /// Also hand every event to `sink` after it is logged (the menu bar turns them into notifications).
    pub fn on_event(&mut self, sink: impl Fn(&Event) + 'a) {
        self.sink = Some(Box::new(sink));
    }

    fn emit(&self, event: Event) {
        // A full disk must not stop the watcher; the state file stays the source of truth.
        if let Err(e) = self.log.append(&event) {
            eprintln!("agent-switch: cannot write the event log: {e}");
        }
        if let Some(sink) = &self.sink {
            sink(&event);
        }
    }

    fn usage_ctx<'b>(&'b self, config: &'b Config) -> usage::Ctx<'b> {
        usage::Ctx { env: self.env, config, api: self.api, surfaces: self.surfaces }
    }

    fn refresh_ctx(&self) -> token_refresh::Ctx<'_> {
        token_refresh::Ctx { env: self.env, api: self.api }
    }

    /// Repeat `tick` until `stop` is set. Sleeps until the next account is due, at most a minute at a time.
    pub fn run(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            let now = now_secs();
            if let Err(e) = self.tick(now) {
                eprintln!("agent-switch: watcher pass failed: {e}");
            }
            let wait = self.wait_secs(now);
            for _ in 0..wait {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    /// Seconds to wait before the next pass: until the next account is due, between 5 and 60.
    pub fn wait_secs(&self, now: u64) -> u64 {
        let due = match (Config::load(self.env.paths), State::load(self.env.paths)) {
            (Ok(c), Ok(s)) => policy::next_due(&c, &s, now),
            _ => now + MAX_SLEEP_S,
        };
        due.saturating_sub(now_secs()).clamp(MIN_SLEEP_S, MAX_SLEEP_S)
    }

    /// One pass at time `now`.
    pub fn tick(&mut self, now: u64) -> Result<()> {
        let config = Config::load(self.env.paths)?;
        self.poll_due(&config, now);
        self.drop_expired_events(now)?;
        if !config.auto.any_enabled() {
            return Ok(());
        }
        self.update_pause(&config, now)?;
        for kind in SurfaceKind::ALL {
            // Fresh config and state for each surface: polling takes seconds, and a bind or switch from the
            // terminal (which also changes the priority) or an earlier switch in this pass may have landed.
            let config = Config::load(self.env.paths)?;
            let state = State::load(self.env.paths)?;
            let Some(decision) = policy::decide(&config, &state, now).into_iter().find(|d| d.surface == kind) else { continue };
            self.act(&config, decision, now)?;
        }
        Ok(())
    }

    fn poll_due(&self, config: &Config, now: u64) {
        let ctx = self.usage_ctx(config);
        for account in &config.accounts {
            let before = State::load(self.env.paths).ok().and_then(|s| s.usage.get(&account.id).cloned());
            if before.as_ref().is_some_and(|r| now < r.next_poll_at) {
                continue;
            }
            match usage::poll_account(&ctx, account, now) {
                Ok(rec) => {
                    if rec.state == UsageState::Dead && before.as_ref().is_none_or(|b| b.state != UsageState::Dead) {
                        self.emit(Event::new(now, EventKind::AccountDead).to(&account.id).detail(rec.detail.unwrap_or_default()));
                    }
                }
                Err(e) => eprintln!("agent-switch: cannot read usage of `{}`: {e}", account.id),
            }
        }
    }

    fn drop_expired_events(&self, now: u64) -> Result<()> {
        State::update(self.env.paths, |s| {
            for kind in SurfaceKind::ALL {
                if s.pending(kind).is_some_and(|p| p.reason == REASON_SWITCH_NEEDED && p.is_expired(now)) {
                    s.set_pending(kind, None);
                }
            }
        })
    }

    /// Auto mode pauses for a provider whose every reading is stale, and resumes when one is fresh again.
    fn update_pause(&mut self, config: &Config, now: u64) -> Result<()> {
        let state = State::load(self.env.paths)?;
        let now_paused = policy::paused_providers(config, &state, now);
        for p in &now_paused {
            if !self.paused.contains(p) {
                self.emit(Event::new(now, EventKind::AutoPaused).detail(format!(
                    "auto mode is paused for {}: no account has a usage reading from the last {} minutes",
                    p,
                    policy::STALE_AFTER_S / 60
                )));
            }
        }
        for p in &self.paused {
            if !now_paused.contains(p) {
                self.emit(Event::new(now, EventKind::AutoResumed).detail(format!("auto mode resumed for {}", p)));
            }
        }
        self.paused = now_paused;
        Ok(())
    }

    fn act(&mut self, config: &Config, decision: Decision, now: u64) -> Result<()> {
        let kind = decision.surface;
        match decision.outcome {
            Outcome::Switch { from, candidates, split, back } => self.switch_now(config, kind, &from, &candidates, split, back, decision.pct, now),
            Outcome::Notify { from, candidates, follows_cli, back } => self.raise_event(config, kind, &from, &candidates, follows_cli, back, decision.pct, now),
            Outcome::Hold(Hold::NoCandidate) => {
                if self.last_no_candidate.iter().any(|(k, t)| *k == kind && now.saturating_sub(*t) < NO_CANDIDATE_REPEAT_S) {
                    return Ok(());
                }
                self.last_no_candidate.retain(|(k, _)| *k != kind);
                self.last_no_candidate.push((kind, now));
                let from = policy::current_account(config, &State::load(self.env.paths)?, kind).unwrap_or_default();
                self.emit(Event::new(now, EventKind::NoCandidate).surface(kind).from(&from).pct(decision.pct).detail("the account is near its limit and no other account has room"));
                Ok(())
            }
            Outcome::Hold(_) => Ok(()),
        }
    }

    /// Freshen and re-read `account` to see whether it is safe to switch to.
    fn prepare(&self, config: &Config, account: &Account, now: u64) -> Prepared {
        match token_refresh::freshen(&self.refresh_ctx(), account, now, FRESHEN_WITHIN_S) {
            Ok(_) => {}
            Err(FreshenError::Dead(why)) => return Prepared::Failed { kind: EventKind::AccountDead, detail: why },
            Err(FreshenError::Halted(why)) => return Prepared::Failed { kind: EventKind::RefreshHalted, detail: why },
            Err(e) => return Prepared::Failed { kind: EventKind::SwitchFailed, detail: format!("cannot prepare the login: {e}") },
        }
        match usage::poll_account(&self.usage_ctx(config), account, now) {
            Ok(rec) if policy::confirmed(&rec, &config.auto, now) => Prepared::Ready,
            _ => Prepared::NotConfirmed,
        }
    }

    /// Who is logged in on `kind` right now, as a comparable string kept in memory only. A failure is
    /// remembered against it, so a changed login (the user signed in again) is tried at once.
    fn live_key(&self, kind: SurfaceKind) -> String {
        match (self.surfaces)(kind).current_identity(self.env) {
            Ok(Some(i)) => format!("{:?}|{:?}|{:?}|{:?}", i.account_id, i.uuid, i.email, i.note),
            Ok(None) => "logged-out".to_string(),
            Err(e) => format!("unreadable: {e}"),
        }
    }

    fn backing_off(&self, kind: SurfaceKind, target: &str, live: &str, now: u64) -> bool {
        self.failures.iter().any(|f| f.kind == kind && f.target == target && f.live == live && now < f.retry_at)
    }

    /// Remember a failed attempt and schedule the next one. The event is raised only when the error differs
    /// from the one reported for the same key last time.
    fn record_failure(&mut self, kind: SurfaceKind, target: &str, live: &str, now: u64, event: Event) {
        let message = format!("{:?}: {}", event.kind, event.detail.as_deref().unwrap_or_default());
        let known = self.failures.iter().position(|f| f.kind == kind && f.target == target && f.live == live);
        let announce = match known {
            Some(i) => {
                let f = &mut self.failures[i];
                f.count += 1;
                f.retry_at = now + FAILURE_BACKOFF_S[(f.count as usize - 1).min(FAILURE_BACKOFF_S.len() - 1)];
                let changed = f.last_error != message;
                f.last_error = message;
                changed
            }
            None => {
                self.failures.push(Failure { kind, target: target.to_string(), live: live.to_string(), count: 1, retry_at: now + FAILURE_BACKOFF_S[0], last_error: message });
                true
            }
        };
        if announce {
            self.emit(event);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn switch_now(&mut self, config: &Config, kind: SurfaceKind, from: &str, candidates: &[String], split: bool, back: bool, pct: Option<f64>, now: u64) -> Result<()> {
        let live = self.live_key(kind);
        // A changed live login clears what was remembered about this surface.
        self.failures.retain(|f| f.kind != kind || f.live == live);
        for id in candidates {
            let Some(account) = config.account(id) else { continue };
            if self.backing_off(kind, id, &live, now) {
                continue;
            }
            match self.prepare(config, account, now) {
                Prepared::Ready => {}
                Prepared::NotConfirmed => continue,
                Prepared::Failed { kind: event_kind, detail } => {
                    self.record_failure(kind, id, &live, now, Event::new(now, event_kind).surface(kind).to(id).detail(detail));
                    continue;
                }
            }
            let rejoin_id = format!("{now:x}-rejoin-{id}");
            if split {
                let record = Pending { id: rejoin_id.clone(), target_account: id.clone(), reason: REASON_REJOIN.into(), from: Some(from.to_string()), created_at: now, expires_at: None };
                State::update(self.env.paths, |s| s.set_pending(kind, Some(record)))?;
            }
            let surface = (self.surfaces)(kind);
            match switch::activate(self.env, config, surface.as_ref(), id) {
                Ok(()) => {
                    self.failures.retain(|f| f.kind != kind);
                    State::update(self.env.paths, |s| {
                        s.last_auto_switch.insert(kind, now);
                    })?;
                    let mut detail = match pct {
                        _ if back => return_detail(id),
                        Some(p) => format!("{from} was at {p:.0}%"),
                        None => format!("{from} was at its limit"),
                    };
                    if kind == SurfaceKind::CodexCli {
                        detail.push_str(match config.auto.codex_cli_mode {
                            crate::config::CodexCliMode::Force => "; running Codex sessions may need a new sign-in",
                            _ => "; running Codex sessions keep the old account until restarted",
                        });
                    }
                    if split {
                        detail.push_str("; the Codex CLI uses its own home until the ChatGPT app moves to the same account");
                    }
                    let mut event = Event::new(now, EventKind::Switched).surface(kind).from(from).to(id).detail(detail).pct(pct);
                    event.back = back;
                    self.emit(event);
                    return Ok(());
                }
                Err(e) => {
                    if split {
                        State::update(self.env.paths, |s| {
                            if s.pending(kind).is_some_and(|p| p.id == rejoin_id) {
                                s.set_pending(kind, None);
                            }
                        })?;
                    }
                    self.record_failure(kind, id, &live, now, Event::new(now, EventKind::SwitchFailed).surface(kind).from(from).to(id).detail(e.to_string()));
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn raise_event(&mut self, config: &Config, kind: SurfaceKind, from: &str, candidates: &[String], follows_cli: bool, back: bool, pct: Option<f64>, now: u64) -> Result<()> {
        let Some(target) = candidates.first() else { return Ok(()) };
        let id = format!("{now:x}-{}-{target}", kind.name());
        let expires_at = now + EVENT_TTL_S;
        let record = Pending { id: id.clone(), target_account: target.clone(), reason: REASON_SWITCH_NEEDED.into(), from: Some(from.to_string()), created_at: now, expires_at: Some(expires_at) };
        State::update(self.env.paths, |s| s.set_pending(kind, Some(record)))?;
        let mut detail = if back { return_detail(target) } else { format!("{from} is near its limit") };
        if follows_cli {
            detail.push_str("; relaunching the app also brings the Codex CLI back to the shared home");
        }
        if kind == SurfaceKind::CodexCli {
            detail.push_str(&format!("; codex_cli_mode is {:?}: running sessions keep the old account until restarted", config.auto.codex_cli_mode));
        }
        let mut event = Event::new(now, EventKind::SwitchNeeded).surface(kind).from(from).to(target).detail(detail).pct(pct);
        event.id = Some(id);
        event.expires_at = Some(expires_at);
        event.back = back;
        self.emit(event);
        Ok(())
    }

    /// The core of the "Relaunch now" click: act on the pending switch-needed event `event_id` of `kind`.
    ///
    /// A stale event changes nothing: it must still be the pending one, within its lifetime, the surface must
    /// still be on the account it was raised for, and the target must still exist, belong to the surface's
    /// provider, not be dead and not be live on another surface that owns its token (another switch may have
    /// moved there since the event was raised). The target is freshened and read again; an unreadable usage endpoint does not
    /// block the switch the user asked for, but a target that is now full does.
    pub fn relaunch(&mut self, kind: SurfaceKind, event_id: &str, now: u64) -> Result<Relaunch> {
        let config = Config::load(self.env.paths)?;
        let state = State::load(self.env.paths)?;
        let stale = |this: &Watcher, why: &str, clear: bool| -> Result<Relaunch> {
            if clear {
                State::update(this.env.paths, |s| {
                    if s.pending(kind).is_some_and(|p| p.id == event_id) {
                        s.set_pending(kind, None);
                    }
                })?;
            }
            Ok(Relaunch::Stale(why.to_string()))
        };
        let Some(p) = state.pending(kind).filter(|p| p.reason == REASON_SWITCH_NEEDED && p.id == event_id).cloned() else {
            return stale(self, "this notification is no longer current", false);
        };
        if p.is_expired(now) {
            return stale(self, "this notification expired", true);
        }
        if config.surfaces.get(kind).account_id().is_none() {
            return stale(self, "the surface is no longer managed by agent-switch", true);
        }
        let current = policy::current_account(&config, &state, kind);
        if p.from.is_some() && current != p.from {
            return stale(self, "the surface is no longer on the account this notification was about", true);
        }
        let Some(target) = config.account(&p.target_account).filter(|a| a.provider == kind.provider()) else {
            return stale(self, "the target account no longer exists", true);
        };
        if state.usage.get(&target.id).is_some_and(|r| r.state == UsageState::Dead) {
            return stale(self, "the target account needs a new sign-in", true);
        }
        if policy::live_elsewhere(&state, kind, &target.id) {
            return stale(self, "the target account is now in use on another surface", true);
        }
        match token_refresh::freshen(&self.refresh_ctx(), target, now, FRESHEN_WITHIN_S) {
            Ok(_) => {}
            Err(FreshenError::Dead(_)) => return stale(self, "the target account needs a new sign-in", true),
            Err(e) => return Err(Error::Invalid(format!("cannot prepare `{}`: {e}", target.id))),
        }
        if let Ok(rec) = usage::poll_account(&self.usage_ctx(&config), target, now) {
            if rec.state == UsageState::Ok && !policy::has_headroom(&rec.windows, &config.auto, now) {
                return stale(self, "the target account is now close to its limit too", true);
            }
        }

        // The app writes the shared `~/.codex`, so a Codex CLI that shares it moves along.
        let moves_cli = kind == SurfaceKind::ChatgptDesktop && policy::codex_shares_home(&config, &state);
        let surface = (self.surfaces)(kind);
        // The event was raised for a running app; a quit app has nothing to relaunch.
        if surface.is_running(self.env).ok().flatten() == Some(false) {
            return stale(self, "the app is not running any more, so there is nothing to relaunch", true);
        }
        switch::activate(self.env, &config, surface.as_ref(), &target.id)?;
        surface.restart(self.env)?;
        State::update(self.env.paths, |s| {
            s.last_auto_switch.insert(kind, now);
            if s.pending(kind).is_some_and(|q| q.id == event_id) {
                s.set_pending(kind, None);
            }
            // Only the app's switch is undoable: undoing the CLI alone would not change the shared files.
            if moves_cli {
                s.set_active(SurfaceKind::CodexCli, Some(target.id.clone()));
            }
        })?;
        let from = p.from.clone().unwrap_or_default();
        self.emit(Event::new(now, EventKind::Switched).surface(kind).from(&from).to(&target.id).detail("relaunched after a click"));
        if kind == SurfaceKind::ChatgptDesktop && !State::load(self.env.paths)?.codex_split() && config.surfaces.codex_cli.account_id().is_some() {
            self.emit(Event::new(now, EventKind::Rejoined).surface(SurfaceKind::CodexCli).to(&target.id).detail("the Codex CLI shares ~/.codex with the app again"));
        }
        Ok(Relaunch::Done { from, to: target.id.clone() })
    }
}

/// The event detail of a move back up the priority list.
fn return_detail(to: &str) -> String {
    format!("returning to higher-priority account; {to} has room again")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CodexCliMode, Provider};
    use crate::state::UsageState;
    use crate::token_refresh::RefreshError;
    use crate::usage::testkit::{windows, Fixture, T0};
    use crate::usage::FetchError;
    use crate::surface::{Identity, Staged, Surface};
    use std::sync::{Arc, Mutex};

    /// Records every apply and restart. `fail` makes `apply` fail.
    struct Rec {
        kind: SurfaceKind,
        calls: Arc<Mutex<Vec<String>>>,
        paths: Paths,
        fail: bool,
        running: Option<bool>,
        who: Option<String>,
        /// Saved once, on the first successful apply: a change made from the terminal during the pass.
        rewrite: Arc<Mutex<Option<Config>>>,
    }

    impl Surface for Rec {
        fn kind(&self) -> SurfaceKind {
            self.kind
        }
        fn current_identity(&self, _: &Env) -> Result<Option<Identity>> {
            Ok(self.who.clone().map(|email| Identity { email: Some(email), ..Identity::default() }))
        }
        fn stage(&self, _: &Env, a: &Account) -> Result<Staged> {
            Ok(Staged::new(a.id.clone(), vec![]))
        }
        fn apply(&self, _: &Env, s: Staged) -> Result<()> {
            if self.fail {
                self.calls.lock().unwrap().push(format!("attempt:{}:{}", self.kind.name(), s.account_id));
                return Err(Error::Invalid("apply failed".into()));
            }
            let split = State::load(&self.paths).unwrap().codex_split();
            self.calls.lock().unwrap().push(format!("apply:{}:{}{}", self.kind.name(), s.account_id, if split { ":split" } else { "" }));
            if let Some(config) = self.rewrite.lock().unwrap().take() {
                config.save(&self.paths).unwrap();
            }
            Ok(())
        }
        fn is_running(&self, _: &Env) -> Result<Option<bool>> {
            Ok(self.running)
        }
        fn restart(&self, _: &Env) -> Result<()> {
            self.calls.lock().unwrap().push(format!("restart:{}", self.kind.name()));
            Ok(())
        }
    }

    struct Rig {
        f: Fixture,
        calls: Arc<Mutex<Vec<String>>>,
        fail: Arc<Mutex<bool>>,
        running: Arc<Mutex<Option<bool>>>,
        who: Arc<Mutex<Option<String>>>,
        rewrite: Arc<Mutex<Option<Config>>>,
    }

    impl Rig {
        fn new(accounts: &[(&str, Provider, i64)]) -> Rig {
            Rig { f: Fixture::new(accounts), calls: Arc::default(), fail: Arc::default(), running: Arc::default(), who: Arc::default(), rewrite: Arc::default() }
        }
        fn run<T>(&self, body: impl FnOnce(&mut Watcher) -> T) -> T {
            let env = self.f.env();
            let factory = |k: SurfaceKind| -> Box<dyn Surface> {
                Box::new(Rec {
                    kind: k,
                    calls: self.calls.clone(),
                    paths: self.f.paths.clone(),
                    fail: *self.fail.lock().unwrap(),
                    running: *self.running.lock().unwrap(),
                    who: self.who.lock().unwrap().clone(),
                    rewrite: self.rewrite.clone(),
                })
            };
            let mut w = Watcher::new(&env, &self.f.api, &factory);
            body(&mut w)
        }
        fn applies(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().filter(|c| c.starts_with("apply:")).cloned().collect()
        }
        fn events(&self) -> Vec<Event> {
            let text = std::fs::read_to_string(self.f.paths.events_file()).unwrap_or_default();
            text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
        }
        fn kinds(&self) -> Vec<EventKind> {
            self.events().into_iter().map(|e| e.kind).collect()
        }
        fn auto(&mut self, f: impl FnOnce(&mut Config)) {
            f(&mut self.f.config);
            self.f.config.save(&self.f.paths).unwrap();
        }
    }

    const OK: i64 = 28_800;

    fn claude3() -> Rig {
        let mut r = Rig::new(&[("a", Provider::Claude, OK), ("b", Provider::Claude, OK), ("c", Provider::Claude, OK)]);
        r.f.bind(SurfaceKind::ClaudeCli, "a");
        r.f.set_active(SurfaceKind::ClaudeCli, "a");
        r.auto(|c| c.auto.enabled = true);
        r
    }

    fn ok(five: f64, week: f64) -> std::result::Result<crate::state::Windows, FetchError> {
        Ok(windows(five, week))
    }

    #[test]
    fn switches_the_cli_once_at_91_and_does_not_flip_back_at_88() {
        let r = claude3();
        r.f.api.script_usage("a", vec![ok(91.0, 10.0), ok(88.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(10.0, 10.0), ok(10.0, 10.0), ok(91.0, 10.0)]);
        r.f.api.script_usage("c", vec![ok(10.0, 10.0)]);
        r.run(|w| {
            w.tick(T0).unwrap();
            assert_eq!(r.applies(), ["apply:claude-cli:b"]);
            // a drops to 88 and b stays low: nothing moves, and a is not a way back anyway.
            w.tick(T0 + 400).unwrap();
        });
        assert_eq!(r.applies().len(), 1, "one switch, no flip-back");
        let st = State::load(&r.f.paths).unwrap();
        assert_eq!(st.active(SurfaceKind::ClaudeCli), Some("b"));
        assert_eq!(st.last_auto_switch.get(&SurfaceKind::ClaudeCli), Some(&T0));
        assert_eq!(r.kinds(), [EventKind::Switched]);
    }

    #[test]
    fn with_auto_off_it_reads_usage_but_switches_nothing() {
        let mut r = claude3();
        r.auto(|c| c.auto.enabled = false);
        r.f.api.script_usage("a", vec![ok(99.0, 99.0)]);
        r.f.api.script_usage("b", vec![ok(1.0, 1.0)]);
        r.f.api.script_usage("c", vec![ok(1.0, 1.0)]);
        r.run(|w| w.tick(T0).unwrap());
        assert!(r.applies().is_empty());
        assert_eq!(State::load(&r.f.paths).unwrap().usage["a"].windows.worst_at(T0), Some(99.0));
    }

    #[test]
    fn the_cli_returns_to_a_higher_priority_account_after_the_cooldown() {
        let r = claude3();
        r.f.set_active(SurfaceKind::ClaudeCli, "b");
        State::update(&r.f.paths, |s| {
            s.last_auto_switch.insert(SurfaceKind::ClaudeCli, T0 - 100);
        })
        .unwrap();
        r.f.api.script_usage("a", vec![ok(10.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(40.0, 10.0)]);
        r.f.api.script_usage("c", vec![ok(10.0, 10.0)]);
        r.run(|w| {
            w.tick(T0).unwrap();
            assert!(r.applies().is_empty(), "still in the cooldown");
            w.tick(T0 + 200).unwrap();
        });
        assert_eq!(r.applies(), ["apply:claude-cli:a"]);
        let e = r.events().pop().unwrap();
        assert_eq!((e.kind, e.from.as_deref(), e.to.as_deref()), (EventKind::Switched, Some("b"), Some("a")));
        assert_eq!(e.detail.as_deref(), Some("returning to higher-priority account; a has room again"));
        assert!(e.back);
        assert!(!r.events().iter().any(|e| e.kind != EventKind::Switched || !e.back), "only the return is logged");
    }

    #[test]
    fn a_desktop_surface_gets_an_event_to_return_to_a_higher_priority_account() {
        let mut r = Rig::new(&[("a", Provider::Claude, OK), ("b", Provider::Claude, OK)]);
        r.auto(|c| c.auto.enabled = true);
        r.f.bind(SurfaceKind::ClaudeDesktop, "a");
        r.f.set_active(SurfaceKind::ClaudeDesktop, "b");
        r.f.api.script_usage("a", vec![ok(10.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(40.0, 10.0)]);
        r.run(|w| w.tick(T0).unwrap());
        assert!(r.applies().is_empty() && r.calls.lock().unwrap().is_empty(), "nothing quits without a click");
        let e = r.events().pop().unwrap();
        assert_eq!((e.kind, e.from.as_deref(), e.to.as_deref()), (EventKind::SwitchNeeded, Some("b"), Some("a")));
        assert!(e.back);
        assert!(e.detail.unwrap().starts_with("returning to higher-priority account; a has room again"));
    }

    #[test]
    fn auto_off_for_one_surface_leaves_it_alone_while_another_still_acts() {
        let mut r = claude3();
        r.f.bind(SurfaceKind::ClaudeDesktop, "b");
        r.f.set_active(SurfaceKind::ClaudeDesktop, "b");
        r.auto(|c| c.auto.set_enabled_for(SurfaceKind::ClaudeCli, false));
        r.f.api.script_usage("a", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("c", vec![ok(10.0, 10.0)]);
        r.run(|w| w.tick(T0).unwrap());
        assert!(r.applies().is_empty(), "the Claude CLI stays on a");
        assert_eq!(r.kinds(), [EventKind::SwitchNeeded]);
        assert_eq!(r.events()[0].surface.as_deref(), Some("claude-desktop"));
    }

    #[test]
    fn a_candidate_that_turns_out_full_or_dead_is_skipped() {
        // b reads 10 at first and 95 on the re-check; c's login is near expiry and its refresh is refused.
        let r = Rig::new(&[("a", Provider::Claude, OK), ("b", Provider::Claude, OK), ("c", Provider::Claude, 100), ("d", Provider::Claude, OK)]);
        let mut r = r;
        r.f.bind(SurfaceKind::ClaudeCli, "a");
        r.f.set_active(SurfaceKind::ClaudeCli, "a");
        r.auto(|c| c.auto.enabled = true);
        r.f.api.script_usage("a", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(10.0, 10.0), ok(95.0, 10.0)]);
        r.f.api.script_usage("c", vec![ok(10.0, 10.0)]);
        r.f.api.script_usage("d", vec![ok(10.0, 10.0)]);
        r.f.api.script_refresh("c", Err(RefreshError::Dead("rejected".into())));
        r.run(|w| w.tick(T0).unwrap());
        assert_eq!(r.applies(), ["apply:claude-cli:d"]);
        assert!(r.kinds().contains(&EventKind::AccountDead));
        assert_eq!(State::load(&r.f.paths).unwrap().usage["c"].state, UsageState::Dead);
    }

    #[test]
    fn a_candidate_near_expiry_is_refreshed_just_before_activation() {
        let mut r = Rig::new(&[("a", Provider::Claude, OK), ("b", Provider::Claude, 100)]);
        r.f.bind(SurfaceKind::ClaudeCli, "a");
        r.f.set_active(SurfaceKind::ClaudeCli, "a");
        r.auto(|c| c.auto.enabled = true);
        r.f.api.script_usage("a", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(10.0, 10.0)]);
        r.run(|w| w.tick(T0).unwrap());
        assert_eq!(r.applies(), ["apply:claude-cli:b"]);
        assert_eq!(r.f.api.count("refresh:b"), 1);
        assert_eq!(r.f.api.count("refresh:a"), 0, "the active account is never refreshed");
    }

    #[test]
    fn a_failed_activation_is_logged_and_the_next_candidate_is_tried() {
        let r = claude3();
        for id in ["a", "b", "c"] {
            r.f.api.script_usage(id, vec![ok(if id == "a" { 95.0 } else { 10.0 }, 10.0)]);
        }
        *r.fail.lock().unwrap() = true;
        r.run(|w| w.tick(T0).unwrap());
        assert!(r.applies().is_empty());
        let kinds = r.kinds();
        assert_eq!(kinds.iter().filter(|k| **k == EventKind::SwitchFailed).count(), 2, "both candidates tried");
        assert_eq!(State::load(&r.f.paths).unwrap().active(SurfaceKind::ClaudeCli), Some("a"));
    }

    fn attempts(r: &Rig) -> usize {
        r.calls.lock().unwrap().iter().filter(|c| c.starts_with("attempt:")).count()
    }

    #[test]
    fn a_failing_switch_is_not_retried_every_tick() {
        let r = claude3();
        r.f.api.script_usage("a", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(10.0, 10.0)]);
        r.f.api.script_usage("c", vec![ok(10.0, 10.0)]);
        *r.fail.lock().unwrap() = true;
        r.run(|w| {
            w.tick(T0).unwrap();
            assert_eq!(attempts(&r), 2, "both candidates tried once");
            let calls_after_first = r.f.api.calls().len();
            // Inside the first back-off nothing is staged, refreshed, polled or applied again.
            w.tick(T0 + 10).unwrap();
            w.tick(T0 + 50).unwrap();
            assert_eq!(attempts(&r), 2);
            assert_eq!(r.f.api.calls().len(), calls_after_first, "no network calls while backing off");
            // After a minute it tries once more, then waits five minutes.
            w.tick(T0 + 61).unwrap();
            assert_eq!(attempts(&r), 4);
            w.tick(T0 + 61 + 200).unwrap();
            assert_eq!(attempts(&r), 4);
            w.tick(T0 + 61 + 301).unwrap();
            assert_eq!(attempts(&r), 6);
        });
        let failed = r.kinds().iter().filter(|k| **k == EventKind::SwitchFailed).count();
        assert_eq!(failed, 2, "the same error is announced once per target, not on every retry");
    }

    #[test]
    fn a_new_live_login_or_a_success_clears_the_back_off() {
        let r = claude3();
        r.f.api.script_usage("a", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(10.0, 10.0)]);
        r.f.api.script_usage("c", vec![ok(10.0, 10.0)]);
        *r.fail.lock().unwrap() = true;
        r.run(|w| {
            w.tick(T0).unwrap();
            assert_eq!(attempts(&r), 2);
            // A different live login (not the one the failure was remembered for) is tried at once.
            *r.who.lock().unwrap() = Some("someone-else@example.com".into());
            w.tick(T0 + 5).unwrap();
            assert_eq!(attempts(&r), 4, "the changed login is tried immediately");
        });
    }

    #[test]
    fn no_candidate_is_reported_once_per_half_hour() {
        let r = claude3();
        r.f.api.script_usage("a", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(99.0, 10.0)]);
        r.f.api.script_usage("c", vec![ok(99.0, 10.0)]);
        r.run(|w| {
            w.tick(T0).unwrap();
            w.tick(T0 + 200).unwrap();
            w.tick(T0 + 400).unwrap();
        });
        assert_eq!(r.kinds().iter().filter(|k| **k == EventKind::NoCandidate).count(), 1);
    }

    fn claude_desktop_rig() -> Rig {
        let mut r = Rig::new(&[("a", Provider::Claude, OK), ("b", Provider::Claude, OK), ("c", Provider::Claude, OK)]);
        r.auto(|c| c.auto.enabled = true);
        r.f.bind(SurfaceKind::ClaudeDesktop, "a");
        r.f.set_active(SurfaceKind::ClaudeDesktop, "a");
        r.f.api.script_usage("a", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(10.0, 10.0)]);
        r.f.api.script_usage("c", vec![ok(10.0, 10.0)]);
        r
    }

    #[test]
    fn a_desktop_surface_only_gets_an_event_with_an_id_and_a_15_minute_lifetime() {
        let r = claude_desktop_rig();
        r.run(|w| {
            w.tick(T0).unwrap();
            w.tick(T0 + 300).unwrap();
        });
        assert!(!r.applies().iter().any(|c| c.contains("claude-desktop")), "{:?}", r.applies());
        assert!(!r.calls.lock().unwrap().iter().any(|c| c.starts_with("restart")));
        let events: Vec<Event> = r.events().into_iter().filter(|e| e.kind == EventKind::SwitchNeeded).collect();
        assert_eq!(events.len(), 1, "an event that is still valid is not raised again");
        let e = &events[0];
        assert_eq!((e.surface.as_deref(), e.from.as_deref(), e.to.as_deref(), e.expires_at), (Some("claude-desktop"), Some("a"), Some("b"), Some(T0 + 900)));
        let p = State::load(&r.f.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).cloned().unwrap();
        assert_eq!(Some(&p.id), e.id.as_ref());
    }

    #[test]
    fn a_click_on_the_current_event_switches_and_restarts() {
        let r = claude_desktop_rig();
        r.run(|w| {
            w.tick(T0).unwrap();
            let id = State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).unwrap().id.clone();
            assert_eq!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + 60).unwrap(), Relaunch::Done { from: "a".into(), to: "b".into() });
        });
        assert_eq!(r.applies(), ["apply:claude-desktop:b"]);
        assert!(r.calls.lock().unwrap().contains(&"restart:claude-desktop".to_string()));
        let st = State::load(&r.f.paths).unwrap();
        assert_eq!(st.active(SurfaceKind::ClaudeDesktop), Some("b"));
        assert!(st.pending(SurfaceKind::ClaudeDesktop).is_none());
    }

    #[test]
    fn stale_events_do_nothing() {
        let r = claude_desktop_rig();
        r.run(|w| {
            w.tick(T0).unwrap();
            let id = State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).unwrap().id.clone();
            // Unknown id.
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, "nope", T0 + 10).unwrap(), Relaunch::Stale(_)));
            // Expired.
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + EVENT_TTL_S).unwrap(), Relaunch::Stale(m) if m.contains("expired")));
            assert!(State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).is_none(), "the expired event is dropped");
            // Already answered: clicking the same id again finds nothing.
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + 20).unwrap(), Relaunch::Stale(_)));
        });
        assert!(r.applies().is_empty());
        assert!(r.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn an_event_is_stale_when_the_surface_moved_on_or_the_target_filled_up() {
        let r = claude_desktop_rig();
        r.run(|w| {
            w.tick(T0).unwrap();
            let id = State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).unwrap().id.clone();
            // The user switched the app by hand to c in the meantime.
            State::update(w.env.paths, |s| s.set_active(SurfaceKind::ClaudeDesktop, Some("c".into()))).unwrap();
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + 10).unwrap(), Relaunch::Stale(m) if m.contains("no longer on the account")));
        });
        let r = claude_desktop_rig();
        r.f.api.script_usage("b", vec![ok(10.0, 10.0), ok(97.0, 10.0)]);
        r.run(|w| {
            w.tick(T0).unwrap();
            let id = State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).unwrap().id.clone();
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + 10).unwrap(), Relaunch::Stale(m) if m.contains("close to its limit")));
        });
        assert!(r.applies().is_empty());
    }

    #[test]
    fn a_click_is_stale_when_the_surface_is_unmanaged_or_the_app_is_no_longer_running() {
        // The user unbinds the surface after the event was raised.
        let r = claude_desktop_rig();
        r.run(|w| {
            w.tick(T0).unwrap();
            let id = State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).unwrap().id.clone();
            switch::bind(w.env.paths, SurfaceKind::ClaudeDesktop, crate::config::Binding::Unmanaged).unwrap();
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + 10).unwrap(), Relaunch::Stale(m) if m.contains("no longer managed")));
            assert!(State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).is_none());
        });
        assert!(r.applies().is_empty());

        // The app was quit by hand: relaunching would only start it, so the click does nothing.
        let r = claude_desktop_rig();
        r.run(|w| w.tick(T0).unwrap());
        *r.running.lock().unwrap() = Some(false);
        r.run(|w| {
            let id = State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).unwrap().id.clone();
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + 10).unwrap(), Relaunch::Stale(m) if m.contains("not running")));
        });
        assert!(r.applies().is_empty() && r.calls.lock().unwrap().is_empty());

        // Running: the click goes through.
        let r = claude_desktop_rig();
        *r.running.lock().unwrap() = Some(true);
        r.run(|w| {
            w.tick(T0).unwrap();
            let id = State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).unwrap().id.clone();
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + 10).unwrap(), Relaunch::Done { .. }));
        });
    }

    #[test]
    fn events_reach_the_sink_with_the_reading_that_caused_them() {
        let r = claude3();
        for id in ["a", "b", "c"] {
            r.f.api.script_usage(id, vec![ok(if id == "a" { 91.0 } else { 10.0 }, 10.0)]);
        }
        let seen: Arc<Mutex<Vec<Event>>> = Arc::default();
        let sink = seen.clone();
        r.run(|w| {
            w.on_event(move |e| sink.lock().unwrap().push(e.clone()));
            w.tick(T0).unwrap();
        });
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].back, "leaving an account at its limit is not a return");
        assert_eq!((seen[0].kind, seen[0].from.as_deref(), seen[0].to.as_deref(), seen[0].pct), (EventKind::Switched, Some("a"), Some("b"), Some(91.0)));
    }

    #[test]
    fn a_click_is_stale_when_the_target_is_now_live_on_another_token_surface() {
        let r = claude_desktop_rig();
        r.run(|w| {
            w.tick(T0).unwrap();
            let id = State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).unwrap().id.clone();
            // While the event waited, the Claude CLI moved to its target.
            State::update(w.env.paths, |s| s.set_active(SurfaceKind::ClaudeCli, Some("b".into()))).unwrap();
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + 10).unwrap(), Relaunch::Stale(m) if m.contains("in use on another surface")));
            assert!(State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).is_none(), "the stale event is dropped");
        });
        assert!(r.applies().is_empty() && r.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn an_old_config_file_with_a_surface_bound_below_the_first_account_switches_nothing() {
        let r = Rig::new(&[("a", Provider::Claude, OK), ("b", Provider::Claude, OK)]);
        std::fs::write(
            r.f.paths.config_file(),
            "[[accounts]]\nid = \"a\"\nprovider = \"claude\"\n[[accounts]]\nid = \"b\"\nprovider = \"claude\"\n\
             [surfaces]\nclaude-cli = \"b\"\nclaude-desktop = \"b\"\n[auto]\nenabled = true\n",
        )
        .unwrap();
        r.f.set_active(SurfaceKind::ClaudeCli, "b");
        r.f.set_active(SurfaceKind::ClaudeDesktop, "b");
        r.f.api.script_usage("a", vec![ok(5.0, 5.0)]);
        r.f.api.script_usage("b", vec![ok(40.0, 10.0)]);
        r.run(|w| {
            w.tick(T0).unwrap();
            w.tick(T0 + 400).unwrap();
        });
        assert!(r.applies().is_empty());
        assert!(r.kinds().is_empty(), "no switch and no event: {:?}", r.kinds());
    }

    #[test]
    fn an_unreadable_usage_endpoint_does_not_block_a_click() {
        let r = claude_desktop_rig();
        r.f.api.script_usage("b", vec![ok(10.0, 10.0), Err(FetchError::Failed { kind: "http-503".into(), retry_after_s: None })]);
        r.run(|w| {
            w.tick(T0).unwrap();
            let id = State::load(w.env.paths).unwrap().pending(SurfaceKind::ClaudeDesktop).unwrap().id.clone();
            assert!(matches!(w.relaunch(SurfaceKind::ClaudeDesktop, &id, T0 + 10).unwrap(), Relaunch::Done { .. }));
        });
    }

    fn openai_rig(mode: CodexCliMode, together: bool) -> Rig {
        let mut r = Rig::new(&[("x", Provider::Openai, OK), ("y", Provider::Openai, OK)]);
        r.f.bind(SurfaceKind::CodexCli, "x");
        r.f.bind(SurfaceKind::ChatgptDesktop, if together { "x" } else { "y" });
        r.f.set_active(SurfaceKind::CodexCli, "x");
        r.f.set_active(SurfaceKind::ChatgptDesktop, if together { "x" } else { "y" });
        r.auto(|c| {
            c.auto.enabled = true;
            c.auto.codex_cli_mode = mode;
        });
        r.f.api.script_usage("x", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("y", vec![ok(5.0, 5.0)]);
        r
    }

    #[test]
    fn codex_cli_modes_new_sessions_and_force_switch_now_notify_raises_an_event() {
        for (mode, switches) in [(CodexCliMode::NewSessions, true), (CodexCliMode::Force, true), (CodexCliMode::Notify, false)] {
            // The app is on its own account (y), so the CLI needs no split and moves to z.
            let mut r = Rig::new(&[("x", Provider::Openai, OK), ("y", Provider::Openai, OK), ("z", Provider::Openai, OK)]);
            r.f.bind(SurfaceKind::CodexCli, "x");
            r.f.bind(SurfaceKind::ChatgptDesktop, "y");
            r.f.set_active(SurfaceKind::CodexCli, "x");
            r.f.set_active(SurfaceKind::ChatgptDesktop, "y");
            r.auto(|c| {
                c.auto.enabled = true;
                c.auto.codex_cli_mode = mode;
            });
            r.f.api.script_usage("x", vec![ok(95.0, 10.0)]);
            r.f.api.script_usage("y", vec![ok(5.0, 5.0)]);
            r.f.api.script_usage("z", vec![ok(5.0, 5.0)]);
            r.run(|w| w.tick(T0).unwrap());
            let kinds = r.kinds();
            if switches {
                assert_eq!(r.applies(), ["apply:codex-cli:z"], "{mode:?}");
                assert_eq!(kinds, [EventKind::Switched], "{mode:?}");
                let detail = r.events()[0].detail.clone().unwrap();
                assert_eq!(detail.contains("may need a new sign-in"), mode == CodexCliMode::Force, "{detail}");
            } else {
                assert!(r.applies().is_empty(), "{mode:?}");
                assert_eq!(kinds, [EventKind::SwitchNeeded], "{mode:?}");
                assert!(State::load(&r.f.paths).unwrap().pending(SurfaceKind::CodexCli).is_some());
            }
        }
    }

    #[test]
    fn bound_together_the_cli_splits_first_and_the_click_rejoins() {
        let r = openai_rig(CodexCliMode::NewSessions, true);
        r.run(|w| {
            w.tick(T0).unwrap();
            // The CLI moved now, with the rejoin record already written when apply ran.
            assert_eq!(r.applies(), ["apply:codex-cli:y:split"]);
            let st = State::load(w.env.paths).unwrap();
            assert!(st.codex_split());
            assert_eq!(st.pending(SurfaceKind::CodexCli).unwrap().target_account, "y");
            assert_eq!(st.active(SurfaceKind::CodexCli), Some("y"));
            // The app waits for the click and is aimed at the CLI's account.
            let ev = st.pending(SurfaceKind::ChatgptDesktop).unwrap().clone();
            assert_eq!((ev.reason.as_str(), ev.target_account.as_str()), (REASON_SWITCH_NEEDED, "y"));

            // A restart of the menu bar loses nothing: the state file still says split.
            let reloaded = State::load(w.env.paths).unwrap();
            assert!(reloaded.codex_split());

            assert_eq!(w.relaunch(SurfaceKind::ChatgptDesktop, &ev.id, T0 + 30).unwrap(), Relaunch::Done { from: "x".into(), to: "y".into() });
            let st = State::load(w.env.paths).unwrap();
            assert!(!st.codex_split(), "the app reached the CLI's account");
            assert_eq!(st.active(SurfaceKind::ChatgptDesktop), Some("y"));
        });
        assert!(r.kinds().contains(&EventKind::Rejoined));
    }

    #[test]
    fn a_click_that_moves_a_shared_chatgpt_app_records_the_codex_cli_on_the_new_account_too() {
        // Bound together on y, with x ranked higher for both: the CLI stays (no split for a return) and the
        // app gets the return event. The click rewrites the shared ~/.codex, so the CLI is on x as well.
        let mut r = Rig::new(&[("x", Provider::Openai, OK), ("y", Provider::Openai, OK)]);
        r.f.bind(SurfaceKind::CodexCli, "y");
        r.f.bind(SurfaceKind::ChatgptDesktop, "y");
        r.f.set_active(SurfaceKind::CodexCli, "y");
        r.f.set_active(SurfaceKind::ChatgptDesktop, "y");
        r.auto(|c| {
            c.auto.enabled = true;
            c.priority.codex_cli = vec!["x".into(), "y".into()];
            c.priority.chatgpt_desktop = vec!["x".into(), "y".into()];
        });
        r.f.api.script_usage("x", vec![ok(5.0, 5.0)]);
        r.f.api.script_usage("y", vec![ok(40.0, 10.0)]);
        r.run(|w| {
            w.tick(T0).unwrap();
            assert!(r.applies().is_empty(), "the CLI does not split to return");
            let ev = State::load(w.env.paths).unwrap().pending(SurfaceKind::ChatgptDesktop).unwrap().clone();
            assert_eq!(w.relaunch(SurfaceKind::ChatgptDesktop, &ev.id, T0 + 30).unwrap(), Relaunch::Done { from: "y".into(), to: "x".into() });
        });
        let st = State::load(&r.f.paths).unwrap();
        assert_eq!((st.active(SurfaceKind::ChatgptDesktop), st.active(SurfaceKind::CodexCli)), (Some("x"), Some("x")));
        assert_eq!(st.undo_target(None).unwrap().surface, SurfaceKind::ChatgptDesktop, "Undo moves the app, which takes the CLI along");
        assert!(st.undo_target(Some(SurfaceKind::CodexCli)).is_none());
        assert!(!st.codex_split());
    }

    #[test]
    fn a_config_change_during_a_pass_is_used_for_the_surfaces_decided_after_it() {
        // The Claude CLI switches first; meanwhile Claude Desktop's auto-switch is turned off from outside.
        let mut r = claude3();
        r.f.bind(SurfaceKind::ClaudeDesktop, "b");
        r.f.set_active(SurfaceKind::ClaudeDesktop, "b");
        r.f.api.script_usage("a", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("b", vec![ok(95.0, 10.0)]);
        r.f.api.script_usage("c", vec![ok(10.0, 10.0)]);
        let mut changed = r.f.config.clone();
        changed.auto.set_enabled_for(SurfaceKind::ClaudeDesktop, false);
        *r.rewrite.lock().unwrap() = Some(changed);
        r.run(|w| w.tick(T0).unwrap());
        assert_eq!(r.applies(), ["apply:claude-cli:c"]);
        // With the config read before the switch, Claude Desktop would still be decided and report no candidate.
        assert_eq!(r.kinds(), [EventKind::Switched]);
    }

    #[test]
    fn a_failed_split_leaves_no_rejoin_record() {
        let r = openai_rig(CodexCliMode::NewSessions, true);
        *r.fail.lock().unwrap() = true;
        r.run(|w| w.tick(T0).unwrap());
        assert!(!State::load(&r.f.paths).unwrap().codex_split());
    }

    #[test]
    fn auto_pauses_when_every_reading_is_stale_and_resumes_when_one_returns() {
        let r = claude3();
        let down = || Err(FetchError::Failed { kind: "http-503".into(), retry_after_s: None });
        for id in ["a", "b", "c"] {
            r.f.api.script_usage(id, vec![down(), ok(10.0, 10.0)]);
        }
        r.run(|w| {
            w.tick(T0).unwrap();
            w.tick(T0 + 5).unwrap();
        });
        assert_eq!(r.kinds(), [EventKind::AutoPaused], "announced once");
        assert!(r.applies().is_empty());
        r.run(|w| {
            // The paused state lives in the watcher; a fresh one announces again, so keep going on the same one.
            w.paused = vec![Provider::Claude];
            w.tick(T0 + 2000).unwrap();
        });
        assert!(r.kinds().contains(&EventKind::AutoResumed));
    }

    #[test]
    fn the_event_log_is_private_json_lines_without_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let r = claude3();
        for id in ["a", "b", "c"] {
            r.f.api.script_usage(id, vec![ok(if id == "a" { 95.0 } else { 10.0 }, 10.0)]);
        }
        r.run(|w| w.tick(T0).unwrap());
        let file = r.f.paths.events_file();
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("tok-") && !text.contains("rt-"), "{text}");
        assert!(text.lines().all(|l| serde_json::from_str::<Event>(l).is_ok()));
    }

    #[test]
    fn the_log_rotates_past_its_size_cap() {
        let r = claude3();
        let log = EventLog::new(&r.f.paths);
        let big = Event::new(T0, EventKind::Switched).detail("x".repeat(64 * 1024));
        for _ in 0..20 {
            log.append(&big).unwrap();
        }
        let old = r.f.paths.logs_dir().join("events.jsonl.1");
        assert!(old.is_file() && std::fs::metadata(r.f.paths.events_file()).unwrap().len() < LOG_ROTATE_BYTES + 70_000);
    }

    #[test]
    fn only_one_watcher_can_start() {
        let r = claude3();
        let env = r.f.env();
        let factory = |k: SurfaceKind| -> Box<dyn Surface> { Box::new(Rec { kind: k, calls: Arc::default(), paths: r.f.paths.clone(), fail: false, running: None, who: None, rewrite: Arc::default() }) };
        let first = Watcher::start(&env, &r.f.api, &factory).unwrap();
        assert!(matches!(Watcher::start(&env, &r.f.api, &factory), Err(Error::LockBusy(_))));
        drop(first);
        assert!(Watcher::start(&env, &r.f.api, &factory).is_ok());
    }
}
