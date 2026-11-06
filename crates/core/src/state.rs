//! `~/Library/Application Support/agent-switch/state.json`: metadata only, never tokens.
//!
//! The binding (the user's choice) lives in `config.toml`. This file holds what is true right now: the
//! **active** account per surface, any **pending** record (a switch or rejoin that still needs a click),
//! and a short journal that powers `undo`. A journal entry names accounts by id only. `undo` re-activates
//! the previous account from its *current* vault slot, never from a stored copy of its token, because that
//! copy may have been spent by a refresh in between.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::SurfaceKind;
use crate::error::{Error, Result};
use crate::fsutil::{ensure_private_dir, safe_write, WriteOpts};
use crate::locks::FlockGuard;
use crate::paths::Paths;

const JOURNAL_LIMIT: usize = 100;
/// How many quarantined logins are kept before the oldest is deleted.
pub const QUARANTINE_LIMIT: usize = 3;
const STATE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `Pending::reason` of a Desktop (or Codex notify-mode) event that waits for the user's "Relaunch now" click.
pub const REASON_SWITCH_NEEDED: &str = "switch-needed";
/// `Pending::reason` of a Codex CLI that temporarily split off into the shallow home and rejoins `~/.codex`
/// when the ChatGPT app moves to the same account.
pub const REASON_REJOIN: &str = "rejoin";

/// A switch that still waits for something (for example a Desktop relaunch click, or a Codex rejoin).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub target_account: String,
    pub reason: String,
    /// The account the surface was on when the record was made; a click acts only while that still holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl Pending {
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }
}

/// Whether a usage reading could be taken. Only `Dead` quarantines an account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageState {
    #[default]
    Ok,
    /// The access token expired (or was refused) and no refresh was made this pass.
    Expired,
    /// The refresh token was rejected (`invalid_grant`): the account needs a new login.
    Dead,
    /// The usage endpoint failed (network, 5xx, 429, unexpected body).
    Unreadable,
}

impl std::fmt::Display for UsageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UsageState::Ok => "ok",
            UsageState::Expired => "expired",
            UsageState::Dead => "dead",
            UsageState::Unreadable => "unreadable",
        })
    }
}

/// One usage window as last read.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowSnap {
    /// Percent used.
    pub pct: f64,
    /// Unix seconds when this window resets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

impl WindowSnap {
    /// The percent as it stands at `now`: a window whose reset time has passed has rolled over.
    pub fn pct_at(&self, now: u64) -> f64 {
        match self.resets_at {
            Some(t) if t <= now as i64 => 0.0,
            _ => self.pct,
        }
    }
}

/// Both windows of one reading. A window the account does not have is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Windows {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<WindowSnap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<WindowSnap>,
}

impl Windows {
    /// The larger of the two windows' percents at `now`, or `None` when neither window is known.
    pub fn worst_at(&self, now: u64) -> Option<f64> {
        [self.five_hour, self.weekly].into_iter().flatten().map(|w| w.pct_at(now)).fold(None, |m, p| Some(m.map_or(p, |m: f64| m.max(p))))
    }
}

/// What is known about one account's usage. Kept when a fetch fails, so `list` can show the last values and
/// their age. Holds no token and no email.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageRecord {
    pub state: UsageState,
    #[serde(flatten)]
    pub windows: Windows,
    /// Unix seconds of the last successful reading.
    pub fetched_at: Option<u64>,
    /// Unix seconds of the last attempt.
    pub checked_at: u64,
    /// A short, non-secret reason for a state other than `ok`.
    pub detail: Option<String>,
    /// The next poll is due at this time, and `interval_s` is the spacing that produced it.
    pub next_poll_at: u64,
    pub interval_s: u64,
    /// Last HTTP 429 from the usage endpoint.
    pub last_429_at: Option<u64>,
    /// Consecutive urgent-interval polls; urgent mode is bounded by this.
    pub urgent_streak: u32,
    /// The refresh-token fingerprint that was found dead. A different fingerprint (a new login) clears it.
    pub dead_fp: Option<String>,
}

/// A refresh that was started and not yet committed (see `token_refresh`). Holds a fingerprint only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inflight {
    pub fp: String,
    pub at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchRecord {
    pub surface: SurfaceKind,
    pub previous_account_id: Option<String>,
    pub new_account_id: String,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    pub surfaces: BTreeMap<SurfaceKind, SurfaceState>,
    pub journal: Vec<SwitchRecord>,
    /// Last-known usage per account id.
    pub usage: BTreeMap<String, UsageRecord>,
    /// When the last automatic switch happened on each surface (the cooldown survives a restart).
    pub last_auto_switch: BTreeMap<SurfaceKind, u64>,
    /// Refreshes started and not committed, per account id.
    pub refresh_inflight: BTreeMap<String, Inflight>,
    /// While set and in the future, no token refresh is sent (the token endpoint rejected our client).
    pub refresh_halted_until: Option<u64>,
    /// Recovery copies of live Claude logins that no saved account owned, as vault id -> unix seconds.
    /// At most `QUARANTINE_LIMIT` are kept; `doctor` lists them and `remove` deletes them.
    pub quarantine: BTreeMap<String, u64>,
}

impl Default for State {
    fn default() -> Self {
        State {
            version: 1,
            surfaces: BTreeMap::new(),
            journal: Vec::new(),
            usage: BTreeMap::new(),
            last_auto_switch: BTreeMap::new(),
            refresh_inflight: BTreeMap::new(),
            refresh_halted_until: None,
            quarantine: BTreeMap::new(),
        }
    }
}

impl State {
    pub fn load(paths: &Paths) -> Result<State> {
        let file = paths.state_file();
        match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", file.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(Error::io(format!("read {}", file.display()), e)),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        ensure_private_dir(&paths.support_dir())?;
        let text = serde_json::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        safe_write(&paths.state_file(), text.as_bytes(), &WriteOpts::secret())
    }

    /// Load, change and save under a cross-process lock, so two writers cannot lose each other's update.
    pub fn update<T>(paths: &Paths, change: impl FnOnce(&mut State) -> T) -> Result<T> {
        let _guard = FlockGuard::lock(&paths.state_lock_file(), STATE_LOCK_TIMEOUT)?;
        let mut state = State::load(paths)?;
        let out = change(&mut state);
        state.save(paths)?;
        Ok(out)
    }

    pub fn active(&self, kind: SurfaceKind) -> Option<&str> {
        self.surfaces.get(&kind)?.active.as_deref()
    }

    pub fn pending(&self, kind: SurfaceKind) -> Option<&Pending> {
        self.surfaces.get(&kind)?.pending.as_ref()
    }

    pub fn set_active(&mut self, kind: SurfaceKind, account: Option<String>) {
        self.surfaces.entry(kind).or_default().active = account;
    }

    pub fn set_pending(&mut self, kind: SurfaceKind, pending: Option<Pending>) {
        self.surfaces.entry(kind).or_default().pending = pending;
    }

    /// True while the Codex CLI is temporarily split off into its own home and waits to rejoin `~/.codex`.
    pub fn codex_split(&self) -> bool {
        self.pending(SurfaceKind::CodexCli).is_some_and(|p| p.reason == REASON_REJOIN)
    }

    /// True when `refresh_halted_until` is in the future.
    pub fn refresh_halted(&self, now: u64) -> bool {
        self.refresh_halted_until.is_some_and(|t| t > now)
    }

    pub fn is_active_anywhere(&self, account_id: &str) -> Vec<SurfaceKind> {
        self.surfaces.iter().filter(|(_, s)| s.active.as_deref() == Some(account_id)).map(|(k, _)| *k).collect()
    }

    /// Record a completed switch: the new account becomes active and the change becomes undoable.
    pub fn record_switch(&mut self, kind: SurfaceKind, new_account_id: &str, at: u64) {
        let previous = self.active(kind).map(str::to_string);
        self.set_active(kind, Some(new_account_id.to_string()));
        // The ChatGPT app moving to the account the split Codex CLI already uses is the rejoin.
        if kind == SurfaceKind::ChatgptDesktop {
            let rejoined = self.pending(SurfaceKind::CodexCli).is_some_and(|p| p.reason == REASON_REJOIN && p.target_account == new_account_id);
            if rejoined {
                self.set_pending(SurfaceKind::CodexCli, None);
            }
        }
        // Any switch of a surface answers an event that was waiting for it.
        if self.pending(kind).is_some_and(|p| p.reason == REASON_SWITCH_NEEDED && p.target_account == new_account_id) {
            self.set_pending(kind, None);
        }
        self.journal.push(SwitchRecord { surface: kind, previous_account_id: previous, new_account_id: new_account_id.into(), at });
        if self.journal.len() > JOURNAL_LIMIT {
            let excess = self.journal.len() - JOURNAL_LIMIT;
            self.journal.drain(..excess);
        }
    }

    /// The newest journal entry for `kind`, or the newest overall when `kind` is `None`.
    pub fn undo_target(&self, kind: Option<SurfaceKind>) -> Option<&SwitchRecord> {
        self.journal.iter().rev().find(|r| kind.is_none_or(|k| r.surface == k))
    }

    /// Finish an undo: the previous account is active again and the entry is consumed.
    pub fn complete_undo(&mut self, kind: SurfaceKind) {
        if let Some(idx) = self.journal.iter().rposition(|r| r.surface == kind) {
            let rec = self.journal.remove(idx);
            self.set_active(kind, rec.previous_account_id);
        }
    }

    /// Drop every reference to a removed account.
    pub fn forget_account(&mut self, account_id: &str) {
        self.journal.retain(|r| r.new_account_id != account_id && r.previous_account_id.as_deref() != Some(account_id));
        for s in self.surfaces.values_mut() {
            if s.pending.as_ref().is_some_and(|p| p.target_account == account_id) {
                s.pending = None;
            }
        }
        self.usage.remove(account_id);
        self.refresh_inflight.remove(account_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default_and_corrupt_file_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        assert_eq!(State::load(&paths).unwrap(), State::default());
        ensure_private_dir(&paths.support_dir()).unwrap();
        std::fs::write(paths.state_file(), "{not json").unwrap();
        assert!(State::load(&paths).is_err());
    }

    #[test]
    fn record_switch_tracks_previous_by_id() {
        let mut s = State::default();
        s.record_switch(SurfaceKind::ClaudeCli, "personal", 1);
        s.record_switch(SurfaceKind::ClaudeCli, "team", 2);
        assert_eq!(s.active(SurfaceKind::ClaudeCli), Some("team"));
        let r = s.undo_target(Some(SurfaceKind::ClaudeCli)).unwrap();
        assert_eq!(r.previous_account_id.as_deref(), Some("personal"));
        assert_eq!(r.new_account_id, "team");
    }

    #[test]
    fn journal_holds_ids_only_no_token_material() {
        let mut s = State::default();
        s.record_switch(SurfaceKind::CodexCli, "work", 5);
        let json = serde_json::to_string(&s).unwrap();
        let keys: Vec<_> = serde_json::from_str::<serde_json::Value>(&json).unwrap()["journal"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys.len(), 4);
        for k in keys {
            assert!(["surface", "previous_account_id", "new_account_id", "at"].contains(&k.as_str()), "{k}");
        }
    }

    #[test]
    fn complete_undo_restores_previous_and_consumes_the_entry() {
        let mut s = State::default();
        s.record_switch(SurfaceKind::ClaudeCli, "personal", 1);
        s.record_switch(SurfaceKind::ClaudeCli, "team", 2);
        s.record_switch(SurfaceKind::CodexCli, "gpt", 3);
        s.complete_undo(SurfaceKind::ClaudeCli);
        assert_eq!(s.active(SurfaceKind::ClaudeCli), Some("personal"));
        assert_eq!(s.active(SurfaceKind::CodexCli), Some("gpt"));
        // The next undo for claude-cli targets the first switch; the codex entry is untouched.
        assert_eq!(s.undo_target(Some(SurfaceKind::ClaudeCli)).unwrap().new_account_id, "personal");
        assert_eq!(s.undo_target(None).unwrap().surface, SurfaceKind::CodexCli);
    }

    #[test]
    fn first_ever_switch_has_no_previous() {
        let mut s = State::default();
        s.record_switch(SurfaceKind::ClaudeCli, "personal", 1);
        assert_eq!(s.undo_target(None).unwrap().previous_account_id, None);
        s.complete_undo(SurfaceKind::ClaudeCli);
        assert_eq!(s.active(SurfaceKind::ClaudeCli), None);
    }

    #[test]
    fn journal_is_capped() {
        let mut s = State::default();
        for i in 0..(JOURNAL_LIMIT + 20) {
            s.record_switch(SurfaceKind::ClaudeCli, if i % 2 == 0 { "a" } else { "b" }, i as u64);
        }
        assert_eq!(s.journal.len(), JOURNAL_LIMIT);
    }

    #[test]
    fn binding_and_active_are_independent() {
        // `State` has no binding field at all: auto-switch changes active/pending, never the binding.
        let mut s = State::default();
        s.set_pending(
            SurfaceKind::ChatgptDesktop,
            Some(Pending { id: "e1".into(), target_account: "team".into(), reason: "usage".into(), from: None, created_at: 1, expires_at: Some(901) }),
        );
        assert_eq!(s.pending(SurfaceKind::ChatgptDesktop).unwrap().target_account, "team");
        assert_eq!(s.active(SurfaceKind::ChatgptDesktop), None);
    }

    #[test]
    fn forget_account_prunes_journal_and_pending() {
        let mut s = State::default();
        s.record_switch(SurfaceKind::ClaudeCli, "a", 1);
        s.record_switch(SurfaceKind::ClaudeCli, "b", 2);
        s.set_pending(SurfaceKind::ClaudeCli, Some(Pending { id: "p".into(), target_account: "a".into(), reason: "r".into(), from: None, created_at: 1, expires_at: None }));
        s.forget_account("a");
        assert!(s.journal.is_empty());
        assert!(s.pending(SurfaceKind::ClaudeCli).is_none());
    }

    #[test]
    fn update_persists_under_lock() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        State::update(&paths, |s| s.record_switch(SurfaceKind::ClaudeCli, "a", 1)).unwrap();
        State::update(&paths, |s| s.record_switch(SurfaceKind::ClaudeCli, "b", 2)).unwrap();
        let loaded = State::load(&paths).unwrap();
        assert_eq!(loaded.active(SurfaceKind::ClaudeCli), Some("b"));
        assert_eq!(loaded.journal.len(), 2);
    }

    #[test]
    fn concurrent_updates_do_not_lose_writes() {
        let d = tempfile::tempdir().unwrap();
        let paths = std::sync::Arc::new(Paths::from_home(d.path()));
        let hs: Vec<_> = (0..6)
            .map(|i| {
                let p = paths.clone();
                std::thread::spawn(move || State::update(&p, |s| s.record_switch(SurfaceKind::ClaudeCli, &format!("a{i}"), i)).unwrap())
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(State::load(&paths).unwrap().journal.len(), 6);
    }

    fn pending(reason: &str, target: &str, expires_at: Option<u64>) -> Pending {
        Pending { id: "e".into(), target_account: target.into(), reason: reason.into(), from: None, created_at: 1, expires_at }
    }

    #[test]
    fn the_app_moving_to_the_split_accounts_target_is_the_rejoin() {
        let mut s = State::default();
        s.set_pending(SurfaceKind::CodexCli, Some(pending(REASON_REJOIN, "b", None)));
        s.set_pending(SurfaceKind::ChatgptDesktop, Some(pending(REASON_SWITCH_NEEDED, "b", Some(900))));
        assert!(s.codex_split());
        s.record_switch(SurfaceKind::ChatgptDesktop, "c", 5); // some other account: still split
        assert!(s.codex_split() && s.pending(SurfaceKind::ChatgptDesktop).is_some());
        s.record_switch(SurfaceKind::ChatgptDesktop, "b", 6);
        assert!(!s.codex_split());
        assert!(s.pending(SurfaceKind::ChatgptDesktop).is_none(), "the answered event is gone");
    }

    #[test]
    fn pending_expiry_and_window_rollover() {
        assert!(pending("x", "a", Some(10)).is_expired(10));
        assert!(!pending("x", "a", Some(10)).is_expired(9));
        assert!(!pending("x", "a", None).is_expired(u64::MAX));
        let w = Windows { five_hour: Some(WindowSnap { pct: 95.0, resets_at: Some(100) }), weekly: Some(WindowSnap { pct: 40.0, resets_at: None }) };
        assert_eq!(w.worst_at(50), Some(95.0));
        assert_eq!(w.worst_at(100), Some(40.0), "a window past its reset has rolled over");
        assert_eq!(Windows::default().worst_at(0), None);
    }

    #[test]
    fn usage_records_round_trip_and_hold_no_secrets() {
        let mut s = State::default();
        s.usage.insert(
            "a".into(),
            UsageRecord {
                state: UsageState::Expired,
                windows: Windows { five_hour: Some(WindowSnap { pct: 13.0, resets_at: Some(1_800_000_000) }), weekly: None },
                fetched_at: Some(7),
                checked_at: 9,
                detail: Some("access token expired".into()),
                ..UsageRecord::default()
            },
        );
        let json = serde_json::to_string(&s).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert!(json.contains(r#""state":"expired""#) && json.contains(r#""five_hour":{"pct":13.0"#), "{json}");
    }

    #[test]
    fn forget_account_drops_its_usage_and_inflight_record() {
        let mut s = State::default();
        s.usage.insert("a".into(), UsageRecord::default());
        s.refresh_inflight.insert("a".into(), Inflight { fp: "f".into(), at: 1 });
        s.forget_account("a");
        assert!(s.usage.is_empty() && s.refresh_inflight.is_empty());
    }
}
