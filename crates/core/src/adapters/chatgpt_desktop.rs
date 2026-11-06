//! The ChatGPT desktop app (`com.openai.codex`). It runs its bundled `codex app-server`, which always uses
//! `~/.codex` (a Dock launch cannot carry environment variables), so switching the app means replacing
//! `~/.codex/auth.json` while nothing uses it, then reopening the app.
//!
//! Order (the app-server rewrites `auth.json` when it refreshes, so a write made before it exits could be
//! overwritten or could overwrite a rotated token):
//! 1. quit the app and confirm by PID that it and every `codex app-server` on `~/.codex` are gone,
//! 2. save back the live login (it may have refreshed on the way out),
//! 3. write the target login under Codex's `auth.lock`,
//! 4. open the app again (`restart`).
//!
//! `apply` never quits anything unless the caller built the adapter with `with_quit`, which is the
//! user's consent to quit the app.

use std::cell::Cell;

use crate::adapters::codex::{
    check_pin, cli_uses_shallow, codex_processes_in, find_account, identity, openai_account, pin_file_store,
    read_copy, read_live, save_back, vault_copy, write_auth, CodexProc, Homes,
};
use crate::config::{Account, Config, SurfaceKind};
use crate::error::{Error, Result};
use crate::macos_apps::{bundle_pids, join_pids, quit_and_wait, QuitOpts, System, SystemApps, CHATGPT};
use crate::shallow_home;
use crate::surface::{Env, Identity, Staged, Surface};
use crate::vault::Slot;

/// Decides whether processes the app does not own (for example an IDE extension's `codex app-server`)
/// may be sent SIGTERM. The adapter never terminates such a process without a `true` answer.
pub type ConfirmOthers = Box<dyn Fn(&[CodexProc]) -> bool>;

pub struct ChatgptDesktop {
    sys: Box<dyn System>,
    confirm_others: Option<ConfirmOthers>,
    opts: QuitOpts,
    /// Set when `apply` quit a running app, so `restart` opens it again (and a failed `apply` can too).
    quit_by_us: Cell<bool>,
}

impl ChatgptDesktop {
    pub fn production() -> ChatgptDesktop {
        Self::with_system(Box::new(SystemApps))
    }

    pub fn with_system(sys: Box<dyn System>) -> ChatgptDesktop {
        ChatgptDesktop { sys, confirm_others: None, opts: QuitOpts::default(), quit_by_us: Cell::new(false) }
    }

    /// The user agreed to quit the app. `confirm_others` answers for each batch of other processes.
    pub fn with_quit(mut self, confirm_others: ConfirmOthers) -> ChatgptDesktop {
        self.confirm_others = Some(confirm_others);
        self
    }

    pub fn with_quit_opts(mut self, opts: QuitOpts) -> ChatgptDesktop {
        self.opts = opts;
        self
    }

    /// Checks that need no lock and change nothing, run by `stage` (so a refusal costs nothing) and again
    /// by `apply` once the app has exited.
    fn preflight(&self, env: &Env, config: &Config, target: &Account) -> Result<()> {
        let homes = Homes::new(env.paths);
        check_pin(&homes.primary)?;
        let live = read_live(&homes)?;
        if let Some(p) = &live.primary {
            if find_account(config, &p.login).is_none() {
                return Err(Error::Invalid(
                    "~/.codex holds a login that agent-switch has not saved, and switching would overwrite it. \
                     Save it first: `agent-switch add openai --from-file ~/.codex/auth.json`"
                        .into(),
                ));
            }
        }
        if let Some(s) = &live.shallow {
            if s.login.is_account(target) && cli_uses_shallow(config) {
                return Err(Error::Invalid(format!(
                    "`{}` is also live in the Codex CLI's own home; two live copies of one login would invalidate each other. \
                     Bind codex-cli to `{}` as well, or to another account, first",
                    target.id, target.id
                )));
            }
        }
        Ok(())
    }

    /// Quit the app and wait for every user of `~/.codex` to exit. Returns whether the app was running.
    fn quit_phase(&self, homes: &Homes) -> Result<bool> {
        let procs = self.sys.processes()?;
        let app_running = !bundle_pids(&procs, &CHATGPT).is_empty();
        let users: Vec<CodexProc> = codex_processes_in(self.sys.as_ref(), &procs, homes).into_iter().filter(|p| p.home == homes.primary).collect();
        if !app_running && users.is_empty() {
            return Ok(false);
        }
        let Some(confirm) = &self.confirm_others else {
            let mut pids = bundle_pids(&procs, &CHATGPT);
            pids.extend(users.iter().map(|p| p.pid));
            pids.sort_unstable();
            pids.dedup();
            return Err(Error::Invalid(format!(
                "ChatGPT or a Codex process is using ~/.codex (pids {}). agent-switch writes the login only while none of them runs. \
                 Quit them, or run `agent-switch switch chatgpt-desktop <account> --restart` to let it quit the app",
                join_pids(&pids)
            )));
        };
        let others: Vec<CodexProc> = users.into_iter().filter(|p| !p.in_app).collect();
        if !others.is_empty() {
            if !confirm(&others) {
                let list: Vec<String> = others.iter().map(CodexProc::label).collect();
                return Err(Error::Invalid(format!("not switching: {} use ~/.codex and were not stopped", list.join(", "))));
            }
            for p in &others {
                self.sys.terminate(p.pid)?;
            }
        }
        let also = |procs: &[crate::macos_apps::ProcInfo]| -> Vec<u32> {
            codex_processes_in(self.sys.as_ref(), procs, homes).into_iter().filter(|p| p.home == homes.primary).map(|p| p.pid).collect()
        };
        quit_and_wait(self.sys.as_ref(), &CHATGPT, &also, &self.opts)?;
        Ok(app_running)
    }

    fn write_phase(&self, env: &Env, config: &Config, target: &Account, held: &[String]) -> Result<()> {
        let homes = Homes::new(env.paths);
        self.preflight(env, config, target)?;
        pin_file_store(env.paths, &homes.primary)?;

        let live = read_live(&homes)?;
        let copies: Vec<_> = live.primary.iter().chain(live.shallow.iter()).collect();
        let held: Vec<&str> = held.iter().map(String::as_str).collect();
        save_back(env, config, &held, &copies)?;

        // Re-read after the save-back: the vault now holds the newest copy of the target from any location.
        let fresh = vault_copy(env.vault, target)?;
        if !live.primary.as_ref().is_some_and(|p| crate::vault::normalize_blob(&p.text).is_ok_and(|n| n == fresh.text)) {
            write_auth(&homes.primary, &fresh.text)?;
        }
        // The CLI bound to this account too: its own copy would now be a second live token chain.
        if live.shallow.as_ref().is_some_and(|s| s.login.is_account(target)) {
            shallow_home::join_auth(env.paths)?;
        }
        Ok(())
    }
}

impl Surface for ChatgptDesktop {
    fn kind(&self) -> SurfaceKind {
        SurfaceKind::ChatgptDesktop
    }

    fn current_identity(&self, env: &Env) -> Result<Option<Identity>> {
        let config = Config::load(env.paths).ok();
        let live = read_copy(&Homes::new(env.paths).primary, "~/.codex")?;
        Ok(live.map(|c| identity(&c, config.as_ref(), None)))
    }

    fn capture(&self, env: &Env, _account: &Account) -> Result<Option<Slot>> {
        read_copy(&Homes::new(env.paths).primary, "~/.codex")?.map(|c| c.slot()).transpose()
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
        let homes = Homes::new(env.paths);

        let was_running = self.quit_phase(&homes)?;
        self.quit_by_us.set(was_running);
        let result = self.write_phase(env, &config, &target, &staged.held);
        if result.is_err() && was_running {
            // The app is closed and the login is unchanged: bring the app back on the old login.
            let _ = self.sys.open(&CHATGPT);
            self.quit_by_us.set(false);
        }
        result
    }

    fn is_running(&self, _env: &Env) -> Result<Option<bool>> {
        Ok(Some(!bundle_pids(&self.sys.processes()?, &CHATGPT).is_empty()))
    }

    fn restart(&self, _env: &Env) -> Result<()> {
        if self.quit_by_us.replace(false) {
            self.sys.open(&CHATGPT)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::codex::tests::Fixture;
    use crate::adapters::codex::{read_copy, Copy};
    use crate::config::Binding;
    use crate::macos_apps::fake::FakeSystem;
    use crate::providers::openai::tests::auth_json;
    use crate::state::State;
    use crate::switch;
    use std::cell::RefCell;
    use std::rc::Rc;

    const MAIN: &str = "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT";
    const APP_SERVER: &str = "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex app-server";
    const T0: &str = "2026-10-27T07:32:17.000000Z";
    const T1: &str = "2026-11-06T03:04:59.000000Z";

    fn quick() -> QuitOpts {
        QuitOpts { timeout: std::time::Duration::from_millis(150), poll: std::time::Duration::from_millis(10) }
    }

    /// Team and personal are saved; `~/.codex` holds team; both are bound to the app/CLI as given.
    fn setup() -> (Fixture, Config, String, String) {
        let f = Fixture::new();
        let team = f.account("team", "zee@work.com", "acct-team", "rt-team", T0);
        let personal = f.account("personal", "me@home.com", "acct-me", "rt-me", T0);
        write_auth(&f.homes.primary, &team).unwrap();
        let mut cfg = Config::load(&f.paths).unwrap();
        cfg.bind(SurfaceKind::ChatgptDesktop, Binding::parse("team")).unwrap();
        cfg.save(&f.paths).unwrap();
        (f, cfg, team, personal)
    }

    fn adapter(sys: &Rc<FakeSystem>, quit: bool) -> ChatgptDesktop {
        let a = ChatgptDesktop::with_system(Box::new(sys.clone())).with_quit_opts(quick());
        if quit {
            a.with_quit(Box::new(|_| false))
        } else {
            a
        }
    }

    fn live_account(f: &Fixture, cfg: &Config) -> Option<String> {
        let c = read_copy(&f.homes.primary, "x").unwrap()?;
        find_account(cfg, &c.login).map(|a| a.id.clone())
    }

    #[test]
    fn capture_and_current_identity_read_the_live_login() {
        let (f, _cfg, team, _) = setup();
        let a = adapter(&Rc::new(FakeSystem::new(true)), false);
        let id = a.current_identity(&f.env()).unwrap().unwrap();
        assert_eq!((id.account_id.as_deref(), id.email.as_deref()), (Some("team"), Some("zee@work.com")));
        let slot = a.capture(&f.env(), &Account { id: "team".into(), provider: crate::config::Provider::Openai, label: "t".into(), email: None, uuid: None }).unwrap().unwrap();
        assert_eq!(Copy::parse("c", slot.blob).unwrap().login.account_id, "acct-team");
        let _ = team;
    }

    #[test]
    fn activate_switches_the_login_when_the_app_is_not_running() {
        let (f, cfg, _team, personal) = setup();
        let sys = Rc::new(FakeSystem::new(true));
        let a = adapter(&sys, false);
        switch::activate(&f.env(), &cfg, &a, "personal").unwrap();
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().login.fingerprint, Copy::parse("p", personal).unwrap().login.fingerprint);
        assert_eq!(State::load(&f.paths).unwrap().active(SurfaceKind::ChatgptDesktop), Some("personal"));
        assert!(sys.opened.borrow().is_empty(), "nothing was quit, so nothing is reopened");
        // The file store is pinned and the previous config was backed up first.
        let cfg_text = std::fs::read_to_string(f.homes.primary.join("config.toml")).unwrap();
        assert!(cfg_text.starts_with("cli_auth_credentials_store = \"file\""));
        assert!(f.paths.support_dir().join("backups").is_dir());
    }

    #[test]
    fn a_running_app_is_never_quit_without_consent_and_nothing_is_written() {
        let (f, cfg, team, _) = setup();
        let sys = Rc::new(FakeSystem::new(true));
        sys.add(10, MAIN);
        let a = adapter(&sys, false);
        let err = switch::activate(&f.env(), &cfg, &a, "personal").unwrap_err().to_string();
        assert!(err.contains("10") && err.contains("--restart"), "{err}");
        assert_eq!(*sys.quit_calls.borrow(), 0);
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, team);
        assert!(State::load(&f.paths).unwrap().active(SurfaceKind::ChatgptDesktop).is_none());
    }

    #[test]
    fn with_consent_the_app_quits_first_then_the_login_is_saved_back_written_and_the_app_reopens() {
        let (f, cfg, _team, personal) = setup();
        // While the app ran it refreshed the team login: ~/.codex now holds a newer rotated copy.
        let rotated = auth_json("zee@work.com", "acct-team", "team", "rt-team-rotated", T1);
        write_auth(&f.homes.primary, &rotated).unwrap();

        let sys = Rc::new(FakeSystem::new(true));
        sys.add(10, MAIN);
        sys.add(11, APP_SERVER);
        let a = adapter(&sys, true);
        switch::activate(&f.env(), &cfg, &a, "personal").unwrap();
        a.restart(&f.env()).unwrap();

        assert_eq!(*sys.quit_calls.borrow(), 1);
        assert_eq!(*sys.opened.borrow(), vec!["ChatGPT"]);
        assert_eq!(live_account(&f, &cfg).as_deref(), Some("personal"));
        let saved = vault_copy(&f.vault, cfg.account("team").unwrap()).unwrap();
        assert_eq!(saved.text, crate::vault::normalize_blob(&rotated).unwrap(), "the rotated team login was saved back before the overwrite");
        let _ = personal;
    }

    #[test]
    fn a_refusing_app_stops_the_switch_before_any_write() {
        let (f, cfg, team, _) = setup();
        let sys = Rc::new(FakeSystem::new(false));
        sys.add(10, MAIN);
        let a = adapter(&sys, true);
        let err = switch::activate(&f.env(), &cfg, &a, "personal").unwrap_err().to_string();
        assert!(err.contains("did not quit"), "{err}");
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, team);
    }

    #[test]
    fn other_codex_servers_on_the_home_need_a_yes_before_sigterm() {
        let (f, cfg, team, _) = setup();
        let sys = Rc::new(FakeSystem::new(true));
        sys.add(10, MAIN);
        sys.add(50, "/Users/me/.vscode/codex app-server");
        let asked = Rc::new(RefCell::new(Vec::new()));
        let log = asked.clone();
        let no = ChatgptDesktop::with_system(Box::new(sys.clone())).with_quit_opts(quick()).with_quit(Box::new(move |p| {
            log.borrow_mut().extend(p.iter().map(|x| x.pid));
            false
        }));
        let err = switch::activate(&f.env(), &cfg, &no, "personal").unwrap_err().to_string();
        assert!(err.contains("pid 50") && err.contains("not stopped"), "{err}");
        assert_eq!(*asked.borrow(), vec![50]);
        assert!(sys.terminated.borrow().is_empty() && *sys.quit_calls.borrow() == 0);
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, team);

        let yes = ChatgptDesktop::with_system(Box::new(sys.clone())).with_quit_opts(quick()).with_quit(Box::new(|_| true));
        switch::activate(&f.env(), &cfg, &yes, "personal").unwrap();
        assert_eq!(*sys.terminated.borrow(), vec![50]);
        assert_eq!(live_account(&f, &cfg).as_deref(), Some("personal"));
    }

    #[test]
    fn an_unknown_live_login_is_never_overwritten() {
        let (f, cfg, _team, _) = setup();
        let stranger = auth_json("who@else.com", "acct-x", "plus", "rt-x", T1);
        write_auth(&f.homes.primary, &stranger).unwrap();
        let a = adapter(&Rc::new(FakeSystem::new(true)), false);
        let err = switch::activate(&f.env(), &cfg, &a, "personal").unwrap_err().to_string();
        assert!(err.contains("has not saved"), "{err}");
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, stranger);
    }

    #[test]
    fn a_failed_write_reopens_the_app_on_the_old_login() {
        let (f, cfg, team, _) = setup();
        // `auth.lock` cannot be opened as a lock file, so the write fails after the app has quit.
        std::fs::remove_file(f.homes.primary.join("auth.lock")).unwrap();
        std::fs::create_dir(f.homes.primary.join("auth.lock")).unwrap();
        let sys = Rc::new(FakeSystem::new(true));
        sys.add(10, MAIN);
        let a = adapter(&sys, true);
        assert!(switch::activate(&f.env(), &cfg, &a, "personal").is_err());
        assert_eq!(*sys.quit_calls.borrow(), 1);
        assert_eq!(*sys.opened.borrow(), vec!["ChatGPT"], "the app is brought back");
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, team);
    }

    #[test]
    fn a_keyring_setting_is_refused_before_the_app_is_quit() {
        let (f, cfg, team, _) = setup();
        std::fs::write(f.homes.primary.join("config.toml"), "cli_auth_credentials_store = \"keyring\"\n").unwrap();
        let sys = Rc::new(FakeSystem::new(true));
        sys.add(10, MAIN);
        let a = adapter(&sys, true);
        let err = switch::activate(&f.env(), &cfg, &a, "personal").unwrap_err().to_string();
        assert!(err.contains("keyring"), "{err}");
        assert_eq!(*sys.quit_calls.borrow(), 0);
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, team);
    }

    #[test]
    fn the_app_follows_the_binding_and_the_shallow_copy_of_the_same_account_is_rejoined() {
        let (f, mut cfg, _team, _personal) = setup();
        // The CLI holds team in the shallow home and is bound to team as well: bound together.
        shallow_home::ensure(&f.paths).unwrap();
        let newer_team = auth_json("zee@work.com", "acct-team", "team", "rt-team-newer", T1);
        write_auth(&f.homes.shallow, &newer_team).unwrap();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("team")).unwrap();
        cfg.save(&f.paths).unwrap();
        // Switch the app to personal and back to team: the newest team copy (from the shallow home) must win.
        let a = adapter(&Rc::new(FakeSystem::new(true)), false);
        switch::activate(&f.env(), &cfg, &a, "personal").unwrap();
        switch::activate(&f.env(), &cfg, &a, "team").unwrap();
        let live = read_copy(&f.homes.primary, "x").unwrap().unwrap();
        assert_eq!(live.text, crate::vault::normalize_blob(&newer_team).unwrap());
        let link = std::fs::read_link(f.homes.shallow.join("auth.json")).unwrap();
        assert_eq!(link, f.homes.primary.join("auth.json"), "one live copy of team remains");
    }

    #[test]
    fn a_split_cli_rejoins_with_its_freshest_login_and_leaves_one_live_copy() {
        use crate::state::{Pending, REASON_REJOIN};
        let (f, mut cfg, team, _personal) = setup();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("team")).unwrap();
        cfg.save(&f.paths).unwrap();
        let shell = || crate::adapters::codex::shell_init(&f.paths, &cfg);
        assert_eq!(shell(), "", "bound together there is nothing to export");

        // The watcher records the split, then moves the CLI to personal in its own home.
        State::update(&f.paths, |s| {
            s.set_pending(SurfaceKind::CodexCli, Some(Pending { id: "r".into(), target_account: "personal".into(), reason: REASON_REJOIN.into(), from: Some("team".into()), created_at: 1, expires_at: None }))
        })
        .unwrap();
        switch::activate(&f.env(), &cfg, &crate::adapters::codex_cli::CodexCli, "personal").unwrap();
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, team, "the app's login is untouched");
        assert_eq!(read_copy(&f.homes.shallow, "x").unwrap().unwrap().login.account_id, "acct-me");
        assert!(shell().contains("export CODEX_HOME="), "shells opened while split use the CLI's own home");
        assert!(State::load(&f.paths).unwrap().codex_split(), "the split survives a restart: it lives in state.json");

        // The CLI refreshed personal while it ran.
        let rotated = auth_json("me@home.com", "acct-me", "team", "rt-me-rotated", T1);
        write_auth(&f.homes.shallow, &rotated).unwrap();

        // The click: the app moves to personal.
        switch::activate(&f.env(), &cfg, &adapter(&Rc::new(FakeSystem::new(true)), false), "personal").unwrap();
        let st = State::load(&f.paths).unwrap();
        assert!(!st.codex_split(), "the rejoin record is cleared once the app reaches the CLI's account");
        let live = read_copy(&f.homes.primary, "x").unwrap().unwrap();
        assert_eq!(live.text, crate::vault::normalize_blob(&rotated).unwrap(), "the freshest copy wins");
        assert_eq!(std::fs::read_link(f.homes.shallow.join("auth.json")).unwrap(), f.homes.primary.join("auth.json"), "one token chain on disk");
        assert_eq!(crate::adapters::codex::vault_copy(&f.vault, cfg.account("personal").unwrap()).unwrap().text, live.text, "the vault holds it too");
        assert_eq!(shell(), "");
        assert_eq!(st.active(SurfaceKind::ChatgptDesktop), Some("personal"));
    }

    #[test]
    fn a_second_live_copy_in_the_cli_home_blocks_the_switch_when_the_cli_is_bound_apart() {
        let (f, mut cfg, _team, personal) = setup();
        shallow_home::ensure(&f.paths).unwrap();
        write_auth(&f.homes.shallow, &personal).unwrap();
        cfg.bind(SurfaceKind::CodexCli, Binding::parse("personal")).unwrap();
        cfg.save(&f.paths).unwrap();
        let a = adapter(&Rc::new(FakeSystem::new(true)), false);
        let err = switch::activate(&f.env(), &cfg, &a, "personal").unwrap_err().to_string();
        assert!(err.contains("two live copies"), "{err}");
    }

    #[test]
    fn sessions_and_databases_are_not_touched_by_a_switch() {
        let (f, cfg, _team, _) = setup();
        std::fs::create_dir_all(f.homes.primary.join("sessions/2026")).unwrap();
        std::fs::write(f.homes.primary.join("sessions/2026/a.jsonl"), "line\n").unwrap();
        std::fs::write(f.homes.primary.join("state_5.sqlite"), "db").unwrap();
        let a = adapter(&Rc::new(FakeSystem::new(true)), false);
        switch::activate(&f.env(), &cfg, &a, "personal").unwrap();
        assert_eq!(std::fs::read_to_string(f.homes.primary.join("sessions/2026/a.jsonl")).unwrap(), "line\n");
        assert_eq!(std::fs::read_to_string(f.homes.primary.join("state_5.sqlite")).unwrap(), "db");
    }

    #[test]
    fn re_activating_the_live_account_leaves_the_file_byte_for_byte_alone() {
        let (f, cfg, team, _) = setup();
        // Same login, different formatting (Codex writes pretty JSON).
        let pretty = serde_json::to_string_pretty(&serde_json::from_str::<serde_json::Value>(&team).unwrap()).unwrap();
        write_auth(&f.homes.primary, &pretty).unwrap();
        switch::activate(&f.env(), &cfg, &adapter(&Rc::new(FakeSystem::new(true)), false), "team").unwrap();
        assert_eq!(std::fs::read_to_string(f.homes.primary.join("auth.json")).unwrap(), pretty);
    }
}
