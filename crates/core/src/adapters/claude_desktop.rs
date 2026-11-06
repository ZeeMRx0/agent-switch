//! Claude Desktop (`com.anthropic.claudefordesktop`). One Electron profile folder per account, opened with
//! `--user-data-dir=<absolute path>`; a switch quits every instance, confirms all exited by PID, and opens
//! the target profile. Swapping data in place does not work: the chat login is the claude.ai `sessionKey`
//! cookie, not the token cache.
//!
//! Identity: `lastKnownAccountUuid` in the profile's `config.json` is a **hint** (a failed in-place test
//! overwrote it and the app stayed on the old account). After a reopen, the claude.ai `sessionKey` cookie
//! must exist and be unexpired; only its name and expiry are read, never its value.
//!
//! Dock and Spotlight launches always open the app's default folder. When the optional default-folder
//! link is enabled (see `manage`), `apply` repoints that symlink at the active profile with a temporary
//! link plus `rename`, and fails closed if the path has become a real folder.
//!
//! Ported from Nemo-Illusionist/claude-code-account-switcher (MIT): `docs/desktop.md` (profile model),
//! `src/desktop.rs` (`lastKnownAccountUuid`, launch), `src/desktop_runtime.rs` (runtime clone).

pub mod manage;
pub mod session_mirror;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{Account, Config, SurfaceKind};
use crate::doctor::{Check, Level};
use crate::error::{Error, Result};
use crate::fsutil::{ensure_private_dir, safe_write, WriteOpts};
use crate::locks::FlockGuard;
use crate::macos_apps::{self, bundle_pids, instances, join_pids, quit_all_instances, quit_all_instances_with, Instance, ProcInfo, QuitOpts, System, SystemApps, CLAUDE};
use crate::paths::Paths;
use crate::proc;
use crate::state::now_secs;
use crate::surface::{Env, Identity, Staged, Surface};

/// The Claude Desktop version this adapter was tested with (2026-11-06).
pub const TESTED_CLAUDE_DESKTOP: &str = "4.23118.0";

const REGISTRY_LOCK_TIMEOUT: Duration = Duration::from_millis(if cfg!(test) { 300 } else { 5_000 });
const SQLITE3: &str = "/usr/bin/sqlite3";
const CHROMIUM_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;

// ---------------------------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------------------------

/// The app's own profile, the one a Dock launch opens: `~/Library/Application Support/Claude`.
pub fn default_profile(paths: &Paths) -> PathBuf {
    paths.home().join("Library/Application Support/Claude")
}

/// `~/Library/Application Support/agent-switch/claude-desktop`: managed profiles and the registry.
pub fn profiles_root(paths: &Paths) -> PathBuf {
    paths.support_dir().join("claude-desktop")
}

pub fn backups_dir(paths: &Paths) -> PathBuf {
    paths.support_dir().join("backups")
}

/// `fs::canonicalize`, or the path itself when it does not resolve.
pub(crate) fn canon(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    canon(a) == canon(b)
}

/// Folders Claude Desktop may run from: the default profile and every registered one.
pub fn known_profile_dirs(paths: &Paths) -> Vec<PathBuf> {
    let mut dirs = vec![default_profile(paths)];
    if let Ok(registry) = Registry::load(paths) {
        dirs.extend(registry.profiles.values().map(|e| e.dir.clone()));
    }
    dirs
}

/// Claude Code agents that Desktop starts run from `<profile>/claude-code/<version>/<hash>/claude.app/Contents/MacOS/`,
/// outside the app bundle. A `claude` started from a terminal runs from other paths and is not matched.
pub fn claude_code_pids(procs: &[ProcInfo], profile_dirs: &[PathBuf]) -> Vec<u32> {
    let mut prefixes: Vec<String> = Vec::new();
    for dir in profile_dirs {
        for root in [dir.clone(), canon(dir)] {
            let prefix = format!("{}/claude-code/", root.display());
            if !prefixes.contains(&prefix) {
                prefixes.push(prefix);
            }
        }
    }
    procs
        .iter()
        .filter(|p| p.command.contains("/claude.app/Contents/MacOS/") && prefixes.iter().any(|prefix| p.command.starts_with(prefix)))
        .map(|p| p.pid)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Registry: which folder is each account's profile
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub dir: PathBuf,
    /// The original Claude data (moved or registered in place). `remove`, uninstall and a repeated `add`
    /// refuse to delete it.
    #[serde(default)]
    pub protected: bool,
    /// The folder `add --from-profile` cloned this profile from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<PathBuf>,
}

/// `claude-desktop/profiles.json`: paths and flags only, never credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registry {
    pub version: u32,
    /// The default folder is a symlink that agent-switch keeps pointed at the active profile.
    pub link: bool,
    pub profiles: BTreeMap<String, ProfileEntry>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry { version: 1, link: false, profiles: BTreeMap::new() }
    }
}

impl Registry {
    fn file(paths: &Paths) -> PathBuf {
        profiles_root(paths).join("profiles.json")
    }

    pub fn load(paths: &Paths) -> Result<Registry> {
        let file = Self::file(paths);
        match fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", file.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(Error::io(format!("read {}", file.display()), e)),
        }
    }

    /// Load, change and save under a cross-process lock. Nothing is saved when `change` fails.
    pub fn update<T>(paths: &Paths, change: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        ensure_private_dir(&profiles_root(paths))?;
        let _guard = FlockGuard::lock(&paths.locks_dir().join("claude-desktop.lock"), REGISTRY_LOCK_TIMEOUT)?;
        let mut reg = Registry::load(paths)?;
        let out = change(&mut reg)?;
        let text = serde_json::to_string_pretty(&reg).map_err(|e| Error::Config(e.to_string()))?;
        safe_write(&Self::file(paths), text.as_bytes(), &WriteOpts::secret())?;
        Ok(out)
    }

    pub fn entry_for_dir(&self, dir: &Path) -> Option<(&String, &ProfileEntry)> {
        self.profiles.iter().find(|(_, e)| same_dir(&e.dir, dir))
    }
}

// ---------------------------------------------------------------------------------------------
// Reading a profile
// ---------------------------------------------------------------------------------------------

/// `lastKnownAccountUuid` from the profile's `config.json`. Nothing else in that file is kept; it also
/// holds an encrypted token cache.
pub fn profile_uuid(dir: &Path) -> Result<Option<String>> {
    let file = dir.join("config.json");
    let text = match fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(format!("read {}", file.display()), e)),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| Error::Invalid(format!("{} is not valid JSON ({e})", file.display())))?;
    Ok(value.get("lastKnownAccountUuid").and_then(|v| v.as_str()).map(str::to_string))
}

/// The state of the claude.ai web login in a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    Valid,
    Expired,
    Missing,
    Unreadable(String),
}

impl Session {
    pub fn is_valid(&self) -> bool {
        matches!(self, Session::Valid)
    }

    pub fn describe(&self) -> String {
        match self {
            Session::Valid => "session cookie present".into(),
            Session::Expired => "signed out (the claude.ai session cookie has expired)".into(),
            Session::Missing => "signed out (no claude.ai session cookie)".into(),
            Session::Unreadable(why) => format!("session cookie could not be read ({why})"),
        }
    }
}

/// Read the expiry rows of the `sessionKey` cookie from Chromium's `Cookies` database. The query selects
/// only `has_expires` and `expires_utc`, so the value never reaches this process.
pub fn session_state(dir: &Path, now_unix: i64) -> Session {
    let cookies = dir.join("Cookies");
    if !cookies.is_file() {
        return Session::Missing;
    }
    let query = "select has_expires, expires_utc from cookies where name = 'sessionKey' and host_key in ('.claude.ai', 'claude.ai')";
    let db = cookies.to_string_lossy();
    let args = ["-readonly", "-cmd", ".timeout 2000", "-separator", "|", db.as_ref(), query];
    match proc::run(Path::new(SQLITE3), &args, None, Duration::from_secs(10)) {
        Ok(out) if out.code == 0 => parse_session_rows(&String::from_utf8_lossy(&out.stdout), now_unix),
        Ok(out) => Session::Unreadable(String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("sqlite3 failed").trim().to_string()),
        Err(e) => Session::Unreadable(e.to_string()),
    }
}

/// Rows are `has_expires|expires_utc`; `expires_utc` counts microseconds since 1601-01-01.
pub fn parse_session_rows(stdout: &str, now_unix: i64) -> Session {
    let mut any = false;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        any = true;
        let Some((has_expires, expires_utc)) = line.trim().split_once('|') else { continue };
        if has_expires != "1" {
            return Session::Valid; // a session cookie lives as long as the app does
        }
        if let Ok(micros) = expires_utc.parse::<i64>() {
            if micros / 1_000_000 - CHROMIUM_EPOCH_OFFSET_SECS > now_unix {
                return Session::Valid;
            }
        }
    }
    if any {
        Session::Expired
    } else {
        Session::Missing
    }
}

fn short(uuid: &str) -> String {
    uuid.chars().take(8).collect()
}

/// Verify the profile at `dir` belongs to `account`: a folder, signed in once, and the uuid hint matches.
/// Returns the web-session state, which the caller reports but this check does not require.
pub fn check_profile(account: &Account, dir: &Path) -> Result<Session> {
    if !dir.is_dir() {
        return Err(Error::Invalid(format!("the profile folder {} is missing", dir.display())));
    }
    let expected = account.uuid.as_deref().ok_or_else(|| {
        Error::Invalid(format!("account `{}` has no uuid recorded, so its Claude Desktop profile cannot be verified", account.id))
    })?;
    match profile_uuid(dir)? {
        None => Err(Error::Invalid(format!(
            "the profile {} has no signed-in account yet; sign in to Claude Desktop on it first",
            dir.display()
        ))),
        Some(got) if got != expected => Err(Error::Invalid(format!(
            "the profile {} is signed in as another account (uuid {}…, expected {}… for `{}`); refusing to use it",
            dir.display(),
            short(&got),
            short(expected),
            account.id
        ))),
        Some(_) => Ok(session_state(dir, now_secs() as i64)),
    }
}

// ---------------------------------------------------------------------------------------------
// The default-folder link
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Missing,
    Symlink(PathBuf),
    /// A real folder or file.
    Real,
}

pub fn link_state(path: &Path) -> Result<LinkState> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            Ok(LinkState::Symlink(fs::read_link(path).map_err(|e| Error::io(format!("read link {}", path.display()), e))?))
        }
        Ok(_) => Ok(LinkState::Real),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LinkState::Missing),
        Err(e) => Err(Error::io(format!("stat {}", path.display()), e)),
    }
}

fn real_folder_error(at: &Path) -> Error {
    Error::Invalid(format!(
        "{} is a real folder, not the link agent-switch manages (Claude may have created a fresh one). Not touching it; \
         quit Claude and run `agent-switch default-link disable`, then check the folder by hand",
        at.display()
    ))
}

/// Point the symlink `at` at `target`: create a temporary link next to it, then `rename` over the old one,
/// so a reader never sees a missing path. Fails closed when `at` is a real folder.
pub fn swap_symlink(at: &Path, target: &Path) -> Result<()> {
    if matches!(link_state(at)?, LinkState::Real) {
        return Err(real_folder_error(at));
    }
    if link_state(at)? == LinkState::Symlink(target.to_path_buf()) {
        return Ok(());
    }
    let parent = at.parent().ok_or_else(|| Error::Invalid(format!("not a folder path: {}", at.display())))?;
    let name = at.file_name().and_then(|n| n.to_str()).unwrap_or("link");
    let tmp = parent.join(format!(".{name}.agent-switch-link-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).map_err(|e| Error::io(format!("create link {}", tmp.display()), e))?;
    fs::rename(&tmp, at).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::io(format!("repoint {}", at.display()), e)
    })
}

// ---------------------------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------------------------

pub struct ClaudeDesktop {
    sys: Box<dyn System>,
    /// `--restart`: the user agreed to quit Claude and wants it opened on the target profile.
    restart_requested: bool,
    quit: QuitOpts,
    reask: Duration,
    launch_timeout: Duration,
    settle: Duration,
    /// Set by `apply`: the account and profile folder `restart` opens.
    reopen: RefCell<Option<(String, PathBuf)>>,
    /// `--force`: switch even when a session is mid-reply or waiting for a person.
    force: bool,
    /// With `force`: the session titles the user saw and agreed to end. A busy session outside this list
    /// refuses the switch again.
    force_only: Option<String>,
    /// Set by `apply`: running profiles whose sessions the busy pre-check could not read.
    precheck_notes: RefCell<Vec<String>>,
    /// Replaces the installed app version when deciding whether session mirroring is safe (tests).
    version_override: Option<String>,
    /// Set by `apply`: the one-line result of mirroring the session records.
    mirror_summary: RefCell<Option<String>>,
    /// Set by `apply`: why mirroring did not run or fully run. The switch itself still went ahead.
    mirror_warning: RefCell<Option<String>>,
}

impl ClaudeDesktop {
    pub fn production() -> ClaudeDesktop {
        Self::with_system(Box::new(SystemApps))
    }

    pub fn with_system(sys: Box<dyn System>) -> ClaudeDesktop {
        ClaudeDesktop {
            sys,
            restart_requested: false,
            quit: QuitOpts::default(),
            reask: Duration::from_secs(2),
            launch_timeout: Duration::from_secs(30),
            // Chromium flushes cookies on a timer; give a fresh launch a moment before judging the session.
            settle: Duration::from_secs(4),
            reopen: RefCell::new(None),
            force: false,
            force_only: None,
            precheck_notes: RefCell::new(Vec::new()),
            version_override: None,
            mirror_summary: RefCell::new(None),
            mirror_warning: RefCell::new(None),
        }
    }

    /// The user agreed to quit Claude and reopen it on the target profile.
    pub fn with_restart(mut self) -> ClaudeDesktop {
        self.restart_requested = true;
        self
    }

    /// Switch even when a session is mid-reply or waiting for a person; that work is lost.
    pub fn with_force(mut self) -> ClaudeDesktop {
        self.force = true;
        self
    }

    /// Like `with_force`, but only for the sessions in `offered` (the titles of an earlier refusal, as its
    /// text lists them). If a different session is busy by the time of the switch, it is refused again.
    pub fn with_force_if_only(mut self, offered: impl Into<String>) -> ClaudeDesktop {
        self.force = true;
        self.force_only = Some(offered.into());
        self
    }

    /// Treat `version` as the installed Claude Desktop version when gating session mirroring.
    pub fn with_desktop_version(mut self, version: impl Into<String>) -> ClaudeDesktop {
        self.version_override = Some(version.into());
        self
    }

    /// Whether `with_force` was applied.
    pub fn is_forced(&self) -> bool {
        self.force
    }

    /// After `apply`: "sessions: N mirrored, M backed up" when the session records were mirrored.
    pub fn mirror_summary(&self) -> Option<String> {
        self.mirror_summary.borrow().clone()
    }

    /// After `apply`: the reason mirroring was skipped or failed (the switch still succeeded).
    pub fn mirror_warning(&self) -> Option<String> {
        let mut parts = self.precheck_notes.borrow().clone();
        parts.extend(self.mirror_warning.borrow().clone());
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    pub fn with_timing(mut self, quit: QuitOpts, reask: Duration, launch_timeout: Duration, settle: Duration) -> ClaudeDesktop {
        self.quit = quit;
        self.reask = reask;
        self.launch_timeout = launch_timeout;
        self.settle = settle;
        self
    }

    fn running(&self) -> Result<Vec<Instance>> {
        Ok(instances(&self.sys.processes()?, &CLAUDE))
    }

    fn open_profile(&self, dir: &Path) -> Result<()> {
        open_profile(self.sys.as_ref(), dir)
    }

    /// Titles of sessions that are mid-reply or waiting for a person, on every running profile, plus a note
    /// for each running profile whose sessions could not be read (Claude's own quit prompt still protects).
    fn busy_scan(&self, running: &[Instance], default: &Path) -> (Vec<String>, Vec<String>) {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for instance in running {
            let dir = canon(instance.profile.as_deref().unwrap_or(default));
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        let (mut titles, mut notes) = (Vec::new(), Vec::new());
        for dir in &dirs {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.display().to_string());
            let unreadable = |why: String| format!("could not read sessions of {name} ({why}); relying on Claude's own quit prompt");
            match profile_uuid(dir) {
                Ok(Some(uuid)) => match session_mirror::busy_sessions(dir, &uuid) {
                    Ok(found) => titles.extend(found),
                    Err(why) => notes.push(unreadable(why)),
                },
                Ok(None) => {}
                Err(e) => notes.push(unreadable(e.to_string())),
            }
        }
        (titles, notes)
    }

    /// The refusal for a busy session. It keeps `BUSY_PREFIX` and ". Nothing was quit" so the menu bar can
    /// read the titles.
    fn busy_refusal(titles: &[String], notes: &[String], changed_since_offer: bool) -> String {
        let changed = if changed_since_offer { " The busy sessions changed since Switch anyway was offered." } else { "" };
        let notes: String = notes.iter().map(|n| format!(" Note: {n}.")).collect();
        format!(
            "{}{}. Nothing was quit.{changed} Wait for it to finish, \
             or add --force (CLI) or use Switch anyway in the menu where offered to quit now and lose it.{notes}",
            session_mirror::BUSY_PREFIX,
            titles.join(", ")
        )
    }

    /// The refusal text for a quit that Claude vetoed with its own dialog. It keeps the busy marker so the
    /// menu bar can offer "Switch anyway".
    fn veto_refusal(&self, running: &[Instance], default: &Path) -> String {
        let (titles, notes) = self.busy_scan(running, default);
        let titles = if titles.is_empty() { "a session Claude reports as working".to_string() } else { titles.join(", ") };
        let notes: String = notes.iter().map(|n| format!(" Note: {n}.")).collect();
        format!(
            "{}{titles}. Nothing was quit. Claude is asking whether to interrupt its work. Choose Cancel to keep \
             working. If you choose Quit anyway or Wait, Claude closes but is not switched; run the switch again \
             afterwards. Or add --force (CLI) or use Switch anyway in the menu where offered to end it now and lose it.{notes}",
            session_mirror::BUSY_PREFIX
        )
    }

    /// Mirror the session records between profiles when this Desktop version is the tested one. Never
    /// fails the switch: the result lands in `mirror_summary` or `mirror_warning`.
    fn mirror_sessions(&self, paths: &Paths) {
        let installed = self.version_override.clone().or_else(|| macos_apps::app_version(CLAUDE.bundle_path));
        if installed.as_deref() != Some(TESTED_CLAUDE_DESKTOP) {
            let found = installed.map_or_else(|| "not installed".to_string(), |v| format!("is {v}"));
            self.mirror_warning.replace(Some(format!(
                "sessions were not mirrored: Claude Desktop {found}, mirroring is tested with {TESTED_CLAUDE_DESKTOP}"
            )));
            return;
        }
        match session_mirror::sync(paths, || session_mirror::claude_alive(self.sys.as_ref(), paths)) {
            Ok(outcome) => {
                self.mirror_summary.replace(Some(outcome.line()));
            }
            Err(e) => {
                self.mirror_warning.replace(Some(format!("sessions were not mirrored: {e}")));
            }
        }
    }

    fn wait_for_instance_on(&self, dir: &Path, default: &Path) -> Result<()> {
        let deadline = Instant::now() + self.launch_timeout;
        loop {
            if self.running()?.iter().any(|i| same_dir(i.profile.as_deref().unwrap_or(default), dir)) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::Invalid(format!("Claude did not start on {} within {}s", dir.display(), self.launch_timeout.as_secs())));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// `open -n -b <id> --args --user-data-dir=<absolute path>`. A `~` after `=` is not expanded, so the path
/// must be absolute.
pub fn open_profile(sys: &dyn System, dir: &Path) -> Result<()> {
    if !dir.is_absolute() {
        return Err(Error::Invalid(format!("profile path {} is not absolute", dir.display())));
    }
    sys.open_new_with_args(&CLAUDE, &[format!("--user-data-dir={}", dir.display())])
}

/// Open Claude on the profile folder of `account_id` unless an instance already runs on it.
/// Returns `false` when one does. The app takes no single-instance lock, so a second copy on the same
/// folder would fight the first over its files.
pub fn open_account_profile(sys: &dyn System, paths: &Paths, account_id: &str) -> Result<bool> {
    let registry = Registry::load(paths)?;
    let entry = registry
        .profiles
        .get(account_id)
        .ok_or_else(|| Error::NotFound(format!("a Claude Desktop profile for `{account_id}`; run `agent-switch add claude-desktop {account_id}`")))?;
    let default = default_profile(paths);
    let open_already = instances(&sys.processes()?, &CLAUDE).iter().any(|i| same_dir(i.profile.as_deref().unwrap_or(&default), &entry.dir));
    if open_already {
        return Ok(false);
    }
    open_profile(sys, &canon(&entry.dir))?;
    Ok(true)
}

fn running_summary(running: &[Instance], default: &Path) -> String {
    running
        .iter()
        .map(|i| format!("pid {} on {}", i.pid, i.profile.as_deref().unwrap_or(default).display()))
        .collect::<Vec<_>>()
        .join("; ")
}

impl Surface for ClaudeDesktop {
    fn kind(&self) -> SurfaceKind {
        SurfaceKind::ClaudeDesktop
    }

    fn current_identity(&self, env: &Env) -> Result<Option<Identity>> {
        let config = Config::load(env.paths).ok();
        let default = default_profile(env.paths);
        let running = self.running()?;
        let profile = running.first().and_then(|i| i.profile.clone()).unwrap_or_else(|| default.clone());
        let profile = canon(&profile);
        if !profile.is_dir() {
            return Ok(None);
        }
        let Some(uuid) = profile_uuid(&profile)? else { return Ok(None) };

        let account = config.as_ref().and_then(|c| c.accounts.iter().find(|a| a.uuid.as_deref() == Some(uuid.as_str())));
        let mut notes = Vec::new();
        if running.is_empty() {
            notes.push("not running; shows the default folder".to_string());
        }
        if running.len() > 1 {
            notes.push(format!("{} instances running", running.len()));
        }
        if !running.is_empty() {
            let session = session_state(&profile, now_secs() as i64);
            if !session.is_valid() {
                notes.push(session.describe());
            }
            let bound = config.as_ref().and_then(|c| c.surfaces.get(SurfaceKind::ClaudeDesktop).account_id().map(str::to_string));
            if let Some(bound) = bound {
                let registry = Registry::load(env.paths)?;
                if registry.profiles.get(&bound).is_some_and(|e| !same_dir(&e.dir, &profile)) {
                    notes.push(format!("the running profile differs from the binding `{bound}`; use the menu bar Open Claude or `switch --restart`"));
                }
            }
        }
        Ok(Some(Identity {
            account_id: account.map(|a| a.id.clone()),
            email: account.and_then(|a| a.email.clone()),
            uuid: Some(uuid),
            note: (!notes.is_empty()).then(|| notes.join("; ")),
        }))
    }

    fn stage(&self, env: &Env, account: &Account) -> Result<Staged> {
        let registry = Registry::load(env.paths)?;
        let entry = registry.profiles.get(&account.id).ok_or_else(|| {
            Error::NotFound(format!("a Claude Desktop profile for `{}`; run `agent-switch add claude-desktop {}`", account.id, account.id))
        })?;
        check_profile(account, &entry.dir)?;
        if registry.link {
            let default = default_profile(env.paths);
            if link_state(&default)? == LinkState::Real {
                return Err(real_folder_error(&default));
            }
        }
        let dir = entry.dir.to_str().ok_or_else(|| Error::Invalid("the profile path is not valid UTF-8".into()))?;
        Ok(Staged::new(account.id.clone(), dir.as_bytes().to_vec()))
    }

    fn apply(&self, env: &Env, staged: Staged) -> Result<()> {
        let target = PathBuf::from(String::from_utf8(staged.payload).map_err(|_| Error::Invalid("the staged profile path is not valid UTF-8".into()))?);
        let default = default_profile(env.paths);
        let link = Registry::load(env.paths)?.link;
        self.reopen.replace(None);
        self.mirror_summary.replace(None);
        self.mirror_warning.replace(None);
        self.precheck_notes.replace(Vec::new());

        let running = self.running()?;
        let all_on_target = !running.is_empty() && running.iter().all(|i| same_dir(i.profile.as_deref().unwrap_or(&default), &target));
        let previous = running.first().and_then(|i| i.profile.clone()).unwrap_or_else(|| default.clone());

        if link && link_state(&default)? == LinkState::Real {
            return Err(real_folder_error(&default)); // before anything is quit
        }
        let mut quit_by_us = false;
        if !running.is_empty() && !all_on_target {
            if !self.restart_requested {
                return Err(Error::Invalid(format!(
                    "Claude is running ({}). Switching needs every instance closed; run \
                     `agent-switch switch claude-desktop {} --restart` to let agent-switch quit and reopen it",
                    running_summary(&running, &default),
                    staged.account_id
                )));
            }
            let (busy, notes) = self.busy_scan(&running, &default);
            if !busy.is_empty() {
                if !self.force {
                    return Err(Error::Invalid(Self::busy_refusal(&busy, &notes, false)));
                }
                if let Some(offered) = &self.force_only {
                    if !session_mirror::titles_within(&busy, offered) {
                        return Err(Error::Invalid(Self::busy_refusal(&busy, &notes, true)));
                    }
                }
            }
            self.precheck_notes.replace(notes);
            let claude_code = |procs: &[ProcInfo]| claude_code_pids(procs, &known_profile_dirs(env.paths));
            match quit_all_instances_with(self.sys.as_ref(), &CLAUDE, &self.quit, self.reask, self.force, &claude_code) {
                Ok(_) => {}
                Err(Error::QuitVetoed { exited, .. }) => {
                    // Claude asked its own "still working" question and the quit was cancelled. Bring back
                    // any instance that had already left so "Nothing was quit" stays true.
                    let mut text = self.veto_refusal(&running, &default);
                    for instance in running.iter().filter(|i| exited.contains(&i.pid)) {
                        let dir = canon(instance.profile.as_deref().unwrap_or(&default));
                        if let Err(e) = self.open_profile(&dir) {
                            text.push_str(&format!(" Could not reopen Claude on {}: {e}; open it yourself.", dir.display()));
                        }
                    }
                    return Err(Error::Invalid(text));
                }
                Err(e) => return Err(e),
            }
            quit_by_us = true;
        }
        if !all_on_target {
            self.mirror_sessions(env.paths);
        }

        if link {
            if let Err(e) = swap_symlink(&default, &target) {
                if quit_by_us {
                    let _ = self.open_profile(&canon(&previous)); // bring the app back where it was
                }
                return Err(e);
            }
        }
        if self.restart_requested && !all_on_target {
            self.reopen.replace(Some((staged.account_id, target)));
        }
        Ok(())
    }

    fn is_running(&self, _env: &Env) -> Result<Option<bool>> {
        Ok(Some(!self.running()?.is_empty()))
    }

    fn restart(&self, env: &Env) -> Result<()> {
        let Some((account_id, dir)) = self.reopen.take() else { return Ok(()) };
        self.open_profile(&dir)?;
        self.wait_for_instance_on(&dir, &default_profile(env.paths))?;
        std::thread::sleep(self.settle);
        match session_state(&dir, now_secs() as i64) {
            Session::Valid => Ok(()),
            other => Err(Error::Invalid(format!(
                "Claude reopened on the profile of `{account_id}`, but it is not signed in: {}. \
                 Sign in in the window that opened (the profile keeps the login afterwards)",
                other.describe()
            ))),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------------------------

fn check(level: Level, detail: impl Into<String>) -> Check {
    Check { level, name: "claude desktop", detail: detail.into() }
}

/// Whether session mirroring is on, off by the user, or off for an untested version; plus what it skips.
fn mirror_checks(env: &Env, out: &mut Vec<Check>) {
    let installed = macos_apps::app_version(CLAUDE.bundle_path);
    let file = session_mirror::Manifest::file(env.paths);
    let manifest = match session_mirror::Manifest::load(&file) {
        Ok(m) => m,
        Err(e) => {
            out.push(check(Level::Warn, format!("session mirror: cannot read its manifest ({e}); switches will not mirror until it is fixed")));
            return;
        }
    };
    if !manifest.enabled {
        out.push(check(Level::Warn, "session mirror off (disabled by you; run `agent-switch desktop-sessions on`)"));
        return;
    }
    if installed.as_deref() != Some(TESTED_CLAUDE_DESKTOP) {
        let found = installed.map_or_else(|| "not installed".to_string(), |v| format!("{v} untested"));
        out.push(check(Level::Warn, format!("session mirror off (Claude {found}; tested with {TESTED_CLAUDE_DESKTOP})")));
        return;
    }
    if file.exists() {
        let tracked = manifest.sessions.values().filter(|e| !e.deleted).count();
        out.push(check(Level::Ok, format!("session mirror on ({tracked} sessions tracked)")));
    } else {
        out.push(check(Level::Ok, "session mirror on (nothing tracked yet; the first switch starts it)"));
    }
    match session_mirror::skipped_notes(env.paths) {
        Ok(notes) => out.extend(notes.into_iter().map(|n| check(Level::Warn, format!("session mirror skips {n}")))),
        Err(e) => out.push(check(Level::Warn, format!("session mirror: cannot scan the profiles ({e})"))),
    }
}

pub fn doctor_checks(env: &Env) -> Vec<Check> {
    let mut out = Vec::new();
    match macos_apps::app_version(CLAUDE.bundle_path) {
        Some(v) if v == TESTED_CLAUDE_DESKTOP => out.push(check(Level::Ok, format!("Claude {v}"))),
        Some(v) => out.push(check(Level::Warn, format!("Claude {v} (tested with {TESTED_CLAUDE_DESKTOP}); re-test a switch after an update"))),
        None => out.push(check(Level::Warn, format!("{} not found", CLAUDE.bundle_path))),
    }

    mirror_checks(env, &mut out);

    let registry = match Registry::load(env.paths) {
        Ok(r) => r,
        Err(e) => {
            out.push(check(Level::Fail, e.to_string()));
            return out;
        }
    };
    let config = Config::load(env.paths).ok();
    if registry.profiles.is_empty() {
        out.push(check(Level::Ok, "no profiles registered; `agent-switch add claude-desktop <account>`"));
    }
    for (id, entry) in &registry.profiles {
        let account = config.as_ref().and_then(|c| c.account(id));
        let protected = if entry.protected { " [protected original data]" } else { "" };
        let line = match account {
            None => (Level::Warn, format!("profile `{id}` has no matching account in the config")),
            Some(a) => match check_profile(a, &entry.dir) {
                Ok(Session::Valid) => (Level::Ok, format!("profile `{id}`: signed in{protected}")),
                Ok(other) => (Level::Warn, format!("profile `{id}`: {}{protected}", other.describe())),
                Err(e) => (Level::Warn, format!("profile `{id}`: {e}")),
            },
        };
        out.push(check(line.0, line.1));
    }

    let default = default_profile(env.paths);
    match (registry.link, link_state(&default)) {
        (true, Ok(LinkState::Symlink(to))) => {
            let known = registry.profiles.values().any(|e| same_dir(&e.dir, &to));
            out.push(check(
                if known { Level::Ok } else { Level::Warn },
                format!("the default folder is a link to {}{}", to.display(), if known { "" } else { " (not a registered profile)" }),
            ));
        }
        (true, Ok(_)) => out.push(check(Level::Fail, format!("the default-folder link is enabled but {} is not a symlink; switching refuses until it is fixed", default.display()))),
        (false, Ok(LinkState::Symlink(to))) => out.push(check(Level::Warn, format!("{} is a symlink to {} that agent-switch does not manage", default.display(), to.display()))),
        (false, Ok(_)) => out.push(check(Level::Ok, "the default folder is a real folder (launcher mode: use Open Claude or `switch --restart`)")),
        (_, Err(e)) => out.push(check(Level::Warn, e.to_string())),
    }
    if let Ok(procs) = SystemApps.processes() {
        let pids = bundle_pids(&procs, &CLAUDE);
        if !pids.is_empty() && instances(&procs, &CLAUDE).len() > 1 {
            out.push(check(Level::Warn, format!("several Claude instances are running (pids {})", join_pids(&pids))));
        }
    }
    out
}
