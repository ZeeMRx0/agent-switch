//! The Codex CLI. It reads its login from `$CODEX_HOME` (default `~/.codex`).
//!
//! * Bound to the same account as the ChatGPT app, it uses `~/.codex` directly, so there is nothing to
//!   write. Activation only checks that `~/.codex` holds that account, and turns a leftover separate
//!   `auth.json` of the shallow home into a link to the shared one so a shell that still exports the old
//!   `CODEX_HOME` does not keep a second copy of the same token chain.
//! * Bound to a different account (or with the app unmanaged), it uses the shallow home
//!   (`~/.agent-switch/codex-cli`): `activate` writes that home's own `auth.json`. A running `codex`
//!   process keeps the login it started with; the new account applies to sessions started afterwards
//!   (checked 2026-11-06: swapping `auth.json` under a running `codex exec` did not disturb it, and
//!   the process did not write the old login back).

use crate::adapters::codex::{
    check_pin, cli_separate, find_account, identity, openai_account, pin_file_store, read_copy, read_live, save_back,
    vault_copy, write_shallow_auth, Homes,
};
use crate::config::{Account, Config, SurfaceKind};
use crate::error::{Error, Result};
use crate::shallow_home;
use crate::surface::{Env, Identity, Staged, Surface};
use crate::vault::Slot;

pub struct CodexCli;

impl CodexCli {
    /// Read-only checks, run by `stage` and again by `apply`.
    fn preflight(&self, env: &Env, config: &Config, target: &Account) -> Result<()> {
        let homes = Homes::new(env.paths);
        let live = read_live(&homes)?;
        if cli_separate(env.paths, config) {
            check_pin(&homes.primary)?;
            if live.primary.as_ref().is_some_and(|p| p.login.is_account(target)) {
                return Err(Error::Invalid(format!(
                    "`{}` is live in ~/.codex (the ChatGPT app); two live copies of one login would invalidate each other. \
                     Bind both surfaces to `{}`, or choose another account for the CLI",
                    target.id, target.id
                )));
            }
            if let Some(s) = &live.shallow {
                if find_account(config, &s.login).is_none() {
                    return Err(Error::Invalid(
                        "the CLI's own home holds a login that agent-switch has not saved, and switching would overwrite it. \
                         Save it first: `agent-switch add openai --from-file ~/.agent-switch/codex-cli/auth.json`"
                            .into(),
                    ));
                }
            }
            return Ok(());
        }
        // Shared home: the app's account must be the target, and it must already be live.
        let app = config.surfaces.chatgpt_desktop.account_id();
        if app != Some(target.id.as_str()) {
            return Err(Error::Invalid(format!(
                "codex-cli shares ~/.codex with chatgpt-desktop (bound to `{}`), so it can only use that account. \
                 Switch chatgpt-desktop, or bind codex-cli to a different account",
                app.unwrap_or("nothing")
            )));
        }
        if !live.primary.as_ref().is_some_and(|p| p.login.is_account(target)) {
            return Err(Error::Invalid(format!(
                "~/.codex does not hold `{}` yet; run `agent-switch switch chatgpt-desktop {} --restart` first",
                target.id, target.id
            )));
        }
        if let Some(s) = &live.shallow {
            if find_account(config, &s.login).is_none() {
                return Err(Error::Invalid("the CLI's own home holds a login that agent-switch has not saved; save it first with `agent-switch add openai --from-file`".into()));
            }
        }
        Ok(())
    }
}

impl Surface for CodexCli {
    fn kind(&self) -> SurfaceKind {
        SurfaceKind::CodexCli
    }

    fn current_identity(&self, env: &Env) -> Result<Option<Identity>> {
        let config = Config::load(env.paths).ok();
        let homes = Homes::new(env.paths);
        let separate = config.as_ref().is_some_and(|c| cli_separate(env.paths, c));
        let (dir, note) = if separate { (&homes.shallow, Some("own home")) } else { (&homes.primary, None) };
        Ok(read_copy(dir, "the Codex CLI home")?.map(|c| identity(&c, config.as_ref(), note)))
    }

    fn capture(&self, env: &Env, _account: &Account) -> Result<Option<Slot>> {
        let config = Config::load(env.paths).ok();
        let homes = Homes::new(env.paths);
        let dir = if config.as_ref().is_some_and(|c| cli_separate(env.paths, c)) { &homes.shallow } else { &homes.primary };
        read_copy(dir, "the Codex CLI home")?.map(|c| c.slot()).transpose()
    }

    fn stage(&self, env: &Env, account: &Account) -> Result<Staged> {
        let config = Config::load(env.paths)?;
        let target = openai_account(&config, &account.id)?;
        let copy = vault_copy(env.vault, target)?;
        self.preflight(env, &config, target)?;
        Ok(Staged::new(target.id.clone(), copy.text.into_bytes()))
    }

    fn apply(&self, env: &Env, staged: Staged) -> Result<()> {
        let config = Config::load(env.paths)?;
        let target = openai_account(&config, &staged.account_id)?.clone();
        self.preflight(env, &config, &target)?;
        let homes = Homes::new(env.paths);
        let held: Vec<&str> = staged.held.iter().map(String::as_str).collect();

        let live = read_live(&homes)?;
        let copies: Vec<_> = live.primary.iter().chain(live.shallow.iter()).collect();
        save_back(env, &config, &held, &copies)?;

        if cli_separate(env.paths, &config) {
            shallow_home::ensure(env.paths)?;
            pin_file_store(env.paths, &homes.primary)?;
            // Read after the save-back, so the newest copy of the target from any location is used.
            let fresh = vault_copy(env.vault, &target)?;
            if !live.shallow.as_ref().is_some_and(|s| crate::vault::normalize_blob(&s.text).is_ok_and(|n| n == fresh.text)) {
                write_shallow_auth(&homes.shallow, &fresh.text)?;
            }
        } else if homes.shallow.is_dir() {
            shallow_home::join_auth(env.paths)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::codex::tests::Fixture;
    use crate::adapters::codex::write_auth;
    use crate::adapters::codex::Copy;
    use crate::config::Binding;
    use crate::providers::openai::tests::auth_json;
    use crate::state::State;
    use crate::switch;
    use crate::vault::normalize_blob;

    const T0: &str = "2026-10-27T07:32:17.000000Z";
    const T1: &str = "2026-11-06T03:04:59.000000Z";

    /// App on team (live in ~/.codex), CLI bound to personal (its own home).
    fn split() -> (Fixture, Config, String, String) {
        let f = Fixture::new();
        let team = f.account("team", "zee@work.com", "acct-team", "rt-team", T0);
        let personal = f.account("personal", "me@home.com", "acct-me", "rt-me", T0);
        write_auth(&f.homes.primary, &team).unwrap();
        let mut cfg = Config::load(&f.paths).unwrap();
        cfg.bind(SurfaceKind::ChatgptDesktop, Binding::parse("team")).unwrap();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("personal")).unwrap();
        cfg.save(&f.paths).unwrap();
        (f, cfg, team, personal)
    }

    #[test]
    fn a_separate_binding_writes_the_shallow_home_and_leaves_the_app_login_alone() {
        let (f, cfg, team, personal) = split();
        std::fs::create_dir_all(f.homes.primary.join("sessions")).unwrap();
        std::fs::write(f.homes.primary.join("state_5.sqlite"), "db").unwrap();

        switch::activate(&f.env(), &cfg, &CodexCli, "personal").unwrap();

        let shallow = read_copy(&f.homes.shallow, "x").unwrap().unwrap();
        assert_eq!(shallow.text, normalize_blob(&personal).unwrap());
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, team);
        assert_eq!(std::fs::read_link(f.homes.shallow.join("sessions")).unwrap(), f.homes.primary.join("sessions"));
        assert!(!f.homes.shallow.join("ipc").exists() && !f.homes.shallow.join("auth.lock").is_symlink());
        assert_eq!(std::fs::read_to_string(f.homes.primary.join("state_5.sqlite")).unwrap(), "db");
        assert_eq!(State::load(&f.paths).unwrap().active(SurfaceKind::CodexCli), Some("personal"));
        let id = CodexCli.current_identity(&f.env()).unwrap().unwrap();
        assert_eq!((id.account_id.as_deref(), id.note.as_deref()), (Some("personal"), Some("own home")));
        assert!(std::fs::read_to_string(f.homes.primary.join("config.toml")).unwrap().starts_with("cli_auth_credentials_store"));
    }

    #[test]
    fn the_rotated_login_in_the_shallow_home_is_saved_back_before_it_is_replaced() {
        let (f, mut cfg, _team, _personal) = split();
        switch::activate(&f.env(), &cfg, &CodexCli, "personal").unwrap();
        // The CLI refreshed its login while it ran.
        let rotated = auth_json("me@home.com", "acct-me", "team", "rt-me-rotated", T1);
        write_auth(&f.homes.shallow, &rotated).unwrap();
        // The user rebinds the CLI to another account that is not live anywhere.
        let other = auth_json("o@x.com", "acct-o", "plus", "rt-o", T0);
        crate::adapters::codex::import(&f.env(), vec![Copy::parse("o", other).unwrap()], Some("other"), None, false).unwrap();
        cfg = Config::load(&f.paths).unwrap();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("other")).unwrap();
        cfg.save(&f.paths).unwrap();

        switch::activate(&f.env(), &cfg, &CodexCli, "other").unwrap();
        let saved = vault_copy(&f.vault, cfg.account("personal").unwrap()).unwrap();
        assert_eq!(saved.text, normalize_blob(&rotated).unwrap());
        assert_eq!(read_copy(&f.homes.shallow, "x").unwrap().unwrap().login.account_id, "acct-o");
    }

    #[test]
    fn an_account_live_in_the_app_home_cannot_also_be_live_in_the_cli_home() {
        let (f, cfg, team, _) = split();
        // One-off switch of the CLI to the account the app is using.
        let mut c = cfg.clone();
        c.bind(SurfaceKind::CodexCli, Binding::parse("personal")).unwrap();
        let err = switch::activate(&f.env(), &c, &CodexCli, "team").unwrap_err().to_string();
        assert!(err.contains("two live copies"), "{err}");
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, team);
        assert!(read_copy(&f.homes.shallow, "x").unwrap().is_none());
    }

    #[test]
    fn bound_together_the_cli_uses_the_shared_home_and_the_old_shallow_copy_becomes_a_link() {
        let (f, mut cfg, team, _) = split();
        switch::activate(&f.env(), &cfg, &CodexCli, "personal").unwrap();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("team")).unwrap();
        cfg.save(&f.paths).unwrap();

        switch::activate(&f.env(), &cfg, &CodexCli, "team").unwrap();
        assert_eq!(std::fs::read_link(f.homes.shallow.join("auth.json")).unwrap(), f.homes.primary.join("auth.json"));
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, team);
        // The personal login that lived in the shallow home was saved, not lost.
        assert!(vault_copy(&f.vault, cfg.account("personal").unwrap()).is_ok());
        let id = CodexCli.current_identity(&f.env()).unwrap().unwrap();
        assert_eq!((id.account_id.as_deref(), id.note), (Some("team"), None));
    }

    #[test]
    fn bound_together_needs_the_app_to_hold_the_account_first() {
        let (f, mut cfg, _team, _) = split();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("team")).unwrap();
        cfg.bind(SurfaceKind::ChatgptDesktop, Binding::parse("personal")).unwrap();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("personal")).unwrap();
        cfg.save(&f.paths).unwrap();
        let err = switch::activate(&f.env(), &cfg, &CodexCli, "personal").unwrap_err().to_string();
        assert!(err.contains("switch chatgpt-desktop personal --restart"), "{err}");

        // A one-off to another account while sharing is refused rather than clobbering the app's login.
        let err = switch::activate(&f.env(), &cfg, &CodexCli, "team").unwrap_err().to_string();
        assert!(err.contains("shares ~/.codex"), "{err}");
    }

    #[test]
    fn an_unknown_login_in_the_cli_home_is_never_overwritten() {
        let (f, cfg, _team, _) = split();
        shallow_home::ensure(&f.paths).unwrap();
        let stranger = auth_json("who@else.com", "acct-x", "plus", "rt-x", T1);
        write_auth(&f.homes.shallow, &stranger).unwrap();
        let err = switch::activate(&f.env(), &cfg, &CodexCli, "personal").unwrap_err().to_string();
        assert!(err.contains("has not saved"), "{err}");
        assert_eq!(read_copy(&f.homes.shallow, "x").unwrap().unwrap().text, stranger);
    }

    #[test]
    fn switching_back_restores_the_previous_cli_account_from_its_current_slot() {
        let (f, _cfg, _team, _) = split();
        crate::adapters::codex::import(
            &f.env(),
            vec![Copy::parse("o", auth_json("o@x.com", "acct-o", "plus", "rt-o", T0)).unwrap()],
            Some("other"),
            None,
            false,
        )
        .unwrap();
        let mut cfg = Config::load(&f.paths).unwrap();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("personal")).unwrap();
        switch::activate(&f.env(), &cfg, &CodexCli, "personal").unwrap();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("other")).unwrap();
        switch::activate(&f.env(), &cfg, &CodexCli, "other").unwrap();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("personal")).unwrap();
        switch::activate(&f.env(), &cfg, &CodexCli, "personal").unwrap();
        assert_eq!(read_copy(&f.homes.shallow, "x").unwrap().unwrap().login.account_id, "acct-me");
    }
}
