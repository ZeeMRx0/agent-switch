//! Orchestration shared by every surface: activate, undo, bind and remove. Adapters supply
//! `stage`/`apply`; this module owns locking and the `state.json` bookkeeping around them.

use std::time::Duration;

use crate::config::{Account, Binding, Config, Provider, SurfaceKind};
use crate::error::{Error, Result};
use crate::locks::{lock_accounts, AccountLock};
use crate::paths::Paths;
use crate::state::{now_secs, State, SwitchRecord, QUARANTINE_LIMIT};
use crate::surface::{Env, Surface};
use crate::vault::{Slot, Vault};

pub const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

fn checked_account<'a>(config: &'a Config, kind: SurfaceKind, id: &str) -> Result<&'a Account> {
    let account = config.account(id).ok_or_else(|| Error::NotFound(format!("account `{id}`")))?;
    if account.provider != kind.provider() {
        return Err(Error::Invalid(format!(
            "surface `{kind}` needs a {} account, but `{id}` is a {} account",
            kind.provider(),
            account.provider
        )));
    }
    Ok(account)
}

/// Stage and apply `target` on `surface` under the account locks of the outgoing and incoming accounts.
/// The switch is journaled only after `apply` succeeds, so a failed apply leaves `state.json` unchanged.
pub fn activate(env: &Env, config: &Config, surface: &dyn Surface, target: &str) -> Result<()> {
    let kind = surface.kind();
    let account = checked_account(config, kind, target)?;
    let previous = State::load(env.paths)?.active(kind).map(str::to_string);

    let mut ids = vec![target];
    ids.extend(previous.as_deref());
    let _locks = lock_accounts(env.paths, &ids, LOCK_TIMEOUT)?;

    let mut staged = surface.stage(env, account)?;
    staged.held = ids.iter().map(|id| id.to_string()).collect();
    surface.apply(env, staged)?;
    State::update(env.paths, |s| {
        if s.active(kind) == Some(target) {
            return;
        }
        s.record_switch(kind, target, now_secs());
    })
}

/// The journal entry `undo` would act on.
pub fn undo_record(paths: &Paths, kind: Option<SurfaceKind>) -> Result<SwitchRecord> {
    State::load(paths)?.undo_target(kind).cloned().ok_or_else(|| Error::NotFound("a switch to undo".into()))
}

/// Re-activate the previous account of the newest switch on `surface`, from its current vault slot, and make
/// it the surface's first priority (an undo is a manual choice). Returns the restored account id.
pub fn undo(env: &Env, config: &Config, surface: &dyn Surface) -> Result<String> {
    let kind = surface.kind();
    let record = undo_record(env.paths, Some(kind))?;
    let previous = record.previous_account_id.clone().ok_or_else(|| {
        Error::Invalid(format!("`{kind}` had no known account before the last switch, so there is nothing to restore"))
    })?;
    let account = checked_account(config, kind, &previous)
        .map_err(|e| Error::Invalid(format!("cannot undo: {e}")))?;

    let ids = [previous.as_str(), record.new_account_id.as_str()];
    let _locks = lock_accounts(env.paths, &ids, LOCK_TIMEOUT)?;
    let mut staged = surface.stage(env, account)?;
    staged.held = ids.iter().map(|id| id.to_string()).collect();
    surface.apply(env, staged)?;
    State::update(env.paths, |s| s.complete_undo(kind))?;
    promote(env.paths, kind, &previous)?;
    Ok(previous)
}

/// Change the binding in `config.toml`. Activation is a separate step so a failed activation never
/// loses the user's choice. The priority is left alone; `choose` is the user's pick.
pub fn bind(paths: &Paths, kind: SurfaceKind, binding: Binding) -> Result<Config> {
    Config::update(paths, |config| {
        config.bind(kind, binding)?;
        Ok(config.clone())
    })
}

/// The user picked `binding` for `kind`: bind it and, for an account, make it the first priority, in one
/// write.
pub fn choose(paths: &Paths, kind: SurfaceKind, binding: Binding) -> Result<Config> {
    Config::update(paths, |config| {
        config.bind(kind, binding.clone())?;
        if let Some(id) = binding.account_id() {
            config.promote(kind, id)?;
        }
        Ok(config.clone())
    })
}

/// Make `id` the first priority of `kind` after the user switched to it by hand, so auto-switch returns to
/// it rather than away from it. Called only once the switch has succeeded.
pub fn promote(paths: &Paths, kind: SurfaceKind, id: &str) -> Result<()> {
    Config::update(paths, |config| config.promote(kind, id))
        .map_err(|e| Error::Invalid(format!("`{kind}` now uses `{id}`, but saving it as the first priority failed: {e}")))
}

/// Keep a recovery copy of a live Claude login that no saved account owns. The vault id is derived from
/// the login's fingerprint, so the same login overwrites its own copy. At most `QUARANTINE_LIMIT` copies
/// exist: the oldest are deleted to make room. The copy is recorded in `state.json`, which is how `doctor`
/// lists it and `remove` deletes it.
pub fn put_quarantine(paths: &Paths, vault: &Vault, id: &str, slot: &Slot) -> Result<()> {
    let store = Provider::Claude.store();
    let _lock = AccountLock::acquire(paths, id, LOCK_TIMEOUT)?;
    let known = State::load(paths)?.quarantine;
    let mut doomed: Vec<String> = Vec::new();
    if !known.contains_key(id) && known.len() >= QUARANTINE_LIMIT {
        let mut by_age: Vec<(&u64, &String)> = known.iter().map(|(k, t)| (t, k)).collect();
        by_age.sort();
        doomed = by_age.into_iter().take(known.len() + 1 - QUARANTINE_LIMIT).map(|(_, k)| k.clone()).collect();
    }
    vault.put(id, store, slot)?;
    let mut gone = Vec::new();
    for old in doomed {
        let deleted = AccountLock::acquire(paths, &old, LOCK_TIMEOUT).and_then(|_l| vault.delete(&old, store));
        if deleted.is_ok() {
            gone.push(old);
        }
    }
    let recorded = State::update(paths, |s| {
        for g in &gone {
            s.quarantine.remove(g);
        }
        s.quarantine.insert(id.to_string(), now_secs());
    });
    if let Err(e) = recorded {
        let _ = vault.delete(id, store);
        return Err(e);
    }
    Ok(())
}

/// Delete a quarantined login. `Ok(false)` means `id` is not a quarantined login.
pub fn remove_quarantine(paths: &Paths, vault: &Vault, id: &str) -> Result<bool> {
    if !State::load(paths)?.quarantine.contains_key(id) {
        return Ok(false);
    }
    let _lock = AccountLock::acquire(paths, id, LOCK_TIMEOUT)?;
    vault.delete(id, Provider::Claude.store())?;
    State::update(paths, |s| s.quarantine.remove(id))?;
    Ok(true)
}

/// Delete an account's vault slot and config entry. Refuses while any surface is bound to it or has it active.
pub fn remove_account(paths: &Paths, vault: &Vault, id: &str) -> Result<Account> {
    let _lock = AccountLock::acquire(paths, id, LOCK_TIMEOUT)?;
    let account = Config::update(paths, |config| {
        let active_on = State::load(paths)?.is_active_anywhere(id);
        if !active_on.is_empty() {
            let names: Vec<_> = active_on.iter().map(|k| k.name()).collect();
            return Err(Error::Invalid(format!("account `{id}` is active on {}; switch those surfaces first", names.join(", "))));
        }
        config.remove_account(id) // also refuses while bound
    })?;
    State::update(paths, |s| s.forget_account(id))?;
    vault
        .delete(id, account.provider.store())
        .map_err(|e| Error::Keychain(format!("account `{id}` was removed from the config but its vault slot remains: {e}")))?;
    Ok(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Provider;
    use crate::keychain::MemoryKeychain;
    use crate::surface::{Identity, Staged};
    use crate::vault::{lineage_fingerprint, Slot};
    use std::sync::{Arc, Mutex};

    /// Reads the account's vault slot in `stage` and records what `apply` receives, like a real adapter.
    struct Fake {
        kind: SurfaceKind,
        applied: Mutex<Vec<(String, String)>>,
        held: Mutex<Vec<Vec<String>>>,
        fail_apply: bool,
    }

    impl Fake {
        fn new(kind: SurfaceKind) -> Self {
            Fake { kind, applied: Mutex::new(vec![]), held: Mutex::new(vec![]), fail_apply: false }
        }
    }

    impl Surface for Fake {
        fn kind(&self) -> SurfaceKind {
            self.kind
        }
        fn current_identity(&self, _: &Env) -> Result<Option<Identity>> {
            Ok(None)
        }
        fn stage(&self, env: &Env, account: &Account) -> Result<Staged> {
            let slot = env.vault.get(&account.id, account.provider.store())?.ok_or_else(|| Error::NotFound("slot".into()))?;
            Ok(Staged::new(account.id.clone(), slot.blob.into_bytes()))
        }
        fn apply(&self, _: &Env, staged: Staged) -> Result<()> {
            if self.fail_apply {
                return Err(Error::Invalid("disk full".into()));
            }
            self.held.lock().unwrap().push(staged.held);
            self.applied.lock().unwrap().push((staged.account_id, String::from_utf8(staged.payload).unwrap()));
            Ok(())
        }
    }

    struct Fixture {
        _home: tempfile::TempDir,
        paths: Paths,
        vault: Vault,
        config: Config,
    }

    fn slot(blob: &str) -> Slot {
        Slot { blob: blob.into(), fingerprint: lineage_fingerprint(blob), refreshed_at: 1 }
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(home.path());
        let vault = Vault::new(Arc::new(MemoryKeychain::new()));
        let mut config = Config::default();
        for (id, p) in [("personal", Provider::Claude), ("team", Provider::Claude), ("gpt", Provider::Openai)] {
            config.add_account(Account { id: id.into(), provider: p, label: id.into(), email: None, uuid: None }).unwrap();
            vault.put(id, p.store(), &slot(&format!(r#"{{"owner":"{id}","gen":1}}"#))).unwrap();
        }
        config.save(&paths).unwrap();
        Fixture { _home: home, paths, vault, config }
    }

    #[test]
    fn activate_applies_and_journals_once() {
        let f = fixture();
        let env = Env { paths: &f.paths, vault: &f.vault };
        let s = Fake::new(SurfaceKind::ClaudeCli);
        activate(&env, &f.config, &s, "personal").unwrap();
        activate(&env, &f.config, &s, "team").unwrap();
        activate(&env, &f.config, &s, "team").unwrap(); // same account again: applied, not journaled
        let st = State::load(&f.paths).unwrap();
        assert_eq!(st.active(SurfaceKind::ClaudeCli), Some("team"));
        assert_eq!(st.journal.len(), 2);
        assert_eq!(s.applied.lock().unwrap().len(), 3);
    }

    #[test]
    fn apply_receives_exactly_the_account_ids_that_were_locked() {
        let f = fixture();
        let env = Env { paths: &f.paths, vault: &f.vault };
        let s = Fake::new(SurfaceKind::ClaudeCli);
        activate(&env, &f.config, &s, "personal").unwrap();
        activate(&env, &f.config, &s, "team").unwrap();
        undo(&env, &f.config, &s).unwrap();
        let held = s.held.lock().unwrap();
        assert_eq!(held[0], ["personal"]);
        assert_eq!(held[1], ["team", "personal"]);
        let mut undone = held[2].clone();
        undone.sort();
        assert_eq!(undone, ["personal", "team"]);
    }

    #[test]
    fn quarantined_logins_can_be_removed_and_other_ids_are_not_touched() {
        let f = fixture();
        let q = slot(r#"{"owner":"stranger"}"#);
        put_quarantine(&f.paths, &f.vault, "quarantine-aaaa", &q).unwrap();
        assert!(State::load(&f.paths).unwrap().quarantine.contains_key("quarantine-aaaa"));
        assert!(!remove_quarantine(&f.paths, &f.vault, "team").unwrap(), "a saved account is not a quarantine item");
        assert!(f.vault.get("team", "claude-oauth").unwrap().is_some());
        assert!(remove_quarantine(&f.paths, &f.vault, "quarantine-aaaa").unwrap());
        assert!(f.vault.get("quarantine-aaaa", "claude-oauth").unwrap().is_none());
        assert!(State::load(&f.paths).unwrap().quarantine.is_empty());
        assert!(!remove_quarantine(&f.paths, &f.vault, "quarantine-aaaa").unwrap());
    }

    #[test]
    fn failed_apply_leaves_state_unchanged() {
        let f = fixture();
        let env = Env { paths: &f.paths, vault: &f.vault };
        let mut s = Fake::new(SurfaceKind::ClaudeCli);
        s.fail_apply = true;
        assert!(activate(&env, &f.config, &s, "personal").is_err());
        assert_eq!(State::load(&f.paths).unwrap(), State::default());
    }

    #[test]
    fn activate_rejects_provider_mismatch_and_unknown_accounts() {
        let f = fixture();
        let env = Env { paths: &f.paths, vault: &f.vault };
        let s = Fake::new(SurfaceKind::ClaudeCli);
        assert!(activate(&env, &f.config, &s, "gpt").is_err());
        assert!(activate(&env, &f.config, &s, "ghost").is_err());
        assert!(s.applied.lock().unwrap().is_empty());
    }

    #[test]
    fn undo_restores_the_previous_account_from_its_current_slot_not_a_stale_copy() {
        let f = fixture();
        let env = Env { paths: &f.paths, vault: &f.vault };
        let s = Fake::new(SurfaceKind::ClaudeCli);
        activate(&env, &f.config, &s, "personal").unwrap();
        activate(&env, &f.config, &s, "team").unwrap();
        // While `team` was active, `personal` was refreshed elsewhere: its refresh token rotated.
        f.vault.put("personal", "claude-oauth", &slot(r#"{"owner":"personal","gen":2}"#)).unwrap();

        let restored = undo(&env, &f.config, &s).unwrap();
        assert_eq!(restored, "personal");
        let applied = s.applied.lock().unwrap();
        let (id, payload) = applied.last().unwrap();
        assert_eq!(id, "personal");
        assert!(payload.contains(r#""gen":2"#), "undo must use the refreshed slot, got {payload}");
        let st = State::load(&f.paths).unwrap();
        assert_eq!(st.active(SurfaceKind::ClaudeCli), Some("personal"));
        assert_eq!(st.journal.len(), 1);
    }

    #[test]
    fn undo_with_nothing_to_restore_is_a_clear_error() {
        let f = fixture();
        let env = Env { paths: &f.paths, vault: &f.vault };
        let s = Fake::new(SurfaceKind::ClaudeCli);
        assert!(matches!(undo(&env, &f.config, &s), Err(Error::NotFound(_))));
        activate(&env, &f.config, &s, "personal").unwrap(); // first ever switch: no previous
        let err = undo(&env, &f.config, &s).unwrap_err();
        assert!(err.to_string().contains("nothing to restore"), "{err}");
    }

    #[test]
    fn undo_fails_when_the_previous_account_was_removed() {
        let f = fixture();
        let env = Env { paths: &f.paths, vault: &f.vault };
        let s = Fake::new(SurfaceKind::ClaudeCli);
        activate(&env, &f.config, &s, "personal").unwrap();
        activate(&env, &f.config, &s, "team").unwrap();
        let mut config = f.config.clone();
        config.remove_account("personal").unwrap();
        assert!(undo(&env, &config, &s).is_err());
    }

    #[test]
    fn bind_saves_only_valid_bindings() {
        let f = fixture();
        assert!(bind(&f.paths, SurfaceKind::ClaudeCli, Binding::parse("gpt")).is_err());
        assert_eq!(*Config::load(&f.paths).unwrap().surfaces.get(SurfaceKind::ClaudeCli), Binding::Unmanaged);
        bind(&f.paths, SurfaceKind::ClaudeCli, Binding::parse("team")).unwrap();
        assert_eq!(Config::load(&f.paths).unwrap().surfaces.get(SurfaceKind::ClaudeCli).account_id(), Some("team"));
    }

    #[test]
    fn remove_refuses_while_bound_or_active_then_cleans_everything() {
        let f = fixture();
        let env = Env { paths: &f.paths, vault: &f.vault };
        bind(&f.paths, SurfaceKind::ClaudeCli, Binding::parse("personal")).unwrap();
        assert!(remove_account(&f.paths, &f.vault, "personal").is_err());

        bind(&f.paths, SurfaceKind::ClaudeCli, Binding::Unmanaged).unwrap();
        let s = Fake::new(SurfaceKind::ClaudeCli);
        activate(&env, &f.config, &s, "personal").unwrap();
        let err = remove_account(&f.paths, &f.vault, "personal").unwrap_err();
        assert!(err.to_string().contains("active on claude-cli"), "{err}");

        activate(&env, &f.config, &s, "team").unwrap();
        remove_account(&f.paths, &f.vault, "personal").unwrap();
        assert!(Config::load(&f.paths).unwrap().account("personal").is_none());
        assert!(f.vault.get("personal", "claude-oauth").unwrap().is_none());
        assert!(State::load(&f.paths).unwrap().journal.is_empty());
        assert!(f.vault.get("team", "claude-oauth").unwrap().is_some());
    }

    #[test]
    fn choosing_an_account_binds_it_and_makes_it_the_first_priority() {
        let f = fixture();
        choose(&f.paths, SurfaceKind::ClaudeCli, Binding::parse("team")).unwrap();
        let c = Config::load(&f.paths).unwrap();
        assert_eq!(c.surfaces.get(SurfaceKind::ClaudeCli).account_id(), Some("team"));
        assert_eq!(c.priority.claude_cli, ["team", "personal"]);
        assert!(c.priority.claude_desktop.is_empty(), "other surfaces keep their order");
        // Unmanaged changes only the binding; a bad choice changes nothing.
        choose(&f.paths, SurfaceKind::ClaudeCli, Binding::Unmanaged).unwrap();
        assert!(choose(&f.paths, SurfaceKind::ClaudeCli, Binding::parse("gpt")).is_err());
        let c = Config::load(&f.paths).unwrap();
        assert_eq!((c.surfaces.get(SurfaceKind::ClaudeCli), c.priority.claude_cli.as_slice()), (&Binding::Unmanaged, ["team".to_string(), "personal".to_string()].as_slice()));
        // A plain bind (the rollback after a failed switch) leaves the priority alone.
        bind(&f.paths, SurfaceKind::ClaudeCli, Binding::parse("personal")).unwrap();
        assert_eq!(Config::load(&f.paths).unwrap().priority.claude_cli, ["team", "personal"]);
    }

    #[test]
    fn undo_makes_the_restored_account_the_first_priority() {
        let f = fixture();
        let env = Env { paths: &f.paths, vault: &f.vault };
        let s = Fake::new(SurfaceKind::ClaudeCli);
        activate(&env, &f.config, &s, "team").unwrap();
        activate(&env, &f.config, &s, "personal").unwrap();
        assert_eq!(undo(&env, &f.config, &s).unwrap(), "team");
        assert_eq!(Config::load(&f.paths).unwrap().priority(SurfaceKind::ClaudeCli), ["team", "personal"]);
    }

    #[test]
    fn remove_unknown_account_is_not_found() {
        let f = fixture();
        assert!(matches!(remove_account(&f.paths, &f.vault, "ghost"), Err(Error::NotFound(_))));
    }
}
