//! Antigravity CLI (`agy`) adapter.
//!
//! `agy` reads its login from one Keychain generic password (service `gemini`, account `antigravity`), in the
//! go-keyring format (`providers::antigravity::decode_item`). A switch replaces that value and nothing else.
//!
//! * The vault slot of an account is the decoded login JSON, every key kept. The slot fingerprint is that of
//!   the refresh token, which Google does not rotate, so a login `agy` refreshed still matches its slot.
//! * The live login belongs to the saved account whose fingerprint it has; failing that, to the saved account
//!   with the same email (the user signed in to that account again inside `agy`, which starts a new chain).
//!   A live login no saved account owns is never overwritten: the switch stops and asks the user to `add`
//!   it first. A running `agy` that writes its old login back after a switch is caught the same way: the
//!   item then names the old account, `status` says so, and the next switch saves it back to that account.
//! * `apply` first saves the live login back to its owner's slot, under the owner's account lock, so a token
//!   `agy` refreshed is never lost, then writes the target login. The live item and the slot are read again
//!   under that lock, and a slot is never replaced by an older login (by `token.expiry`); a login of another
//!   chain found by email replaces it only when strictly newer. When the target already owns the live login
//!   and that login is the one saved, the live copy stays.
//! * Like the other CLI surfaces, `stage` does not refresh anything: the watcher freshens the target before an
//!   automatic switch, and `agy` refreshes an expired access token itself.
//! * The live item goes through `/usr/bin/security` (`SecurityCli`), the tool go-keyring itself uses, so the
//!   item's access list already trusts it.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{mask_email, Account, Config, Provider, SurfaceKind};
use crate::doctor::{Check, Level};
use crate::error::{Error, Result};
use crate::keychain::Keychain;
use crate::locks::AccountLock;
use crate::providers::antigravity::{decode_item, encode_item, Login};
use crate::security_cli::{SecurityCli, TIMEOUT};
use crate::state::now_secs;
use crate::surface::{Env, Identity, Staged, Surface};
use crate::vault::Slot;

/// `agy`'s Keychain item.
pub const LIVE_SERVICE: &str = "gemini";
pub const LIVE_ACCOUNT: &str = "antigravity";
/// Overrides the Keychain service of the live item in test and debug builds only; release builds always use
/// `gemini`. Tests point it at an item that cannot exist, so they never read the real login.
pub const SERVICE_ENV: &str = "AGENT_SWITCH_ANTIGRAVITY_SERVICE";

const PROVIDER: Provider = Provider::Antigravity;

type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A saved Antigravity account and the login in its slot.
struct Saved {
    id: String,
    email: Option<String>,
    fp: String,
}

/// The saved account that owns `live`: by fingerprint, else by email. The flag is true for a fingerprint match.
fn owner<'a>(live: &Login, saved: &'a [Saved]) -> Option<(&'a Saved, bool)> {
    if let Some(s) = saved.iter().find(|s| s.fp == live.fingerprint) {
        return Some((s, true));
    }
    let email = live.email.as_deref()?;
    saved.iter().find(|s| s.email.as_deref().is_some_and(|e| e.eq_ignore_ascii_case(email))).map(|s| (s, false))
}

/// The access-token expiry of a login, `None` (older than any time) when it has none.
fn expiry(blob: &str) -> Option<u64> {
    crate::providers::antigravity::credential(blob).ok().and_then(|c| c.expires_at)
}

/// An account saved by `add`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub id: String,
    pub email: String,
    /// False when the account already existed.
    pub created: bool,
    /// False when the saved login was already the live one.
    pub vault_written: bool,
    pub dry_run: bool,
}

pub struct AntigravityCli {
    live: Arc<dyn Keychain>,
    #[cfg_attr(not(any(test, debug_assertions)), allow(dead_code))]
    getenv: EnvFn,
    agy_bin: PathBuf,
}

impl AntigravityCli {
    pub fn production() -> AntigravityCli {
        AntigravityCli::new(Arc::new(SecurityCli::default()), Arc::new(|k| std::env::var(k).ok()))
    }

    pub fn new(live: Arc<dyn Keychain>, getenv: EnvFn) -> AntigravityCli {
        AntigravityCli { live, getenv, agy_bin: PathBuf::from("agy") }
    }

    pub fn with_agy_binary(mut self, bin: impl Into<PathBuf>) -> AntigravityCli {
        self.agy_bin = bin.into();
        self
    }

    fn service(&self) -> String {
        #[cfg(any(test, debug_assertions))]
        if let Some(service) = (self.getenv)(SERVICE_ENV).filter(|s| !s.is_empty()) {
            return service;
        }
        LIVE_SERVICE.to_string()
    }

    /// The live login as JSON text, or `None` when `agy` is logged out.
    fn read_live(&self) -> Result<Option<String>> {
        self.live.get(&self.service(), LIVE_ACCOUNT)?.map(|text| decode_item(&text)).transpose()
    }

    fn saved(&self, env: &Env) -> Result<Vec<Saved>> {
        let config = Config::load(env.paths)?;
        let mut out = Vec::new();
        for a in config.accounts.iter().filter(|a| a.provider == PROVIDER) {
            if let Some(slot) = env.vault.get(&a.id, PROVIDER.store())? {
                out.push(Saved { id: a.id.clone(), email: a.email.clone(), fp: slot.fingerprint });
            }
        }
        Ok(out)
    }

    fn not_saved(live: &Login) -> Error {
        let who = live.email.as_deref().map(|e| format!(" ({})", mask_email(e))).unwrap_or_default();
        Error::Invalid(format!(
            "the live Antigravity login{who} is not saved in agent-switch, and switching would overwrite it. Nothing was changed. Save it first with `agent-switch add antigravity-cli --id <name>`"
        ))
    }

    /// Save the live `agy` login as account `id`. Safe to repeat: a second run with the same login changes
    /// nothing. Never deletes the Keychain item. `dry_run` reports without writing.
    pub fn import_login(&self, env: &Env, id: &str, label: Option<&str>, dry_run: bool) -> Result<Imported> {
        crate::config::validate_account_id(id)?;
        let blob = self.read_live()?.ok_or_else(|| {
            Error::NotFound("an Antigravity login in the Keychain. Run `agy`, sign in, then run this command again".into())
        })?;
        let login = Login::parse(&blob)?;
        let email = login
            .email
            .clone()
            .ok_or_else(|| Error::Invalid("the Antigravity login names no account (no id_token_email); sign in again in agy".into()))?;
        let config = Config::load(env.paths)?;
        let same_email = |a: &Account| a.email.as_deref().is_some_and(|e| e.eq_ignore_ascii_case(&email));
        if let Some(other) = config.accounts.iter().find(|a| a.provider == PROVIDER && a.id != id && same_email(a)) {
            return Err(Error::Invalid(format!("this Antigravity account is already saved as `{}`", other.id)));
        }
        let created = match config.account(id) {
            Some(a) if a.provider != PROVIDER => return Err(Error::Invalid(format!("`{id}` is a {} account", a.provider))),
            Some(a) if !same_email(a) => return Err(Error::Invalid(format!("`{id}` is a different Antigravity account; choose another --id"))),
            Some(_) => false,
            None => true,
        };
        let current = env.vault.get(id, PROVIDER.store())?;
        let vault_written = current.as_ref().is_none_or(|s| s.blob != blob);
        let result = Imported { id: id.to_string(), email: email.clone(), created, vault_written, dry_run };
        if dry_run {
            return Ok(result);
        }
        let _lock = AccountLock::acquire(env.paths, id, crate::switch::LOCK_TIMEOUT)?;
        if vault_written {
            env.vault.put(id, PROVIDER.store(), &Slot { blob, fingerprint: login.fingerprint, refreshed_at: now_secs() })?;
        }
        if created {
            Config::update(env.paths, |config| {
                config.add_account(Account {
                    id: id.to_string(),
                    provider: PROVIDER,
                    label: label.map(str::to_string).unwrap_or_else(|| id.to_string()),
                    email: Some(email),
                    uuid: None,
                })
            })?;
        }
        Ok(result)
    }

    /// Checks for `agent-switch doctor`: the Keychain item and the installed `agy`.
    pub fn doctor_checks(&self, env: &Env) -> Vec<Check> {
        let mk = |level, name, detail: String| Check { level, name, detail };
        let mut out = Vec::new();
        let service = self.service();
        let in_use = Config::load(env.paths).is_ok_and(|c| c.surfaces.get(SurfaceKind::AntigravityCli).account_id().is_some());
        match self.read_live() {
            Err(e) => out.push(mk(Level::Fail, "antigravity storage", format!("Keychain item `{service}`/`{LIVE_ACCOUNT}`: {e}"))),
            Ok(None) => out.push(mk(
                if in_use { Level::Warn } else { Level::Skip },
                "antigravity storage",
                format!("no login in Keychain item `{service}`/`{LIVE_ACCOUNT}` (agy is logged out or not used)"),
            )),
            Ok(Some(blob)) => {
                let fp = Login::parse(&blob).map(|l| l.fingerprint).unwrap_or_default();
                out.push(mk(Level::Ok, "antigravity storage", format!("Keychain item `{service}`/`{LIVE_ACCOUNT}` holds a readable login (fingerprint {fp})")));
                match self.current_identity(env) {
                    Ok(Some(Identity { account_id: None, .. })) => out.push(mk(
                        Level::Warn,
                        "antigravity storage",
                        "the live agy login is not saved in agent-switch. Fix: `agent-switch add antigravity-cli --id <name>`".into(),
                    )),
                    Ok(_) => {}
                    Err(e) => out.push(mk(Level::Warn, "antigravity storage", format!("identity check failed: {e}"))),
                }
            }
        }
        out.push(match crate::proc::run(&self.agy_bin, &["--version"], None, TIMEOUT) {
            Ok(o) if o.code == 0 => {
                let first: String = String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").trim().chars().take(80).collect();
                mk(Level::Ok, "agy version", if first.is_empty() { "agy is installed".into() } else { first })
            }
            Ok(o) => mk(Level::Warn, "agy version", format!("`agy --version` exited {}", o.code)),
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                mk(Level::Skip, "agy version", "agy is not installed (not found on PATH)".into())
            }
            Err(e) => mk(Level::Warn, "agy version", format!("`agy --version` failed: {e}")),
        });
        out
    }
}

impl Surface for AntigravityCli {
    fn kind(&self) -> SurfaceKind {
        SurfaceKind::AntigravityCli
    }

    fn current_identity(&self, env: &Env) -> Result<Option<Identity>> {
        let Some(blob) = self.read_live()? else { return Ok(None) };
        let live = Login::parse(&blob)?;
        let saved = self.saved(env)?;
        let (account_id, note) = match owner(&live, &saved) {
            Some((s, true)) => (Some(s.id.clone()), None),
            Some((s, false)) => (Some(s.id.clone()), Some("signed in again since it was saved".to_string())),
            None => (None, Some("login not saved in agent-switch".to_string())),
        };
        Ok(Some(Identity { account_id, email: live.email, uuid: None, note }))
    }

    fn capture(&self, _env: &Env, _account: &Account) -> Result<Option<Slot>> {
        let Some(blob) = self.read_live()? else { return Ok(None) };
        let fingerprint = Login::parse(&blob)?.fingerprint;
        Ok(Some(Slot { blob, fingerprint, refreshed_at: now_secs() }))
    }

    fn stage(&self, env: &Env, account: &Account) -> Result<Staged> {
        let slot = env.vault.get(&account.id, PROVIDER.store())?.ok_or_else(|| {
            Error::NotFound(format!("saved login for `{}`; sign in to it in agy and run `agent-switch add antigravity-cli --id {}`", account.id, account.id))
        })?;
        Login::parse(&slot.blob)?;
        Ok(Staged::new(account.id.clone(), slot.blob.into_bytes()))
    }

    fn apply(&self, env: &Env, staged: Staged) -> Result<()> {
        let target = String::from_utf8(staged.payload).map_err(|_| Error::Invalid("bad staged payload".into()))?;
        Login::parse(&target)?;
        let service = self.service();

        // Save the outgoing login back first, under its owner's account lock, so a refreshed token is not lost.
        let mut _owner_lock = None;
        let mut live_is_target = false;
        if let Some(first) = self.read_live()? {
            let first = Login::parse(&first)?;
            let saved = self.saved(env)?;
            let owner_id = owner(&first, &saved).ok_or_else(|| Self::not_saved(&first))?.0.id.clone();
            if !staged.held.contains(&owner_id) {
                _owner_lock = Some(AccountLock::acquire(env.paths, &owner_id, crate::switch::LOCK_TIMEOUT)?);
            }
            // Read the live item and the owner's slot again under the lock: `agy` may have written the item, or a
            // refresh the slot, while we waited.
            if let Some(live_blob) = self.read_live()? {
                let live = Login::parse(&live_blob)?;
                let (owner, same_chain) = owner(&live, &saved).ok_or_else(|| Self::not_saved(&live))?;
                if owner.id != owner_id {
                    return Err(Error::Invalid("the live Antigravity login changed during the switch; nothing was written, try again".into()));
                }
                let current = env.vault.get(&owner_id, PROVIDER.store())?;
                // Never replace a saved login with an older one. A login of another chain (a new sign-in to the
                // same email) replaces the slot only when it is strictly newer.
                let replace = match &current {
                    None => true,
                    Some(slot) if slot.blob == live_blob => false,
                    Some(slot) if same_chain => expiry(&live_blob) >= expiry(&slot.blob),
                    Some(slot) => expiry(&live_blob) > expiry(&slot.blob),
                };
                let live_is_saved = replace || current.is_some_and(|s| s.blob == live_blob);
                if replace {
                    env.vault.put(&owner_id, PROVIDER.store(), &Slot { blob: live_blob, fingerprint: live.fingerprint, refreshed_at: now_secs() })?;
                }
                // The target's own login stays live when it is the one now saved; an older one is replaced.
                live_is_target = owner_id == staged.account_id && live_is_saved;
            }
        }
        if live_is_target {
            return Ok(());
        }
        self.live.set(&service, LIVE_ACCOUNT, &encode_item(&target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Binding;
    use crate::keychain::MemoryKeychain;
    use crate::providers::antigravity::credential;
    use crate::providers::antigravity::tests::{item, login_json};
    use crate::state::State;
    use crate::switch;
    use crate::vault::{lineage_fingerprint, Vault};

    const EXP: &str = "2026-11-06T16:58:16.636636+07:00";
    const LATER: &str = "2026-11-06T18:00:00+07:00";

    struct Fixture {
        _home: tempfile::TempDir,
        paths: crate::paths::Paths,
        vault: Vault,
        live: Arc<MemoryKeychain>,
        agy: AntigravityCli,
    }

    impl Fixture {
        fn new() -> Fixture {
            let home = tempfile::tempdir().unwrap();
            let paths = crate::paths::Paths::from_home(home.path());
            let live = Arc::new(MemoryKeychain::new());
            let agy = AntigravityCli::new(live.clone(), Arc::new(|_| None)).with_agy_binary(home.path().join("no-agy"));
            Fixture { _home: home, paths, vault: Vault::new(Arc::new(MemoryKeychain::new())), live, agy }
        }

        fn env(&self) -> Env<'_> {
            Env { paths: &self.paths, vault: &self.vault }
        }

        /// Put a login into `agy`'s Keychain item, as `agy` would.
        fn sign_in(&self, email: &str, access: &str, refresh: &str) {
            self.sign_in_until(email, access, refresh, EXP);
        }

        /// As `sign_in`, with the access token valid until `expiry`.
        fn sign_in_until(&self, email: &str, access: &str, refresh: &str, expiry: &str) {
            self.live.set(LIVE_SERVICE, LIVE_ACCOUNT, &item(email, access, refresh, expiry)).unwrap();
        }

        fn live_login(&self) -> String {
            decode_item(&self.live.get(LIVE_SERVICE, LIVE_ACCOUNT).unwrap().unwrap()).unwrap()
        }

        fn add(&self, id: &str, email: &str, access: &str, refresh: &str) {
            self.sign_in(email, access, refresh);
            self.agy.import_login(&self.env(), id, None, false).unwrap();
        }

        fn slot(&self, id: &str) -> Slot {
            self.vault.get(id, "antigravity-oauth").unwrap().unwrap()
        }

        fn config(&self) -> Config {
            Config::load(&self.paths).unwrap()
        }
    }

    #[test]
    fn add_saves_the_live_login_and_a_second_run_changes_nothing() {
        let f = Fixture::new();
        f.sign_in("me@gmail.com", "ya29.a", "1//rt-a");
        let dry = f.agy.import_login(&f.env(), "a", Some("Me"), true).unwrap();
        assert!(dry.dry_run && dry.created && dry.vault_written);
        assert!(f.vault.get("a", "antigravity-oauth").unwrap().is_none() && f.config().accounts.is_empty(), "a dry run writes nothing");

        let done = f.agy.import_login(&f.env(), "a", Some("Me"), false).unwrap();
        assert!(done.created && done.vault_written && done.email == "me@gmail.com");
        let acct = f.config().account("a").unwrap().clone();
        assert_eq!((acct.provider, acct.label.as_str(), acct.email.as_deref()), (Provider::Antigravity, "Me", Some("me@gmail.com")));
        let slot = f.slot("a");
        assert_eq!(slot.blob, f.live_login());
        assert_eq!(slot.fingerprint, lineage_fingerprint("1//rt-a"));

        let config_before = std::fs::read_to_string(f.paths.config_file()).unwrap();
        let again = f.agy.import_login(&f.env(), "a", None, false).unwrap();
        assert!(!again.created && !again.vault_written);
        assert_eq!(f.slot("a"), slot, "the slot, with its time, is untouched");
        assert_eq!(std::fs::read_to_string(f.paths.config_file()).unwrap(), config_before);
        assert!(f.live.get(LIVE_SERVICE, LIVE_ACCOUNT).unwrap().is_some(), "the source item is never deleted");
    }

    #[test]
    fn add_refuses_without_a_login_and_for_mismatched_accounts() {
        let f = Fixture::new();
        let err = f.agy.import_login(&f.env(), "a", None, false).unwrap_err().to_string();
        assert!(err.contains("Run `agy`, sign in"), "{err}");
        f.add("a", "me@gmail.com", "ya29.a", "1//rt-a");
        // The same Google account under another id.
        let err = f.agy.import_login(&f.env(), "b", None, false).unwrap_err().to_string();
        assert!(err.contains("already saved as `a`"), "{err}");
        // Another Google account under an existing id.
        f.sign_in("other@gmail.com", "ya29.b", "1//rt-b");
        let err = f.agy.import_login(&f.env(), "a", None, false).unwrap_err().to_string();
        assert!(err.contains("different Antigravity account"), "{err}");
        // An id of another provider.
        Config::update(&f.paths, |c| c.add_account(Account { id: "c".into(), provider: Provider::Claude, label: "c".into(), email: None, uuid: None })).unwrap();
        assert!(f.agy.import_login(&f.env(), "c", None, false).unwrap_err().to_string().contains("claude account"));
        assert!(f.agy.import_login(&f.env(), "Bad Id", None, false).is_err());
    }

    #[test]
    fn identity_matches_by_fingerprint_then_email_and_reports_unsaved_logins() {
        let f = Fixture::new();
        assert_eq!(f.agy.current_identity(&f.env()).unwrap(), None, "logged out");
        f.add("a", "me@gmail.com", "ya29.a", "1//rt-a");
        // agy refreshed the access token: the refresh token, and so the match, is the same.
        f.sign_in("me@gmail.com", "ya29.a2", "1//rt-a");
        let id = f.agy.current_identity(&f.env()).unwrap().unwrap();
        assert_eq!((id.account_id.as_deref(), id.email.as_deref(), id.note), (Some("a"), Some("me@gmail.com"), None));
        // A new sign-in to the same Google account starts a new chain.
        f.sign_in("ME@gmail.com", "ya29.a3", "1//rt-a-new");
        let id = f.agy.current_identity(&f.env()).unwrap().unwrap();
        assert_eq!((id.account_id.as_deref(), id.note.as_deref()), (Some("a"), Some("signed in again since it was saved")));
        f.sign_in("stranger@gmail.com", "ya29.x", "1//rt-x");
        let id = f.agy.current_identity(&f.env()).unwrap().unwrap();
        assert_eq!((id.account_id, id.note.as_deref()), (None, Some("login not saved in agent-switch")));
    }

    #[test]
    fn a_switch_writes_the_target_in_agy_format_and_saves_the_refreshed_outgoing_login_first() {
        let f = Fixture::new();
        f.add("a", "a@gmail.com", "ya29.a", "1//rt-a");
        f.add("b", "b@gmail.com", "ya29.b", "1//rt-b");
        let mut cfg = f.config();
        cfg.bind(SurfaceKind::AntigravityCli, Binding::parse("b")).unwrap();
        cfg.save(&f.paths).unwrap();
        switch::activate(&f.env(), &cfg, &f.agy, "b").unwrap();
        assert_eq!(f.live_login(), f.slot("b").blob);

        // agy refreshed b's access token while it ran, then the user switches to a.
        f.sign_in("b@gmail.com", "ya29.b-refreshed", "1//rt-b");
        let refreshed = f.live_login();
        switch::activate(&f.env(), &cfg, &f.agy, "a").unwrap();
        assert_eq!(f.slot("b").blob, refreshed, "the refreshed login of b was kept");
        let raw = f.live.get(LIVE_SERVICE, LIVE_ACCOUNT).unwrap().unwrap();
        assert!(raw.starts_with("go-keyring-base64:"), "agy's own format");
        assert_eq!(decode_item(&raw).unwrap(), f.slot("a").blob);
        assert_eq!(f.agy.current_identity(&f.env()).unwrap().unwrap().account_id.as_deref(), Some("a"));
        assert_eq!(State::load(&f.paths).unwrap().active(SurfaceKind::AntigravityCli), Some("a"));

        // Switching back puts b back, with its refreshed token.
        switch::activate(&f.env(), &cfg, &f.agy, "b").unwrap();
        assert_eq!(f.live_login(), refreshed);
    }

    #[test]
    fn switching_to_the_account_that_is_already_live_keeps_the_live_copy() {
        let f = Fixture::new();
        f.add("a", "a@gmail.com", "ya29.a", "1//rt-a");
        f.sign_in("a@gmail.com", "ya29.a-newer", "1//rt-a");
        let newer = f.live_login();
        switch::activate(&f.env(), &f.config(), &f.agy, "a").unwrap();
        assert_eq!(f.live_login(), newer, "the stale vault copy did not replace the live one");
        assert_eq!(f.slot("a").blob, newer);
    }

    #[test]
    fn an_unsaved_live_login_is_never_overwritten() {
        let f = Fixture::new();
        f.add("a", "a@gmail.com", "ya29.a", "1//rt-a");
        f.sign_in("stranger@gmail.com", "ya29.x", "1//rt-x");
        let before = f.live.get(LIVE_SERVICE, LIVE_ACCOUNT).unwrap();
        let err = switch::activate(&f.env(), &f.config(), &f.agy, "a").unwrap_err().to_string();
        assert!(err.contains("not saved") && err.contains("st***@gmail.com") && err.contains("add antigravity-cli"), "{err}");
        assert!(!err.contains("ya29") && !err.contains("rt-x"), "{err}");
        assert_eq!(f.live.get(LIVE_SERVICE, LIVE_ACCOUNT).unwrap(), before);
        assert_eq!(State::load(&f.paths).unwrap().active(SurfaceKind::AntigravityCli), None);
    }

    #[test]
    fn a_login_written_back_by_a_running_agy_is_saved_to_the_account_it_belongs_to() {
        let f = Fixture::new();
        f.add("a", "a@gmail.com", "ya29.a", "1//rt-a");
        f.add("b", "b@gmail.com", "ya29.b", "1//rt-b");
        f.add("c", "c@gmail.com", "ya29.c", "1//rt-c");
        let cfg = f.config();
        switch::activate(&f.env(), &cfg, &f.agy, "a").unwrap();
        // A running session still on b writes its refreshed login over the item.
        f.sign_in("b@gmail.com", "ya29.b-late", "1//rt-b");
        let late = f.live_login();
        assert_eq!(f.agy.current_identity(&f.env()).unwrap().unwrap().account_id.as_deref(), Some("b"));
        // The next switch saves it to b (whose lock the switch did not hold) and moves on.
        switch::activate(&f.env(), &cfg, &f.agy, "c").unwrap();
        assert_eq!(f.slot("b").blob, late);
        assert_eq!(f.live_login(), f.slot("c").blob);
    }

    #[test]
    fn a_new_sign_in_of_a_saved_account_replaces_its_chain_on_the_next_switch() {
        let f = Fixture::new();
        f.add("a", "a@gmail.com", "ya29.a", "1//rt-a");
        f.add("b", "b@gmail.com", "ya29.b", "1//rt-b");
        f.sign_in_until("a@gmail.com", "ya29.a2", "1//rt-a2", LATER);
        switch::activate(&f.env(), &f.config(), &f.agy, "b").unwrap();
        assert_eq!(f.slot("a").fingerprint, lineage_fingerprint("1//rt-a2"));
    }

    #[test]
    fn an_older_login_of_another_chain_with_the_same_email_never_replaces_the_saved_one() {
        let f = Fixture::new();
        f.sign_in_until("a@gmail.com", "ya29.a2", "1//rt-a2", LATER);
        f.agy.import_login(&f.env(), "a", None, false).unwrap();
        f.add("b", "b@gmail.com", "ya29.b", "1//rt-b");
        let newer = f.slot("a");
        // A running session on the old chain writes it back over the item.
        f.sign_in("a@gmail.com", "ya29.a1", "1//rt-a1");
        switch::activate(&f.env(), &f.config(), &f.agy, "b").unwrap();
        assert_eq!(f.slot("a"), newer, "the newer chain is kept");
        assert_eq!(f.live_login(), f.slot("b").blob);
        // Switching back to a writes its saved (newer) login, not the stale one.
        f.sign_in("a@gmail.com", "ya29.a1", "1//rt-a1");
        switch::activate(&f.env(), &f.config(), &f.agy, "a").unwrap();
        assert_eq!(f.live_login(), newer.blob);
    }

    #[test]
    fn a_live_login_older_than_its_saved_copy_does_not_replace_it() {
        let f = Fixture::new();
        f.sign_in_until("a@gmail.com", "ya29.a-refreshed", "1//rt-a", LATER);
        f.agy.import_login(&f.env(), "a", None, false).unwrap();
        f.add("b", "b@gmail.com", "ya29.b", "1//rt-b");
        let refreshed = f.slot("a");
        f.sign_in("a@gmail.com", "ya29.a-old", "1//rt-a");
        switch::activate(&f.env(), &f.config(), &f.agy, "b").unwrap();
        assert_eq!(f.slot("a"), refreshed);
    }

    #[test]
    fn the_save_back_uses_the_live_login_read_under_the_owners_lock() {
        /// Writes `pending` into the item right after the first read, as `agy` would between two reads.
        struct WritesAfterFirstRead {
            inner: MemoryKeychain,
            pending: std::sync::Mutex<Option<String>>,
        }
        impl Keychain for WritesAfterFirstRead {
            fn name(&self) -> &'static str {
                "scripted"
            }
            fn get(&self, s: &str, a: &str) -> Result<Option<String>> {
                let got = self.inner.get(s, a);
                if let Some(v) = self.pending.lock().unwrap().take() {
                    self.inner.set(s, a, &v)?;
                }
                got
            }
            fn set(&self, s: &str, a: &str, v: &str) -> Result<()> {
                self.inner.set(s, a, v)
            }
            fn delete(&self, s: &str, a: &str) -> Result<bool> {
                self.inner.delete(s, a)
            }
        }
        let f = Fixture::new();
        f.add("a", "a@gmail.com", "ya29.a", "1//rt-a");
        f.add("b", "b@gmail.com", "ya29.b", "1//rt-b");
        let live = Arc::new(WritesAfterFirstRead { inner: MemoryKeychain::new(), pending: std::sync::Mutex::new(None) });
        live.inner.set(LIVE_SERVICE, LIVE_ACCOUNT, &item("b@gmail.com", "ya29.b1", "1//rt-b", EXP)).unwrap();
        *live.pending.lock().unwrap() = Some(item("b@gmail.com", "ya29.b2", "1//rt-b", LATER));
        let agy = AntigravityCli::new(live.clone(), Arc::new(|_| None));
        let staged = agy.stage(&f.env(), f.config().account("a").unwrap()).unwrap();
        agy.apply(&f.env(), staged).unwrap();
        assert_eq!(credential(&f.slot("b").blob).unwrap().access_token, "ya29.b2");
    }

    #[test]
    fn staging_an_account_without_a_saved_login_says_how_to_add_it() {
        let f = Fixture::new();
        Config::update(&f.paths, |c| c.add_account(Account { id: "g".into(), provider: Provider::Antigravity, label: "g".into(), email: None, uuid: None })).unwrap();
        let err = f.agy.stage(&f.env(), f.config().account("g").unwrap()).unwrap_err().to_string();
        assert!(err.contains("add antigravity-cli --id g"), "{err}");
    }

    #[test]
    fn capture_reads_the_live_login_for_usage() {
        let f = Fixture::new();
        f.sign_in("a@gmail.com", "ya29.a", "1//rt-a");
        let acct = Account { id: "a".into(), provider: Provider::Antigravity, label: "a".into(), email: None, uuid: None };
        let slot = f.agy.capture(&f.env(), &acct).unwrap().unwrap();
        assert_eq!(slot.blob, serde_json::from_str::<serde_json::Value>(&login_json("a@gmail.com", "ya29.a", "1//rt-a", EXP)).unwrap().to_string());
        assert_eq!(crate::providers::antigravity::credential(&slot.blob).unwrap().access_token, "ya29.a");
    }

    #[test]
    fn the_service_override_points_at_another_item() {
        let f = Fixture::new();
        f.sign_in("a@gmail.com", "ya29.a", "1//rt-a");
        let other = AntigravityCli::new(f.live.clone(), Arc::new(|k| (k == SERVICE_ENV).then(|| "nowhere".to_string())));
        assert_eq!(other.current_identity(&f.env()).unwrap(), None);
    }

    #[test]
    fn doctor_reports_the_item_and_tolerates_a_missing_agy() {
        let f = Fixture::new();
        let checks = f.agy.doctor_checks(&f.env());
        let storage = checks.iter().find(|c| c.name == "antigravity storage").unwrap();
        assert_eq!(storage.level, Level::Skip, "not in use: {}", storage.detail);
        let version = checks.iter().find(|c| c.name == "agy version").unwrap();
        assert_eq!(version.level, Level::Skip, "{}", version.detail);

        f.sign_in("stranger@gmail.com", "ya29.x", "1//rt-x");
        let checks = f.agy.doctor_checks(&f.env());
        assert!(checks.iter().any(|c| c.level == Level::Ok && c.detail.contains("readable login")), "{checks:?}");
        assert!(checks.iter().any(|c| c.level == Level::Warn && c.detail.contains("add antigravity-cli")), "{checks:?}");
        assert!(checks.iter().all(|c| !c.detail.contains("ya29") && !c.detail.contains("rt-x")), "{checks:?}");
    }

    #[test]
    fn doctor_shows_the_agy_version() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let bin = f._home.path().join("agy");
        std::fs::write(&bin, "#!/bin/sh\necho 'agy 1.5.1'\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let agy = AntigravityCli::new(f.live.clone(), Arc::new(|_| None)).with_agy_binary(&bin);
        let checks = agy.doctor_checks(&f.env());
        let version = checks.iter().find(|c| c.name == "agy version").unwrap();
        assert_eq!((version.level, version.detail.as_str()), (Level::Ok, "agy 1.5.1"));
    }
}
