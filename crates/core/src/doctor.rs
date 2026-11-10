//! `agent-switch doctor`: checks owned by the core. Adapter phases append their own (Claude and Codex
//! storage formats, shallow-home drift, `cli_auth_credentials_store`, app versions).


use crate::config::{mask_email, Config, SurfaceKind};
use crate::paths::Paths;
use crate::state::State;
use crate::vault::Vault;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
    /// Not checkable yet because the owning phase is not built.
    Skip,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    pub name: &'static str,
    pub detail: String,
}

fn check(level: Level, name: &'static str, detail: impl Into<String>) -> Check {
    Check { level, name, detail: detail.into() }
}

pub struct DoctorInput<'a> {
    pub paths: &'a Paths,
    pub vault: &'a Vault,
}

pub fn run(input: &DoctorInput) -> Vec<Check> {
    let mut out = Vec::new();

    let config = match Config::load(input.paths) {
        Ok(c) => {
            out.push(check(Level::Ok, "config", format!("{} accounts in {}", c.accounts.len(), input.paths.config_file().display())));
            Some(c)
        }
        Err(e) => {
            out.push(check(Level::Fail, "config", e.to_string()));
            None
        }
    };

    let state = match State::load(input.paths) {
        Ok(s) => {
            out.push(check(Level::Ok, "state", format!("{} journal entries", s.journal.len())));
            Some(s)
        }
        Err(e) => {
            out.push(check(Level::Fail, "state", e.to_string()));
            None
        }
    };

    out.extend(keychain_checks());

    if let Some(config) = &config {
        for a in &config.accounts {
            let who = a.email.as_deref().map(mask_email).unwrap_or_else(|| a.label.clone());
            let level_detail = match input.vault.get(&a.id, a.provider.store()) {
                Ok(Some(slot)) => (Level::Ok, format!("{} ({who}) readable, fingerprint {}", a.id, slot.fingerprint)),
                Ok(None) => (Level::Warn, format!("{} ({who}) has no saved login; run `agent-switch add`", a.id)),
                Err(e) => (Level::Fail, format!("{} ({who}) not readable without UI: {e}", a.id)),
            };
            out.push(check(level_detail.0, "vault", level_detail.1));
        }
        if let Some(state) = &state {
            out.extend(consistency_checks(config, state));
        }
    }
    if let Some(state) = &state {
        out.extend(quarantine_checks(state));
    }
    out.extend(symlink_checks(input.paths));

    out.push(check(Level::Skip, "claude storage", "not implemented yet (phase 2)"));
    out.push(check(Level::Skip, "codex storage", "not implemented yet (phase 3): auth.json format, cli_auth_credentials_store, shallow-home drift"));
    out.push(check(Level::Skip, "app versions", "not implemented yet (phases 3-4)"));
    out
}

fn keychain_checks() -> Vec<Check> {
    vec![check(
        Level::Ok,
        "keychain",
        "using /usr/bin/security (no certificate needed; vault logins over about 4 KB are stored in parts)",
    )]
}

/// Recovery copies of live logins that no saved account owned (see `switch::put_quarantine`).
fn quarantine_checks(state: &State) -> Vec<Check> {
    state
        .quarantine
        .keys()
        .map(|id| {
            check(
                Level::Warn,
                "quarantine",
                format!("`{id}` is a recovery copy of a login that was live but not saved; keep it with `agent-switch add claude-cli --from-keychain-item ...` or delete it with `agent-switch remove {id}`"),
            )
        })
        .collect()
}

/// Files and folders agent-switch writes that must not be symlinks: a write would either fail (folders) or
/// have to replace a link a dotfile manager owns (files), and agent-switch refuses both.
fn symlink_checks(paths: &Paths) -> Vec<Check> {
    let home = paths.home();
    let candidates = [
        home.join(".claude.json"),
        home.join(".claude"),
        home.join(".claude/.credentials.json"),
        home.join(".codex"),
        home.join(".codex/auth.json"),
        home.join(".codex/config.toml"),
    ];
    candidates
        .iter()
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
        .map(|p| {
            let target = std::fs::read_link(p).map(|t| t.display().to_string()).unwrap_or_else(|_| "?".into());
            check(
                Level::Warn,
                "symlink",
                format!(
                    "{} is a symlink to {target} (a dotfile manager such as stow or chezmoi may own it). agent-switch will not replace a link with a regular file, so switching fails while it is a link. Make it a regular file, or exclude it from the manager",
                    p.display()
                ),
            )
        })
        .collect()
}

fn consistency_checks(config: &Config, state: &State) -> Vec<Check> {
    let mut out = Vec::new();
    for kind in SurfaceKind::ALL {
        let bound = config.surfaces.get(kind).account_id();
        let active = state.active(kind);
        if let Some(active) = active {
            if config.account(active).is_none() {
                out.push(check(Level::Warn, "state", format!("{kind}: active account `{active}` is not in the config")));
                continue;
            }
        }
        if bound.is_some() && bound != active {
            out.push(check(
                Level::Warn,
                "binding",
                format!("{kind}: bound to `{}` but active is {}; run `agent-switch switch {kind} {}`", bound.unwrap_or(""), active.map(|a| format!("`{a}`")).unwrap_or_else(|| "unknown".into()), bound.unwrap_or("")),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Account, Binding, Provider};
    use crate::keychain::MemoryKeychain;
    use crate::vault::Slot;
    use std::sync::Arc;

    fn setup() -> (tempfile::TempDir, Paths, Vault) {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        (d, paths, Vault::new(Arc::new(MemoryKeychain::new())))
    }

    #[test]
    fn reports_missing_slot_and_binding_drift() {
        let (_d, paths, vault) = setup();
        let mut c = Config::default();
        c.add_account(Account { id: "a".into(), provider: Provider::Claude, label: "A".into(), email: Some("zee@x.com".into()), uuid: None }).unwrap();
        c.bind(SurfaceKind::ClaudeCli, Binding::parse("a")).unwrap();
        c.save(&paths).unwrap();
        let checks = run(&DoctorInput { paths: &paths, vault: &vault });
        let vault_check = checks.iter().find(|c| c.name == "vault").unwrap();
        assert_eq!(vault_check.level, Level::Warn);
        assert!(vault_check.detail.contains("ze***@x.com") && !vault_check.detail.contains("zee@"), "{}", vault_check.detail);
        assert!(checks.iter().any(|c| c.name == "binding" && c.level == Level::Warn));

        vault.put("a", "claude-oauth", &Slot { blob: "{}".into(), fingerprint: "ff".into(), refreshed_at: 1 }).unwrap();
        let checks = run(&DoctorInput { paths: &paths, vault: &vault });
        assert_eq!(checks.iter().find(|c| c.name == "vault").unwrap().level, Level::Ok);
    }

    #[test]
    fn symlinked_dotfiles_and_quarantine_copies_are_reported() {
        let (d, paths, vault) = setup();
        let real = d.path().join("dotfiles");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("claude.json"), "{}").unwrap();
        std::os::unix::fs::symlink(real.join("claude.json"), d.path().join(".claude.json")).unwrap();
        State::update(&paths, |s| {
            s.quarantine.insert("quarantine-abcd".into(), 1);
        })
        .unwrap();
        let checks = run(&DoctorInput { paths: &paths, vault: &vault });
        let link = checks.iter().find(|c| c.name == "symlink").expect("a symlink check");
        assert_eq!(link.level, Level::Warn);
        assert!(link.detail.contains(".claude.json") && link.detail.contains("will not replace"), "{}", link.detail);
        let q = checks.iter().find(|c| c.name == "quarantine").expect("a quarantine check");
        assert!(q.detail.contains("quarantine-abcd") && q.detail.contains("remove quarantine-abcd"), "{}", q.detail);
    }

    #[test]
    fn invalid_config_fails() {
        let (_d, paths, vault) = setup();
        std::fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
        std::fs::write(paths.config_file(), "[surfaces]\nclaude-cli = \"ghost\"\n").unwrap();
        let checks = run(&DoctorInput { paths: &paths, vault: &vault });
        assert_eq!(checks.iter().find(|c| c.name == "config").unwrap().level, Level::Fail);
    }

    #[test]
    fn empty_setup_is_healthy_with_skips() {
        let (_d, paths, vault) = setup();
        let checks = run(&DoctorInput { paths: &paths, vault: &vault });
        assert!(checks.iter().all(|c| matches!(c.level, Level::Ok | Level::Skip)), "{checks:?}");
    }
}
