//! Refreshing a saved login's access token.
//!
//! The refresh contract per provider (endpoint, client id, body, error classes) is written out in the
//! phase 5 plan with its sources; the provider modules implement it (`providers::anthropic::refresh`,
//! `providers::openai::refresh`). This module owns the rules around it, ported from claude-swap's consume
//! gate (`oauth.py:147-158`, `switcher.py:2378-2395`):
//!
//! * A refresh runs under the account lock and re-reads the vault slot first, so two callers send one request.
//! * An in-flight record (account id and refresh-token fingerprint) is written to `state.json` **before** the
//!   POST, because the server rotates the refresh token: if the process dies after the POST, the old token
//!   may be spent and the new one is lost. The record is cleared only after the rotated login is committed to
//!   the vault. A record left behind is reported in the next failure message.
//! * `invalid_grant` means the login is dead: the account is quarantined. `invalid_client` means our client
//!   registration was rejected, which says nothing about any account: all refreshes stop for a while and
//!   nothing is quarantined (`oauth.py:232-238`). Network, 5xx and 429 errors are transient.
//! * An account that is active on a surface that holds its token chain is never refreshed here: the CLI or
//!   the app owns that refresh.

use std::fmt;
use std::time::Duration;

use crate::config::Account;
use crate::error::Error;
use crate::locks::AccountLock;
use crate::state::{Inflight, State, UsageState};
use crate::surface::Env;
use crate::usage::{active_chains, ProviderApi};
use crate::vault::Slot;

/// Refresh just before a switch when the access token expires within this many seconds (twice Claude Code's
/// own buffer, so a running Claude Code that re-reads under its lock sees a fresh token and skips its refresh;
/// claude-swap `autoswitch.py:24-28`).
pub const FRESHEN_WITHIN_S: u64 = 600;

/// How long every refresh stays stopped after `invalid_client`.
pub const HALT_S: u64 = 6 * 3600;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const COMMIT_ATTEMPTS: u32 = 3;

/// What the token endpoint said, reduced to the three classes the policy acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshError {
    /// `invalid_grant` and its relatives: this login needs a new sign-in.
    Dead(String),
    /// `invalid_client`: our client was rejected. Stop all refreshes; never quarantine for it.
    Systemic(String),
    /// Try again later. `may_have_consumed` is true when the server could have rotated the token before the
    /// failure (a timeout or a 5xx), so the old token might be spent.
    Transient { detail: String, may_have_consumed: bool },
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefreshError::Dead(m) | RefreshError::Systemic(m) => f.write_str(m),
            RefreshError::Transient { detail, .. } => f.write_str(detail),
        }
    }
}

/// Success results of `freshen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshened {
    /// The saved login was valid for long enough already; nothing was sent.
    Fresh,
    /// A refresh request rotated the login and the vault holds the new one.
    Refreshed,
    /// The account is active on a surface that owns its refresh; nothing was sent.
    Skipped,
}

#[derive(Debug)]
pub enum FreshenError {
    /// The refresh token is dead; the account is quarantined.
    Dead(String),
    /// Refreshes are stopped (`invalid_client`).
    Halted(String),
    /// Try again later.
    Transient(String),
    Other(Error),
}

impl fmt::Display for FreshenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FreshenError::Dead(m) | FreshenError::Halted(m) | FreshenError::Transient(m) => f.write_str(m),
            FreshenError::Other(e) => e.fmt(f),
        }
    }
}

impl From<Error> for FreshenError {
    fn from(e: Error) -> Self {
        FreshenError::Other(e)
    }
}

pub struct Ctx<'a> {
    pub env: &'a Env<'a>,
    pub api: &'a dyn ProviderApi,
}

/// Quarantine `id` in the usage record so that no policy picks it and no refresh is retried for this login.
fn mark_dead(state: &mut State, id: &str, fp: &str, detail: &str, now: u64) {
    let rec = state.usage.entry(id.to_string()).or_default();
    rec.state = UsageState::Dead;
    rec.dead_fp = Some(fp.to_string());
    rec.detail = Some(detail.to_string());
    rec.checked_at = now;
}

/// Make sure the saved login of `account` is valid for at least `within_s` more seconds, refreshing it when
/// it is not. Used just before a switch and, with the user's opt-in, in the background.
pub fn freshen(ctx: &Ctx, account: &Account, now: u64, within_s: u64) -> Result<Freshened, FreshenError> {
    let paths = ctx.env.paths;
    let id = account.id.as_str();
    let state = State::load(paths)?;
    if state.refresh_halted(now) {
        return Err(FreshenError::Halted(
            "token refreshes are paused because the token endpoint rejected agent-switch's client (invalid_client); nothing was quarantined".into(),
        ));
    }
    if !active_chains(&state, id).is_empty() {
        return Ok(Freshened::Skipped);
    }

    let _lock = AccountLock::acquire(paths, id, LOCK_TIMEOUT)?;
    // Re-read under the lock: another process may have refreshed it, or made the account live, while we waited.
    let state = State::load(paths)?;
    if !active_chains(&state, id).is_empty() {
        return Ok(Freshened::Skipped);
    }
    let slot = ctx.env.vault.get(id, account.provider.store())?.ok_or_else(|| Error::NotFound(format!("saved login for `{id}`")))?;
    let cred = ctx.api.credential(account, &slot.blob)?;
    if let Some(rec) = state.usage.get(id).filter(|r| r.state == UsageState::Dead && r.dead_fp.as_deref() == Some(cred.fingerprint.as_str())) {
        return Err(FreshenError::Dead(rec.detail.clone().unwrap_or_else(|| "the login is dead; sign in again".into())));
    }
    if cred.expires_at.is_none_or(|t| t > now + within_s) {
        return Ok(Freshened::Fresh);
    }

    let interrupted = state.refresh_inflight.contains_key(id);
    State::update(paths, |s| {
        s.refresh_inflight.insert(id.to_string(), Inflight { fp: cred.fingerprint.clone(), at: now });
    })?;

    match ctx.api.refresh(account, &slot.blob, now) {
        Ok(new_blob) => {
            let committed = commit(ctx, account, new_blob, now);
            match committed {
                Ok(()) => {
                    State::update(paths, |s| {
                        s.refresh_inflight.remove(id);
                    })?;
                    Ok(Freshened::Refreshed)
                }
                Err(e) => {
                    // The server rotated the token and we could not keep the result: the old token is spent.
                    let why = "the token was refreshed but the new login could not be saved; sign in to this account again";
                    State::update(paths, |s| {
                        s.refresh_inflight.remove(id);
                        mark_dead(s, id, &cred.fingerprint, why, now);
                    })?;
                    Err(FreshenError::Other(Error::Keychain(format!("{why} ({e})"))))
                }
            }
        }
        Err(RefreshError::Dead(m)) => {
            let detail = if interrupted { format!("{m}; an earlier refresh did not finish, so its new token may have been lost") } else { m };
            State::update(paths, |s| {
                s.refresh_inflight.remove(id);
                mark_dead(s, id, &cred.fingerprint, &detail, now);
            })?;
            Err(FreshenError::Dead(detail))
        }
        Err(RefreshError::Systemic(m)) => {
            State::update(paths, |s| {
                s.refresh_inflight.remove(id);
                s.refresh_halted_until = Some(now + HALT_S);
            })?;
            Err(FreshenError::Halted(m))
        }
        Err(RefreshError::Transient { detail, may_have_consumed }) => {
            if !may_have_consumed {
                State::update(paths, |s| {
                    s.refresh_inflight.remove(id);
                })?;
            }
            Err(FreshenError::Transient(detail))
        }
    }
}

/// Write the rotated login to the vault right after the POST. Three attempts, because a lost write loses the
/// only copy of the new refresh token.
fn commit(ctx: &Ctx, account: &Account, new_blob: String, now: u64) -> Result<(), Error> {
    let cred = ctx.api.credential(account, &new_blob)?;
    let slot = Slot { blob: new_blob, fingerprint: cred.fingerprint, refreshed_at: now };
    let mut last = None;
    for attempt in 0..COMMIT_ATTEMPTS {
        match ctx.env.vault.put(&account.id, account.provider.store(), &slot) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        if attempt + 1 < COMMIT_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Err(last.unwrap_or_else(|| Error::Keychain("vault write failed".into())))
}

/// A refresh that started and never committed, for `doctor` and `list`.
pub fn interrupted(state: &State, id: &str) -> Option<Inflight> {
    state.refresh_inflight.get(id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Provider, SurfaceKind};
    use crate::usage::testkit::*;

    fn run(f: &Fixture, id: &str, now: u64) -> Result<Freshened, FreshenError> {
        let env = f.env();
        freshen(&Ctx { env: &env, api: &f.api }, f.config.account(id).unwrap(), now, FRESHEN_WITHIN_S)
    }

    #[test]
    fn a_login_valid_for_more_than_ten_minutes_is_left_alone() {
        let f = Fixture::new(&[("a", Provider::Claude, 601)]);
        assert_eq!(run(&f, "a", T0).unwrap(), Freshened::Fresh);
        assert_eq!(f.api.count("refresh:"), 0);
    }

    #[test]
    fn a_login_expiring_within_ten_minutes_is_refreshed_and_saved_back() {
        let f = Fixture::new(&[("a", Provider::Claude, 599)]);
        assert_eq!(run(&f, "a", T0).unwrap(), Freshened::Refreshed);
        let slot = f.vault.get("a", "claude-oauth").unwrap().unwrap();
        assert!(slot.blob.contains("rt-a-next"));
        assert_eq!(slot.fingerprint, crate::vault::lineage_fingerprint("rt-a-next"));
        assert!(State::load(&f.paths).unwrap().refresh_inflight.is_empty(), "the journal entry is cleared after the commit");
        // Fresh now: the next call sends nothing.
        assert_eq!(run(&f, "a", T0 + 1).unwrap(), Freshened::Fresh);
        assert_eq!(f.api.count("refresh:"), 1);
    }

    #[test]
    fn the_journal_entry_exists_before_the_request_and_holds_no_token() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Peek<'a> {
            inner: &'a FakeApi,
            paths: &'a crate::paths::Paths,
            seen: AtomicBool,
        }
        impl ProviderApi for Peek<'_> {
            fn credential(&self, a: &Account, b: &str) -> crate::Result<crate::usage::Credential> {
                self.inner.credential(a, b)
            }
            fn fetch_usage(&self, a: &Account, c: &crate::usage::Credential) -> Result<crate::state::Windows, crate::usage::FetchError> {
                self.inner.fetch_usage(a, c)
            }
            fn refresh(&self, a: &Account, b: &str, now: u64) -> Result<String, RefreshError> {
                let text = std::fs::read_to_string(self.paths.state_file()).unwrap();
                self.seen.store(text.contains("refresh_inflight") && text.contains(&crate::vault::lineage_fingerprint("rt-a")) && !text.contains("rt-a\""), Ordering::SeqCst);
                self.inner.refresh(a, b, now)
            }
        }
        let f = Fixture::new(&[("a", Provider::Claude, 10)]);
        let peek = Peek { inner: &f.api, paths: &f.paths, seen: AtomicBool::new(false) };
        let env = f.env();
        freshen(&Ctx { env: &env, api: &peek }, f.config.account("a").unwrap(), T0, FRESHEN_WITHIN_S).unwrap();
        assert!(peek.seen.load(Ordering::SeqCst));
    }

    #[test]
    fn invalid_grant_quarantines_and_is_not_retried() {
        let f = Fixture::new(&[("a", Provider::Claude, 10)]);
        f.api.script_refresh("a", Err(RefreshError::Dead("invalid_grant".into())));
        assert!(matches!(run(&f, "a", T0), Err(FreshenError::Dead(_))));
        let rec = State::load(&f.paths).unwrap().usage["a"].clone();
        assert_eq!(rec.state, UsageState::Dead);
        assert!(matches!(run(&f, "a", T0 + 5), Err(FreshenError::Dead(_))));
        assert_eq!(f.api.count("refresh:"), 1, "the second call is refused locally");
        assert!(State::load(&f.paths).unwrap().refresh_inflight.is_empty());
    }

    #[test]
    fn invalid_client_stops_every_refresh_and_quarantines_nobody() {
        let f = Fixture::new(&[("a", Provider::Claude, 10), ("b", Provider::Claude, 10)]);
        f.api.script_refresh("a", Err(RefreshError::Systemic("invalid_client".into())));
        assert!(matches!(run(&f, "a", T0), Err(FreshenError::Halted(_))));
        let st = State::load(&f.paths).unwrap();
        assert!(st.usage.values().all(|r| r.state != UsageState::Dead), "no account is quarantined for a client error");
        assert!(st.refresh_halted(T0 + 1));
        assert!(matches!(run(&f, "b", T0 + 1), Err(FreshenError::Halted(_))));
        assert_eq!(f.api.count("refresh:"), 1, "the other account sent nothing");
        // The pause ends on its own.
        f.api.script_refresh("b", Ok(blob(T0 + HALT_S + 100_000, "rt-b2")));
        assert_eq!(run(&f, "b", T0 + HALT_S + 1).unwrap(), Freshened::Refreshed);
    }

    #[test]
    fn transient_errors_keep_the_login_and_only_ambiguous_ones_keep_the_journal() {
        let f = Fixture::new(&[("a", Provider::Claude, 10)]);
        f.api.script_refresh("a", Err(RefreshError::Transient { detail: "network".into(), may_have_consumed: false }));
        assert!(matches!(run(&f, "a", T0), Err(FreshenError::Transient(_))));
        let st = State::load(&f.paths).unwrap();
        assert!(st.refresh_inflight.is_empty() && st.usage.is_empty());

        f.api.script_refresh("a", Err(RefreshError::Transient { detail: "timeout".into(), may_have_consumed: true }));
        assert!(matches!(run(&f, "a", T0 + 1), Err(FreshenError::Transient(_))));
        let st = State::load(&f.paths).unwrap();
        assert!(interrupted(&st, "a").is_some(), "the server may have rotated the token");
        assert!(f.vault.get("a", "claude-oauth").unwrap().unwrap().blob.contains("rt-a"), "the saved login is untouched");

        // An interrupted refresh followed by invalid_grant says why.
        f.api.script_refresh("a", Err(RefreshError::Dead("invalid_grant".into())));
        let Err(FreshenError::Dead(m)) = run(&f, "a", T0 + 2) else { panic!("expected dead") };
        assert!(m.contains("did not finish"), "{m}");
    }

    #[test]
    fn an_account_active_on_a_chain_holding_surface_is_never_refreshed() {
        let f = Fixture::new(&[("a", Provider::Claude, -100)]);
        f.set_active(SurfaceKind::ClaudeCli, "a");
        assert_eq!(run(&f, "a", T0).unwrap(), Freshened::Skipped);
        assert_eq!(f.api.count("refresh:"), 0);
        // Claude Desktop keeps no token chain: its account may be refreshed.
        let g = Fixture::new(&[("b", Provider::Claude, -100)]);
        g.set_active(SurfaceKind::ClaudeDesktop, "b");
        assert_eq!(run(&g, "b", T0).unwrap(), Freshened::Refreshed);
    }

    #[test]
    fn an_account_made_live_while_waiting_for_the_lock_is_not_refreshed() {
        let f = Fixture::new(&[("a", Provider::Claude, -100)]);
        let held = AccountLock::acquire(&f.paths, "a", Duration::from_secs(1)).unwrap();
        let result = std::thread::scope(|scope| {
            let worker = scope.spawn(|| run(&f, "a", T0));
            std::thread::sleep(Duration::from_millis(300));
            f.set_active(SurfaceKind::ClaudeCli, "a");
            drop(held);
            worker.join().unwrap()
        });
        assert_eq!(result.unwrap(), Freshened::Skipped);
        assert_eq!(f.api.count("refresh:"), 0, "the live chain must not be rotated");
    }

    #[test]
    fn a_failed_commit_marks_the_account_dead_instead_of_losing_the_token_silently() {
        use crate::keychain::Keychain;
        struct Failing(crate::keychain::MemoryKeychain, std::sync::atomic::AtomicBool);
        impl Keychain for Failing {
            fn name(&self) -> &'static str {
                "failing"
            }
            fn get(&self, s: &str, a: &str) -> crate::Result<Option<String>> {
                self.0.get(s, a)
            }
            fn set(&self, s: &str, a: &str, v: &str) -> crate::Result<()> {
                if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(Error::Keychain("locked".into()));
                }
                self.0.set(s, a, v)
            }
            fn delete(&self, s: &str, a: &str) -> crate::Result<bool> {
                self.0.delete(s, a)
            }
        }
        let f = Fixture::new(&[("a", Provider::Claude, 10)]);
        let kc = std::sync::Arc::new(Failing(crate::keychain::MemoryKeychain::new(), std::sync::atomic::AtomicBool::new(false)));
        let vault = crate::vault::Vault::new(kc.clone());
        let slot = f.vault.get("a", "claude-oauth").unwrap().unwrap();
        vault.put("a", "claude-oauth", &slot).unwrap();
        kc.1.store(true, std::sync::atomic::Ordering::SeqCst);
        let env = crate::surface::Env { paths: &f.paths, vault: &vault };
        let r = freshen(&Ctx { env: &env, api: &f.api }, f.config.account("a").unwrap(), T0, FRESHEN_WITHIN_S);
        assert!(matches!(r, Err(FreshenError::Other(Error::Keychain(_)))), "{r:?}");
        let st = State::load(&f.paths).unwrap();
        assert_eq!(st.usage["a"].state, UsageState::Dead);
        assert!(st.refresh_inflight.is_empty());
    }
}
