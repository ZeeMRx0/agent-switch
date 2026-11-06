use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::rc::Rc;

use super::manage::*;
use super::*;
use crate::apfs_clone::{tree_stats, BUNDLE};
use crate::config::{Binding, Provider};
use crate::keychain::MemoryKeychain;
use crate::macos_apps::fake::FakeSystem;
use crate::state::State;
use crate::switch;
use crate::vault::Vault;

const U_PERSONAL: &str = "11111111-aaaa-4aaa-8aaa-111111111111";
const U_TEAM: &str = "22222222-bbbb-4bbb-8bbb-222222222222";
const MAIN: &str = "/Applications/Claude.app/Contents/MacOS/Claude";
const COOKIE_VALUE: &str = "sk-ant-sid01-THE-COOKIE-VALUE";

fn quick() -> QuitOpts {
    QuitOpts { timeout: Duration::from_millis(300), poll: Duration::from_millis(5) }
}

fn reask() -> Duration {
    Duration::from_millis(15)
}

fn future() -> i64 {
    now_secs() as i64 + 86_400
}

fn past() -> i64 {
    now_secs() as i64 - 86_400
}

/// A Chromium profile: `config.json` with a uuid (and a token cache that must never be read out) and, when
/// `cookie_expiry` is given, a `Cookies` database with a claude.ai `sessionKey` row.
fn make_profile(dir: &Path, uuid: Option<&str>, cookie_expiry: Option<i64>) {
    fs::create_dir_all(dir).unwrap();
    let mut cfg = serde_json::json!({"oauth:tokenCache": "TOKEN-CACHE-SECRET"});
    if let Some(u) = uuid {
        cfg["lastKnownAccountUuid"] = u.into();
    }
    fs::write(dir.join("config.json"), cfg.to_string()).unwrap();
    if let Some(expiry) = cookie_expiry {
        let db = dir.join("Cookies");
        fs::remove_file(&db).ok();
        let micros = (expiry + 11_644_473_600) * 1_000_000;
        let sql = format!(
            "create table cookies(host_key text, name text, value text, expires_utc integer, has_expires integer);\
             insert into cookies values('.claude.ai','sessionKey','{COOKIE_VALUE}',{micros},1);\
             insert into cookies values('.claude.ai','other','x',{micros},1);"
        );
        let out = Command::new("/usr/bin/sqlite3").arg(&db).arg(sql).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
}

struct Fx {
    _home: tempfile::TempDir,
    paths: Paths,
    vault: Vault,
}

impl Fx {
    fn new() -> Fx {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(fs::canonicalize(home.path()).unwrap());
        let mut config = Config::default();
        for (id, uuid) in [("personal", U_PERSONAL), ("team", U_TEAM)] {
            config
                .add_account(Account { id: id.into(), provider: Provider::Claude, label: id.into(), email: Some(format!("{id}@x.com")), uuid: Some(uuid.into()) })
                .unwrap();
        }
        config.save(&paths).unwrap();
        Fx { _home: home, vault: Vault::new(Arc::new(MemoryKeychain::new())), paths }
    }

    fn env(&self) -> Env<'_> {
        Env { paths: &self.paths, vault: &self.vault }
    }

    fn default(&self) -> PathBuf {
        default_profile(&self.paths)
    }

    fn root(&self) -> PathBuf {
        profiles_root(&self.paths)
    }

    /// The app's own profile holding the personal account, with a runtime and an MCP config.
    fn default_with_runtime(&self) {
        let d = self.default();
        make_profile(&d, Some(U_PERSONAL), Some(future()));
        let bundle = d.join(BUNDLE);
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("rootfs.img"), vec![1u8; 2048]).unwrap();
        fs::write(bundle.join("machineIdentifier"), b"vm-identity").unwrap();
        fs::create_dir_all(d.join("claude-code/2.1.0")).unwrap();
        fs::write(d.join("claude-code/2.1.0/claude"), vec![2u8; 512]).unwrap();
        fs::create_dir_all(d.join("Cache")).unwrap();
        fs::write(d.join("Cache/blob"), vec![3u8; 1024]).unwrap();
        fs::write(
            d.join("claude_desktop_config.json"),
            r#"{"mcpServers":{"fs":{"command":"x","env":{"API_TOKEN":"tok-123","REGION":"eu"}},"plain":{"command":"y"}},"preferences":{"a":1}}"#,
        )
        .unwrap();
        fs::set_permissions(d.join("claude_desktop_config.json"), fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn adapter(&self, sys: &Rc<FakeSystem>) -> ClaudeDesktop {
        ClaudeDesktop::with_system(Box::new(sys.clone())).with_timing(quick(), reask(), Duration::from_millis(300), Duration::ZERO)
    }

    fn account(&self, id: &str) -> Account {
        Config::load(&self.paths).unwrap().account(id).unwrap().clone()
    }

    fn register(&self, id: &str, dir: &Path, protected: bool) {
        Registry::update(&self.paths, |r| {
            r.profiles.insert(id.into(), ProfileEntry { dir: dir.to_path_buf(), protected, from: None });
            Ok(())
        })
        .unwrap();
    }

    /// A managed team profile, signed in with a good session.
    fn team_profile(&self) -> PathBuf {
        let dir = self.root().join("team");
        make_profile(&dir, Some(U_TEAM), Some(future()));
        self.register("team", &dir, false);
        dir
    }
}

use std::sync::Arc;

fn req<'a>(account: &'a str, from: Option<&'a Path>, dry_run: bool) -> AddRequest<'a> {
    AddRequest { account, from_profile: from, dry_run }
}

// ---------------------------------------------------------------------------------------------
// Session cookie and identity
// ---------------------------------------------------------------------------------------------

#[test]
fn session_rows_are_judged_by_expiry() {
    let now = 1_800_000_000;
    let chromium = |unix: i64| ((unix + 11_644_473_600) * 1_000_000).to_string();
    assert_eq!(parse_session_rows(&format!("1|{}\n", chromium(now + 10)), now), Session::Valid);
    assert_eq!(parse_session_rows(&format!("1|{}\n", chromium(now - 10)), now), Session::Expired);
    assert_eq!(parse_session_rows("0|0\n", now), Session::Valid, "a session cookie has no expiry");
    assert_eq!(parse_session_rows("", now), Session::Missing);
    // The newest of several rows decides.
    assert_eq!(parse_session_rows(&format!("1|{}\n1|{}\n", chromium(now - 10), chromium(now + 10)), now), Session::Valid);
}

#[test]
fn the_cookie_database_is_read_by_name_and_expiry_only() {
    let d = tempfile::tempdir().unwrap();
    let now = now_secs() as i64;
    assert_eq!(session_state(d.path(), now), Session::Missing, "no Cookies file");
    make_profile(d.path(), Some(U_TEAM), Some(future()));
    let state = session_state(d.path(), now);
    assert_eq!(state, Session::Valid);
    assert!(!format!("{state:?} {}", state.describe()).contains(COOKIE_VALUE));

    let e = tempfile::tempdir().unwrap();
    make_profile(e.path(), Some(U_TEAM), Some(past()));
    assert_eq!(session_state(e.path(), now), Session::Expired);

    // Other cookies do not count as a login.
    let f = tempfile::tempdir().unwrap();
    make_profile(f.path(), Some(U_TEAM), Some(future()));
    Command::new("/usr/bin/sqlite3").arg(f.path().join("Cookies")).arg("delete from cookies where name = 'sessionKey'").output().unwrap();
    assert_eq!(session_state(f.path(), now), Session::Missing);
}

#[test]
fn identity_needs_a_matching_uuid_and_reports_the_session() {
    let fx = Fx::new();
    let personal = fx.account("personal");
    let d = fx.root().join("p");

    make_profile(&d, Some(U_PERSONAL), Some(future()));
    assert_eq!(check_profile(&personal, &d).unwrap(), Session::Valid);

    // A matching uuid with an expired web session is "signed out", not a success.
    make_profile(&d, Some(U_PERSONAL), None);
    fs::remove_file(d.join("Cookies")).ok();
    assert_eq!(check_profile(&personal, &d).unwrap(), Session::Missing);

    make_profile(&d, Some(U_TEAM), Some(future()));
    let err = check_profile(&personal, &d).unwrap_err().to_string();
    assert!(err.contains("another account") && err.contains("22222222"), "{err}");

    make_profile(&d, None, None);
    assert!(check_profile(&personal, &d).unwrap_err().to_string().contains("no signed-in account"));
    assert!(check_profile(&personal, &fx.root().join("missing")).is_err());

    let mut no_uuid = personal.clone();
    no_uuid.uuid = None;
    make_profile(&d, Some(U_PERSONAL), None);
    assert!(check_profile(&no_uuid, &d).unwrap_err().to_string().contains("no uuid recorded"));
}

// ---------------------------------------------------------------------------------------------
// add: new profile
// ---------------------------------------------------------------------------------------------

#[test]
fn a_new_profile_gets_the_config_as_is_the_runtime_and_a_sign_in_window() {
    let fx = Fx::new();
    fx.default_with_runtime();
    let sys = Rc::new(FakeSystem::new(true));
    let report = add_profile(&fx.env(), sys.as_ref(), &req("team", None, false)).unwrap();

    let dir = fx.root().join("team");
    assert_eq!(report.dir, dir);
    assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    // The MCP config is copied byte for byte at 0600; only key names are listed.
    let copied = fs::read(dir.join("claude_desktop_config.json")).unwrap();
    assert_eq!(copied, fs::read(fx.default().join("claude_desktop_config.json")).unwrap());
    assert_eq!(fs::metadata(dir.join("claude_desktop_config.json")).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(report.config_copied);
    assert_eq!(report.mcp_env_keys, vec!["fs.API_TOKEN", "fs.REGION"]);
    assert!(!format!("{report:?}").contains("tok-123"));
    // Runtime: images and versions travel; identity files and caches do not.
    let rt = report.runtime.unwrap();
    assert_eq!(rt.cloned, 2);
    assert!(dir.join(BUNDLE).join("rootfs.img").exists() && dir.join("claude-code/2.1.0/claude").exists());
    assert!(!dir.join(BUNDLE).join("machineIdentifier").exists() && !dir.join("Cache").exists());
    // Never copies the default profile's login.
    assert!(!dir.join("config.json").exists() && !dir.join("Cookies").exists());
    // Registered, unprotected, and Claude opened on the absolute path.
    let reg = Registry::load(&fx.paths).unwrap();
    assert_eq!(reg.profiles["team"], ProfileEntry { dir: dir.clone(), protected: false, from: None });
    assert!(report.opened_for_sign_in);
    assert_eq!(sys.opened_args.borrow().as_slice(), &[vec![format!("--user-data-dir={}", dir.display())]]);
    // The default profile is untouched.
    assert_eq!(profile_uuid(&fx.default()).unwrap().as_deref(), Some(U_PERSONAL));
}

#[test]
fn add_refuses_while_claude_runs_and_names_the_processes() {
    let fx = Fx::new();
    fx.default_with_runtime();
    let sys = Rc::new(FakeSystem::new(true));
    sys.add(77, MAIN);
    let err = add_profile(&fx.env(), sys.as_ref(), &req("team", None, false)).unwrap_err().to_string();
    assert!(err.contains("Claude is running") && err.contains("pid 77"), "{err}");
    assert!(!fx.root().join("team").exists());
    assert!(sys.opened_args.borrow().is_empty());

    // A dry run reports the refusal without failing or writing.
    let report = add_profile(&fx.env(), sys.as_ref(), &req("team", None, true)).unwrap();
    assert!(report.notes.iter().any(|n| n.contains("would refuse")), "{:?}", report.notes);
    assert!(!fx.root().join("team").exists());
}

#[test]
fn a_new_profile_for_the_account_the_default_folder_already_holds_is_refused() {
    let fx = Fx::new();
    fx.default_with_runtime();
    let sys = Rc::new(FakeSystem::new(true));
    let err = add_profile(&fx.env(), sys.as_ref(), &req("personal", None, false)).unwrap_err().to_string();
    assert!(err.contains("--from-profile"), "{err}");
}

#[test]
fn unknown_and_foreign_accounts_are_rejected() {
    let fx = Fx::new();
    let sys = Rc::new(FakeSystem::new(true));
    assert!(matches!(add_profile(&fx.env(), sys.as_ref(), &req("ghost", None, false)), Err(Error::NotFound(_))));
    let mut config = Config::load(&fx.paths).unwrap();
    config.add_account(Account { id: "gpt".into(), provider: Provider::Openai, label: "g".into(), email: None, uuid: None }).unwrap();
    config.save(&fx.paths).unwrap();
    assert!(add_profile(&fx.env(), sys.as_ref(), &req("gpt", None, false)).is_err());
}

// ---------------------------------------------------------------------------------------------
// add: existing profiles
// ---------------------------------------------------------------------------------------------

#[test]
fn from_profile_clones_into_the_managed_folder_and_leaves_the_original() {
    let fx = Fx::new();
    let src = fx._home.path().join("tmp/desktop-b");
    let src = {
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        make_profile(&src, Some(U_TEAM), Some(future()));
        fs::write(src.join("extra.txt"), b"keep me").unwrap();
        fs::canonicalize(&src).unwrap()
    };
    let sys = Rc::new(FakeSystem::new(true));

    let dry = add_profile(&fx.env(), sys.as_ref(), &req("team", Some(&src), true)).unwrap();
    assert!(dry.dry_run && !fx.root().join("team").exists());

    let report = add_profile(&fx.env(), sys.as_ref(), &req("team", Some(&src), false)).unwrap();
    let dir = fx.root().join("team");
    assert_eq!(report.dir, dir);
    assert_eq!(report.clone_stats, Some(tree_stats(&src).unwrap()));
    assert_eq!(tree_stats(&dir).unwrap(), tree_stats(&src).unwrap());
    assert_eq!(fs::read(dir.join("extra.txt")).unwrap(), b"keep me");
    assert!(src.join("extra.txt").exists(), "the original is not deleted");
    assert_eq!(report.session, Some(Session::Valid));
    let entry = &Registry::load(&fx.paths).unwrap().profiles["team"];
    assert_eq!((entry.protected, entry.from.as_deref()), (false, Some(src.as_path())));

    // Repeating the same request changes nothing; a different folder is refused.
    let again = add_profile(&fx.env(), sys.as_ref(), &req("team", Some(&src), false)).unwrap();
    assert!(again.already_registered && again.dir == dir);
    let other = fx._home.path().join("other");
    make_profile(&other, Some(U_TEAM), None);
    let err = add_profile(&fx.env(), sys.as_ref(), &req("team", Some(&other), false)).unwrap_err().to_string();
    assert!(err.contains("already has the profile"), "{err}");
}

#[test]
fn from_profile_refuses_a_folder_signed_in_as_someone_else() {
    let fx = Fx::new();
    let src = fx._home.path().join("tmp/wrong");
    make_profile(&src, Some(U_PERSONAL), Some(future()));
    let sys = Rc::new(FakeSystem::new(true));
    let err = add_profile(&fx.env(), sys.as_ref(), &req("team", Some(&src), false)).unwrap_err().to_string();
    assert!(err.contains("another account"), "{err}");
    assert!(Registry::load(&fx.paths).unwrap().profiles.is_empty() && !fx.root().join("team").exists());
}

#[test]
fn registering_the_default_profile_is_in_place_protected_and_idempotent() {
    let fx = Fx::new();
    fx.default_with_runtime();
    let before = tree_stats(&fx.default()).unwrap();
    let sys = Rc::new(FakeSystem::new(true));
    sys.add(5, MAIN); // registering in place touches no data, so a running Claude is fine
    let default = fx.default();
    let report = add_profile(&fx.env(), sys.as_ref(), &req("personal", Some(&default), false)).unwrap();
    assert!(report.in_place && report.dir == default);
    assert_eq!(tree_stats(&default).unwrap(), before);
    let entry = &Registry::load(&fx.paths).unwrap().profiles["personal"];
    assert!(entry.protected && entry.dir == default);

    assert!(add_profile(&fx.env(), sys.as_ref(), &req("personal", Some(&default), false)).unwrap().already_registered);
    // A different folder can never replace the protected one.
    let other = fx._home.path().join("other");
    make_profile(&other, Some(U_PERSONAL), None);
    let err = add_profile(&fx.env(), sys.as_ref(), &req("personal", Some(&other), false)).unwrap_err().to_string();
    assert!(err.contains("protected"), "{err}");
    assert_eq!(tree_stats(&default).unwrap(), before);
}

#[test]
fn two_accounts_cannot_share_one_folder() {
    let fx = Fx::new();
    fx.default_with_runtime();
    fx.register("personal", &fx.default(), true);
    let err = register(&fx.paths, "team", ProfileEntry { dir: fx.default(), protected: false, from: None }).unwrap_err().to_string();
    assert!(err.contains("already the profile of `personal`"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------------------------

#[test]
fn remove_refuses_the_protected_folder_and_deletes_a_managed_one() {
    let fx = Fx::new();
    fx.default_with_runtime();
    fx.register("personal", &fx.default(), true);
    let sys = FakeSystem::new(true);
    let err = plan_remove(&fx.paths, &sys, "personal").unwrap_err().to_string();
    assert!(err.contains("protected") && err.contains("never deletes"), "{err}");
    // Even a forged call to the deleter refuses it.
    let entry = Registry::load(&fx.paths).unwrap().profiles["personal"].clone();
    assert!(finish_remove(&fx.paths, "personal", &entry).is_err());
    assert!(fx.default().join("config.json").exists());

    let team = fx.team_profile();
    assert_eq!(plan_remove(&fx.paths, &sys, "ghost").unwrap(), None);
    let planned = plan_remove(&fx.paths, &sys, "team").unwrap().unwrap();
    finish_remove(&fx.paths, "team", &planned).unwrap();
    assert!(!team.exists() && !Registry::load(&fx.paths).unwrap().profiles.contains_key("team"));
}

#[test]
fn remove_refuses_the_profile_the_default_link_points_at() {
    let fx = Fx::new();
    let team = fx.team_profile();
    fs::create_dir_all(fx.default().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&team, fx.default()).unwrap();
    let sys = FakeSystem::new(true);
    let err = plan_remove(&fx.paths, &sys, "team").unwrap_err().to_string();
    assert!(err.contains("points at the profile of `team`") && err.contains("dangling"), "{err}");
    assert!(team.exists(), "nothing was deleted");

    // Once the link points elsewhere the profile can go.
    fs::remove_file(fx.default()).unwrap();
    assert!(plan_remove(&fx.paths, &sys, "team").unwrap().is_some());
}

#[test]
fn remove_waits_for_a_running_profile_and_never_follows_links_or_leaves_the_managed_folder() {
    let fx = Fx::new();
    let team = fx.team_profile();
    let sys = FakeSystem::new(true);
    sys.add(9, &format!("{MAIN} --user-data-dir={}", team.display()));
    assert!(plan_remove(&fx.paths, &sys, "team").unwrap_err().to_string().contains("running"));

    let outside = fx._home.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let entry = ProfileEntry { dir: outside.clone(), protected: false, from: None };
    assert!(finish_remove(&fx.paths, "team", &entry).is_err());
    assert!(outside.exists());

    let linked = fx.root().join("linked");
    std::os::unix::fs::symlink(&outside, &linked).unwrap();
    let entry = ProfileEntry { dir: linked.clone(), protected: false, from: None };
    assert!(finish_remove(&fx.paths, "team", &entry).is_err());
    assert!(outside.exists(), "a symlinked profile is never followed");
}

// ---------------------------------------------------------------------------------------------
// The link
// ---------------------------------------------------------------------------------------------

#[test]
fn swapping_the_link_is_atomic_and_fails_closed_on_a_real_folder() {
    let d = tempfile::tempdir().unwrap();
    let (a, b) = (d.path().join("a"), d.path().join("b"));
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let at = d.path().join("Claude");

    swap_symlink(&at, &a).unwrap(); // nothing there yet
    assert_eq!(link_state(&at).unwrap(), LinkState::Symlink(a.clone()));
    swap_symlink(&at, &b).unwrap();
    assert_eq!(link_state(&at).unwrap(), LinkState::Symlink(b.clone()));
    swap_symlink(&at, &b).unwrap(); // same target: no-op
    let names: Vec<_> = fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().all(|n| !n.contains("agent-switch-link")), "temporary link left behind: {names:?}");

    fs::remove_file(&at).unwrap();
    fs::create_dir_all(at.join("data")).unwrap();
    fs::write(at.join("data/keep"), b"real").unwrap();
    let err = swap_symlink(&at, &a).unwrap_err().to_string();
    assert!(err.contains("real folder"), "{err}");
    assert_eq!(fs::read(at.join("data/keep")).unwrap(), b"real", "the real folder is untouched");
    assert_eq!(link_state(&at).unwrap(), LinkState::Real);
}

fn registered_default(fx: &Fx) -> TreeStats {
    fx.default_with_runtime();
    let sys = Rc::new(FakeSystem::new(true));
    let default = fx.default();
    add_profile(&fx.env(), sys.as_ref(), &req("personal", Some(&default), false)).unwrap();
    tree_stats(&fx.default()).unwrap()
}

use crate::apfs_clone::TreeStats;

#[test]
fn enabling_the_link_backs_up_verifies_moves_and_links() {
    let fx = Fx::new();
    let before = registered_default(&fx);
    let sys = FakeSystem::new(true);
    sys.add(1, MAIN);

    let dry = enable_default_link(&fx.env(), &sys, &quick(), reask(), true).unwrap();
    assert!(!dry.changed && link_state(&fx.default()).unwrap() == LinkState::Real && *sys.quit_calls.borrow() == 0);

    let report = enable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap();
    let target = fx.root().join("personal");
    assert!(report.changed && report.target == target);
    // Claude was quit first, the backup matches, the data moved, the link points at it.
    assert!(*sys.quit_calls.borrow() >= 1);
    let backup = report.backup.unwrap();
    assert!(backup.starts_with(backups_dir(&fx.paths)) && backup.file_name().unwrap().to_string_lossy().starts_with("claude-default-"));
    assert_eq!(tree_stats(&backup).unwrap(), before);
    assert_eq!(tree_stats(&target).unwrap(), before);
    assert_eq!(link_state(&fx.default()).unwrap(), LinkState::Symlink(target.clone()));
    assert_eq!(profile_uuid(&fx.default()).unwrap().as_deref(), Some(U_PERSONAL), "the app still finds its data through the link");
    let reg = Registry::load(&fx.paths).unwrap();
    assert!(reg.link && reg.profiles["personal"].protected && reg.profiles["personal"].dir == target);

    // Running it again changes nothing.
    let again = enable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap();
    assert!(!again.changed && again.notes.iter().any(|n| n.contains("already")));
}

#[test]
fn enabling_changes_nothing_when_claude_will_not_quit() {
    let fx = Fx::new();
    let before = registered_default(&fx);
    let sys = FakeSystem::new(false);
    sys.add(1, MAIN);
    let err = enable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap_err().to_string();
    assert!(err.contains("did not quit"), "{err}");
    assert_eq!(link_state(&fx.default()).unwrap(), LinkState::Real);
    assert_eq!(tree_stats(&fx.default()).unwrap(), before);
    assert!(!backups_dir(&fx.paths).join("x").exists() && fs::read_dir(backups_dir(&fx.paths)).unwrap().next().is_none());
    assert!(!Registry::load(&fx.paths).unwrap().link);
}

#[test]
fn enabling_waits_out_processes_still_running_from_the_folder() {
    let fx = Fx::new();
    let before = registered_default(&fx);
    let sys = FakeSystem::new(true);
    sys.add(42, &format!("{}/claude-code/2.1.0/claude --output-format stream-json", fx.default().display()));
    let err = enable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap_err().to_string();
    assert!(err.contains("still run from") && err.contains("42"), "{err}");
    assert_eq!(link_state(&fx.default()).unwrap(), LinkState::Real);
    assert_eq!(tree_stats(&fx.default()).unwrap(), before);
}

#[test]
fn a_failure_after_the_move_puts_the_real_folder_back() {
    let fx = Fx::new();
    let before = registered_default(&fx);
    let sys = FakeSystem::new(true);
    // Hold the registry lock so the final registry write fails after the folder was moved and linked.
    let _held = FlockGuard::lock(&fx.paths.locks_dir().join("claude-desktop.lock"), Duration::from_secs(1)).unwrap();
    let err = enable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap_err().to_string();
    assert!(err.contains("real folder is back"), "{err}");
    assert_eq!(link_state(&fx.default()).unwrap(), LinkState::Real);
    assert_eq!(tree_stats(&fx.default()).unwrap(), before);
    assert!(!fx.root().join("personal").exists());
    // The backup stays, as evidence.
    assert_eq!(fs::read_dir(backups_dir(&fx.paths)).unwrap().count(), 1);
}

#[test]
fn enabling_needs_the_default_folder_registered_and_refuses_foreign_links() {
    let fx = Fx::new();
    fx.default_with_runtime();
    let sys = FakeSystem::new(true);
    let err = enable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap_err().to_string();
    assert!(err.contains("not registered"), "{err}");
    assert_eq!(link_state(&fx.default()).unwrap(), LinkState::Real);

    let elsewhere = fx._home.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::remove_dir_all(fx.default()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, fx.default()).unwrap();
    assert!(enable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap_err().to_string().contains("did not create"));
    assert!(disable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap_err().to_string().contains("did not create"));
    assert!(elsewhere.exists());
}

#[test]
fn disabling_moves_the_protected_folder_back_to_the_default_path() {
    let fx = Fx::new();
    let before = registered_default(&fx);
    let sys = FakeSystem::new(true);
    enable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap();
    // Point the link at the team profile, as a switch would.
    let team = fx.team_profile();
    swap_symlink(&fx.default(), &team).unwrap();

    let dry = disable_default_link(&fx.env(), &sys, &quick(), reask(), true).unwrap();
    assert!(!dry.changed && link_state(&fx.default()).unwrap() == LinkState::Symlink(team.clone()));

    let report = disable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap();
    assert!(report.changed);
    assert_eq!(link_state(&fx.default()).unwrap(), LinkState::Real);
    assert_eq!(tree_stats(&fx.default()).unwrap(), before);
    assert!(!fx.root().join("personal").exists() && team.exists(), "team data is not touched");
    let reg = Registry::load(&fx.paths).unwrap();
    assert!(!reg.link && reg.profiles["personal"].dir == fx.default() && reg.profiles["personal"].protected);
    assert!(!disable_default_link(&fx.env(), &sys, &quick(), reask(), false).unwrap().changed);
}

// ---------------------------------------------------------------------------------------------
// The surface
// ---------------------------------------------------------------------------------------------

fn bound_fixture() -> (Fx, PathBuf, Config) {
    let fx = Fx::new();
    fx.default_with_runtime();
    fx.register("personal", &fx.default(), true);
    let team = fx.team_profile();
    let mut config = Config::load(&fx.paths).unwrap();
    config.bind(SurfaceKind::ClaudeDesktop, Binding::parse("personal")).unwrap();
    config.save(&fx.paths).unwrap();
    (fx, team, config)
}

#[test]
fn stage_checks_the_profile_without_side_effects() {
    let (fx, team, _) = bound_fixture();
    let sys = Rc::new(FakeSystem::new(true));
    sys.add(1, MAIN);
    let a = fx.adapter(&sys);
    let staged = a.stage(&fx.env(), &fx.account("team")).unwrap();
    assert_eq!(staged.payload, team.to_str().unwrap().as_bytes());
    assert_eq!(*sys.quit_calls.borrow(), 0);

    // Unknown profile, and a profile that signs in as someone else.
    let mut config = Config::load(&fx.paths).unwrap();
    config.add_account(Account { id: "third".into(), provider: Provider::Claude, label: "t".into(), email: None, uuid: Some("33333333-x".into()) }).unwrap();
    config.save(&fx.paths).unwrap();
    assert!(matches!(a.stage(&fx.env(), &fx.account("third")), Err(Error::NotFound(_))));
    make_profile(&team, Some(U_PERSONAL), Some(future()));
    assert!(a.stage(&fx.env(), &fx.account("team")).unwrap_err().to_string().contains("another account"));
}

#[test]
fn switching_while_claude_runs_needs_consent_and_names_the_pids() {
    let (fx, team, config) = bound_fixture();
    let sys = Rc::new(FakeSystem::new(true));
    sys.add(10, MAIN);
    sys.add(11, "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper --type=gpu-process");
    let a = fx.adapter(&sys);
    let err = switch::activate(&fx.env(), &config, &a, "team").unwrap_err().to_string();
    assert!(err.contains("Claude is running") && err.contains("pid 10") && err.contains("--restart"), "{err}");
    assert_eq!(*sys.quit_calls.borrow(), 0);
    assert_eq!(State::load(&fx.paths).unwrap().active(SurfaceKind::ClaudeDesktop), None, "a refused switch is not journaled");
    let _ = team;
}

#[test]
fn a_restart_quits_every_instance_then_opens_the_target_profile() {
    let (fx, team, config) = bound_fixture();
    let mut fake = FakeSystem::new(true);
    fake.quit_one_per_request = true; // one request reaches one instance
    fake.spawn_on_open = true;
    let sys = Rc::new(fake);
    sys.add(10, MAIN);
    sys.add(20, &format!("{MAIN} --user-data-dir={}", team.display()));
    sys.add(21, "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper --type=renderer");
    let a = fx.adapter(&sys).with_restart();

    switch::activate(&fx.env(), &config, &a, "team").unwrap();
    assert!(bundle_pids(&sys.processes().unwrap(), &CLAUDE).is_empty(), "every PID is gone before the reopen");
    a.restart(&fx.env()).unwrap();
    assert_eq!(sys.opened_args.borrow().as_slice(), &[vec![format!("--user-data-dir={}", team.display())]]);
    assert_eq!(State::load(&fx.paths).unwrap().active(SurfaceKind::ClaudeDesktop), Some("team"));
    // A second restart call opens nothing more.
    a.restart(&fx.env()).unwrap();
    assert_eq!(sys.opened_args.borrow().len(), 1);
}

#[test]
fn a_switch_onto_a_signed_out_profile_is_reported_not_celebrated() {
    let (fx, team, config) = bound_fixture();
    fs::remove_file(team.join("Cookies")).unwrap();
    let mut fake = FakeSystem::new(true);
    fake.spawn_on_open = true;
    let sys = Rc::new(fake);
    sys.add(10, MAIN);
    let a = fx.adapter(&sys).with_restart();
    switch::activate(&fx.env(), &config, &a, "team").unwrap();
    let err = a.restart(&fx.env()).unwrap_err().to_string();
    assert!(err.contains("not signed in") && err.contains("signed out"), "{err}");

    // An expired cookie is the same.
    make_profile(&team, Some(U_TEAM), Some(past()));
    switch::activate(&fx.env(), &config, &a, "personal").unwrap();
    a.restart(&fx.env()).unwrap();
    switch::activate(&fx.env(), &config, &a, "team").unwrap();
    assert!(a.restart(&fx.env()).unwrap_err().to_string().contains("expired"));
}

#[test]
fn switching_to_the_profile_already_running_quits_nothing() {
    let (fx, _team, config) = bound_fixture();
    let sys = Rc::new(FakeSystem::new(true));
    sys.add(10, MAIN); // no argument: the default profile, which is personal's
    let a = fx.adapter(&sys).with_restart();
    switch::activate(&fx.env(), &config, &a, "personal").unwrap();
    a.restart(&fx.env()).unwrap();
    assert_eq!(*sys.quit_calls.borrow(), 0);
    assert!(sys.opened_args.borrow().is_empty());
}

#[test]
fn with_the_link_enabled_a_switch_repoints_it_and_a_real_folder_stops_everything() {
    let (fx, team, config) = bound_fixture();
    let sys = Rc::new(FakeSystem::new(true));
    let a = fx.adapter(&sys);
    enable_default_link(&fx.env(), sys.as_ref(), &quick(), reask(), false).unwrap();
    let personal = fx.root().join("personal");
    assert_eq!(link_state(&fx.default()).unwrap(), LinkState::Symlink(personal.clone()));

    switch::activate(&fx.env(), &config, &a, "team").unwrap();
    assert_eq!(link_state(&fx.default()).unwrap(), LinkState::Symlink(team.clone()));
    assert_eq!(profile_uuid(&fx.default()).unwrap().as_deref(), Some(U_TEAM));
    switch::activate(&fx.env(), &config, &a, "personal").unwrap();
    assert_eq!(link_state(&fx.default()).unwrap(), LinkState::Symlink(personal.clone()));

    // Claude made a fresh real folder in the meantime: fail closed before quitting anything.
    fs::remove_file(fx.default()).unwrap();
    fs::create_dir_all(fx.default()).unwrap();
    fs::write(fx.default().join("fresh"), b"new data").unwrap();
    sys.add(10, MAIN);
    let a = fx.adapter(&sys).with_restart();
    let err = switch::activate(&fx.env(), &config, &a, "team").unwrap_err().to_string();
    assert!(err.contains("real folder"), "{err}");
    assert_eq!(*sys.quit_calls.borrow(), 0);
    assert_eq!(fs::read(fx.default().join("fresh")).unwrap(), b"new data");
}

#[test]
fn current_identity_reads_the_running_profile_and_warns_about_a_mismatch() {
    let (fx, team, _) = bound_fixture();
    let sys = Rc::new(FakeSystem::new(true));
    let a = fx.adapter(&sys);

    // Not running: the default folder, i.e. personal.
    let id = a.current_identity(&fx.env()).unwrap().unwrap();
    assert_eq!((id.account_id.as_deref(), id.uuid.as_deref()), (Some("personal"), Some(U_PERSONAL)));
    assert!(id.note.unwrap().contains("not running"));

    // Running on the team profile while the binding says personal.
    sys.add(20, &format!("{MAIN} --user-data-dir={}", team.display()));
    let id = a.current_identity(&fx.env()).unwrap().unwrap();
    assert_eq!((id.account_id.as_deref(), id.email.as_deref()), (Some("team"), Some("team@x.com")));
    assert!(id.note.unwrap().contains("differs from the binding `personal`"));

    // Two instances, one signed out.
    sys.add(21, MAIN);
    fs::remove_file(fx.default().join("Cookies")).unwrap();
    sys.procs.borrow_mut().retain(|p| p.pid != 20);
    let id = a.current_identity(&fx.env()).unwrap().unwrap();
    assert!(id.note.unwrap().contains("signed out"));
}

#[test]
fn a_profile_that_was_never_signed_in_has_no_identity() {
    let fx = Fx::new();
    make_profile(&fx.default(), None, None);
    let sys = Rc::new(FakeSystem::new(true));
    assert_eq!(fx.adapter(&sys).current_identity(&fx.env()).unwrap(), None);
    fs::remove_dir_all(fx.default()).unwrap();
    assert_eq!(fx.adapter(&sys).current_identity(&fx.env()).unwrap(), None);
}

// ---------------------------------------------------------------------------------------------
// Smaller pieces
// ---------------------------------------------------------------------------------------------

#[test]
fn confirm_sign_in_waits_for_the_right_account() {
    let fx = Fx::new();
    let team = fx.account("team");
    let dir = fx.root().join("team");
    fs::create_dir_all(&dir).unwrap();
    let short = Duration::from_millis(60);
    let err = confirm_sign_in(&team, &dir, short, Duration::from_millis(5)).unwrap_err().to_string();
    assert!(err.contains("not confirmed"), "{err}");

    make_profile(&dir, Some(U_TEAM), Some(future()));
    assert_eq!(confirm_sign_in(&team, &dir, short, Duration::from_millis(5)).unwrap(), Session::Valid);

    make_profile(&dir, Some(U_PERSONAL), Some(future()));
    let err = confirm_sign_in(&team, &dir, short, Duration::from_millis(5)).unwrap_err().to_string();
    assert!(err.contains("another account"), "{err}");

    // The right uuid with no session cookie yet is not enough.
    make_profile(&dir, Some(U_TEAM), None);
    fs::remove_file(dir.join("Cookies")).ok();
    assert!(confirm_sign_in(&team, &dir, short, Duration::from_millis(5)).unwrap_err().to_string().contains("not confirmed"));
}

#[test]
fn only_env_key_names_are_listed() {
    let keys = mcp_env_keys(br#"{"mcpServers":{"a":{"env":{"K1":"secret-1","K2":"secret-2"}},"b":{}}}"#);
    assert_eq!(keys, vec!["a.K1", "a.K2"]);
    assert!(mcp_env_keys(b"not json").is_empty() && mcp_env_keys(b"{}").is_empty());
}

#[test]
fn backup_names_use_a_utc_stamp() {
    assert_eq!(stamp(0), "19700101-000000");
    assert_eq!(stamp(1_793_939_033), "20261106-042353");
    assert_eq!(stamp(951_782_400), "20000229-000000"); // leap day
}

#[test]
fn the_registry_survives_a_round_trip_and_finds_folders_by_identity() {
    let fx = Fx::new();
    assert_eq!(Registry::load(&fx.paths).unwrap(), Registry::default());
    let dir = fx.root().join("team");
    fs::create_dir_all(&dir).unwrap();
    fx.register("team", &dir, false);
    let reg = Registry::load(&fx.paths).unwrap();
    assert_eq!(reg.entry_for_dir(&dir).map(|(id, _)| id.as_str()), Some("team"));
    assert_eq!(fs::metadata(profiles_root(&fx.paths).join("profiles.json")).unwrap().permissions().mode() & 0o777, 0o600);
    // A failed change saves nothing.
    assert!(Registry::update(&fx.paths, |r| -> Result<()> {
        r.link = true;
        Err(Error::Invalid("no".into()))
    })
    .is_err());
    assert!(!Registry::load(&fx.paths).unwrap().link);
}

#[test]
fn profile_paths_must_be_absolute_for_the_launcher() {
    let sys = FakeSystem::new(true);
    assert!(open_profile(&sys, Path::new("~/relative")).is_err());
    assert!(sys.opened_args.borrow().is_empty());
}

#[test]
fn open_account_profile_opens_once_and_never_twice_on_one_folder() {
    let fx = Fx::new();
    let dir = profiles_root(&fx.paths).join("team");
    fs::create_dir_all(&dir).unwrap();
    fx.register("team", &dir, false);
    let sys = FakeSystem::new(true);
    // Another profile is open: a new instance on the team folder is started.
    sys.add(10, &format!("{MAIN} --user-data-dir=/somewhere/else"));
    assert!(open_account_profile(&sys, &fx.paths, "team").unwrap());
    assert_eq!(sys.opened_args.borrow().len(), 1);
    assert_eq!(sys.opened_args.borrow()[0], [format!("--user-data-dir={}", canon(&dir).display())]);
    // One already runs on the team folder: nothing is opened.
    sys.add(11, &format!("{MAIN} --user-data-dir={}", dir.display()));
    assert!(!open_account_profile(&sys, &fx.paths, "team").unwrap());
    assert_eq!(sys.opened_args.borrow().len(), 1);
    // No profile for the account.
    assert!(open_account_profile(&sys, &fx.paths, "personal").is_err());
}

#[test]
fn doctor_reports_each_registered_profile() {
    let (fx, _team, _) = bound_fixture();
    let checks = doctor_checks(&fx.env());
    let text: Vec<String> = checks.iter().map(|c| c.detail.clone()).collect();
    assert!(text.iter().any(|t| t.contains("profile `personal`: signed in") && t.contains("protected")), "{text:?}");
    assert!(text.iter().any(|t| t.contains("profile `team`: signed in")), "{text:?}");
    assert!(text.iter().any(|t| t.contains("default folder")), "{text:?}");
    assert!(checks.iter().all(|c| !c.detail.contains(COOKIE_VALUE)));
}
