//! Creating, registering, linking and removing Claude Desktop profiles.
//!
//! The app's own folder (`~/Library/Application Support/Claude`) holds the personal account's data. No
//! function here deletes or rewrites it. Registering it records its path (protected); the optional link
//! step backs it up first, verifies the backup, moves it into the managed folder and leaves a symlink.

use std::time::{Duration, Instant};

use super::*;
use crate::apfs_clone::{clone_runtime, clone_tree_verified, same_filesystem, tree_stats, RuntimeReport, TreeStats};
use crate::config::Provider;
use crate::macos_apps::ProcInfo;

// ---------------------------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------------------------

pub struct AddRequest<'a> {
    pub account: &'a str,
    /// Register this existing profile folder instead of creating a new one.
    pub from_profile: Option<&'a Path>,
    pub dry_run: bool,
}

#[derive(Debug)]
pub struct AddReport {
    pub account_id: String,
    pub dir: PathBuf,
    pub dry_run: bool,
    pub already_registered: bool,
    /// The folder was registered where it is (the app's own profile), not copied.
    pub in_place: bool,
    pub cloned_from: Option<PathBuf>,
    pub clone_stats: Option<TreeStats>,
    pub config_copied: bool,
    /// `server.KEY` names from the copied MCP config. Values are never read out.
    pub mcp_env_keys: Vec<String>,
    pub runtime: Option<RuntimeReport>,
    pub notes: Vec<String>,
    /// Claude was opened on the new profile and waits for a sign-in.
    pub opened_for_sign_in: bool,
    pub session: Option<Session>,
}

fn running_error(sys: &dyn System, paths: &Paths) -> Result<Option<Error>> {
    let procs = sys.processes()?;
    let pids = bundle_pids(&procs, &CLAUDE);
    if pids.is_empty() {
        return Ok(None);
    }
    let running = instances(&procs, &CLAUDE);
    let who = if running.is_empty() { format!("pids {}", join_pids(&pids)) } else { running_summary(&running, &default_profile(paths)) };
    Ok(Some(Error::Invalid(format!(
        "Claude is running ({who}). Quit every instance first: the sign-in link is handed to a running copy, and a profile cannot be cloned while it is open"
    ))))
}

/// Names of the `env` variables of each MCP server in a `claude_desktop_config.json`.
pub fn mcp_env_keys(bytes: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else { return Vec::new() };
    let Some(servers) = value.get("mcpServers").and_then(|v| v.as_object()) else { return Vec::new() };
    servers
        .iter()
        .flat_map(|(name, server)| {
            server
                .get("env")
                .and_then(|e| e.as_object())
                .into_iter()
                .flat_map(move |env| env.keys().map(move |k| format!("{name}.{k}")))
        })
        .collect()
}

/// Copy the MCP servers and preferences as they are, at mode 0600 (it can hold server credentials).
fn copy_desktop_config(from_profile: &Path, to_profile: &Path) -> Result<Option<Vec<String>>> {
    let src = from_profile.join("claude_desktop_config.json");
    let bytes = match fs::read(&src) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(format!("read {}", src.display()), e)),
    };
    safe_write(&to_profile.join("claude_desktop_config.json"), &bytes, &WriteOpts::secret())?;
    Ok(Some(mcp_env_keys(&bytes)))
}

pub(super) fn register(paths: &Paths, id: &str, entry: ProfileEntry) -> Result<()> {
    Registry::update(paths, |r| {
        if let Some((other, _)) = r.entry_for_dir(&entry.dir) {
            if other != id {
                return Err(Error::Invalid(format!("the folder {} is already the profile of `{other}`", entry.dir.display())));
            }
        }
        r.profiles.insert(id.to_string(), entry);
        Ok(())
    })
}

/// Create or register the Claude Desktop profile of `req.account`. Idempotent: an account that already has
/// a profile is reported, never touched, and a protected profile is never replaced.
pub fn add_profile(env: &Env, sys: &dyn System, req: &AddRequest) -> Result<AddReport> {
    let config = Config::load(env.paths)?;
    let account = config.account(req.account).ok_or_else(|| Error::NotFound(format!("account `{}`", req.account)))?;
    if account.provider != Provider::Claude {
        return Err(Error::Invalid(format!("`{}` is a {} account; Claude Desktop needs a claude account", account.id, account.provider)));
    }
    let default = default_profile(env.paths);
    let mut report = AddReport {
        account_id: account.id.clone(),
        dir: PathBuf::new(),
        dry_run: req.dry_run,
        already_registered: false,
        in_place: false,
        cloned_from: None,
        clone_stats: None,
        config_copied: false,
        mcp_env_keys: Vec::new(),
        runtime: None,
        notes: Vec::new(),
        opened_for_sign_in: false,
        session: None,
    };

    let registry = Registry::load(env.paths)?;
    if let Some(entry) = registry.profiles.get(&account.id) {
        let same_request = req.from_profile.is_none_or(|src| same_dir(src, &entry.dir) || entry.from.as_deref().is_some_and(|f| same_dir(f, src)));
        if !same_request {
            return Err(Error::Invalid(format!(
                "`{}` already has the profile {}{}; agent-switch does not replace it",
                account.id,
                entry.dir.display(),
                if entry.protected { " (protected: original Claude data)" } else { "" }
            )));
        }
        report.already_registered = true;
        report.dir = entry.dir.clone();
        report.session = check_profile(account, &entry.dir).ok();
        return Ok(report);
    }

    match req.from_profile {
        Some(src) if same_dir(src, &default) => register_in_place(env, account, &default, req.dry_run, &mut report)?,
        Some(src) => clone_existing(env, sys, account, src, req.dry_run, &mut report)?,
        None => create_new(env, sys, account, &default, req.dry_run, &mut report)?,
    }
    Ok(report)
}

/// The app's own profile: record where it is. Nothing is copied, moved or rewritten.
fn register_in_place(env: &Env, account: &Account, default: &Path, dry_run: bool, report: &mut AddReport) -> Result<()> {
    let dir = canon(default);
    report.session = Some(check_profile(account, &dir)?);
    report.dir = dir.clone();
    report.in_place = true;
    if !dry_run {
        register(env.paths, &account.id, ProfileEntry { dir, protected: true, from: None })?;
    }
    Ok(())
}

/// A profile that is already signed in elsewhere (for example `~/.agent-switch/tmp/desktop-b`): clone the
/// whole folder into the managed location. The original is left in place.
fn clone_existing(env: &Env, sys: &dyn System, account: &Account, src: &Path, dry_run: bool, report: &mut AddReport) -> Result<()> {
    let src = canon(src);
    report.session = Some(check_profile(account, &src)?);
    let target = profiles_root(env.paths).join(&account.id);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(Error::Invalid(format!("{} already exists but is not registered; move it away first", target.display())));
    }
    if !same_filesystem(&src, &target)? {
        return Err(Error::Invalid(format!(
            "{} and the managed folder are on different disks; a clone would be a full copy. Move the profile onto the same disk first",
            src.display()
        )));
    }
    report.dir = target.clone();
    report.cloned_from = Some(src.clone());
    if let Some(e) = running_error(sys, env.paths)? {
        if dry_run {
            report.notes.push(format!("would refuse: {e}"));
            return Ok(());
        }
        return Err(e);
    }
    if dry_run {
        report.clone_stats = Some(tree_stats(&src)?);
        return Ok(());
    }
    ensure_private_dir(&profiles_root(env.paths))?;
    report.clone_stats = Some(clone_tree_verified(&src, &target)?);
    if let Err(e) = register(env.paths, &account.id, ProfileEntry { dir: target.clone(), protected: false, from: Some(src) }) {
        let _ = fs::remove_dir_all(&target); // we created it a moment ago
        return Err(e);
    }
    Ok(())
}

/// A fresh profile: private folder, the MCP config copied as is, the runtime cloned, then Claude opened on it
/// for the sign-in.
fn create_new(env: &Env, sys: &dyn System, account: &Account, default: &Path, dry_run: bool, report: &mut AddReport) -> Result<()> {
    if account.uuid.is_some() && profile_uuid(default).ok().flatten() == account.uuid {
        return Err(Error::Invalid(format!(
            "the app's own profile {} already holds `{}`; register it instead: \
             `agent-switch add claude-desktop {} --from-profile \"{}\"`",
            default.display(),
            account.id,
            account.id,
            default.display()
        )));
    }
    let target = profiles_root(env.paths).join(&account.id);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(Error::Invalid(format!("{} already exists but is not registered; move it away first", target.display())));
    }
    report.dir = target.clone();
    if let Some(e) = running_error(sys, env.paths)? {
        if dry_run {
            report.notes.push(format!("would refuse: {e}"));
        } else {
            return Err(e);
        }
    }
    if dry_run {
        report.notes.push("would create the profile, copy claude_desktop_config.json, clone the runtime, then open Claude to sign in".into());
        return Ok(());
    }

    ensure_private_dir(&target)?;
    let source = canon(default);
    let built = (|| -> Result<()> {
        if let Some(keys) = copy_desktop_config(&source, &target)? {
            report.config_copied = true;
            report.mcp_env_keys = keys;
        }
        match clone_runtime(&source, &target) {
            Ok(r) => report.runtime = Some(r),
            // A runtime on another disk is not worth a full copy; the app downloads its own.
            Err(e @ Error::Invalid(_)) => report.notes.push(e.to_string()),
            Err(e) => return Err(e),
        }
        register(env.paths, &account.id, ProfileEntry { dir: target.clone(), protected: false, from: None })
    })();
    if let Err(e) = built {
        let _ = fs::remove_dir_all(&target); // created above, holds nothing of the user's
        return Err(e);
    }
    open_profile(sys, &target)?;
    report.opened_for_sign_in = true;
    Ok(())
}

/// Wait until the profile at `dir` shows the account's uuid and an unexpired session cookie. Fails at once
/// when the profile signs in as a different account.
pub fn confirm_sign_in(account: &Account, dir: &Path, timeout: Duration, poll: Duration) -> Result<Session> {
    let deadline = Instant::now() + timeout;
    loop {
        // A half-written config.json is a read error for a moment; keep waiting.
        if profile_uuid(dir).ok().flatten().is_some() {
            let session = check_profile(account, dir)?;
            if session.is_valid() {
                return Ok(session);
            }
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid(format!(
                "sign-in on {} was not confirmed within {}s (needs the account's uuid and an unexpired claude.ai session cookie). \
                 Finish signing in, then run `agent-switch doctor`",
                dir.display(),
                timeout.as_secs()
            )));
        }
        std::thread::sleep(poll);
    }
}

// ---------------------------------------------------------------------------------------------
// the default-folder link
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct LinkReport {
    pub changed: bool,
    pub dry_run: bool,
    pub default: PathBuf,
    pub target: PathBuf,
    pub backup: Option<PathBuf>,
    pub stats: Option<TreeStats>,
    pub notes: Vec<String>,
}

/// `YYYYMMDD-HHMMSS` in UTC.
pub fn stamp(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let rem = unix % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rem / 3_600, rem % 3_600 / 60, rem % 60)
}

/// Processes other than the app's bundle that run from inside `dir` (the embedded Claude Code sessions).
fn users_of(procs: &[ProcInfo], dir: &Path) -> Vec<u32> {
    let needle = format!("{}/", dir.display());
    procs.iter().filter(|p| p.command.contains(&needle)).map(|p| p.pid).collect()
}

/// Quit Claude and wait for every process that still runs from `dir`. Moving a folder under a live process
/// would split its writes between two places.
fn quiesce(sys: &dyn System, dir: &Path, quit: &QuitOpts, reask: Duration) -> Result<()> {
    quit_all_instances(sys, &CLAUDE, quit, reask, false)?;
    let deadline = Instant::now() + quit.timeout;
    loop {
        let users = users_of(&sys.processes()?, dir);
        if users.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid(format!(
                "processes still run from {} (pids {}); stop them and run the command again. Nothing was changed",
                dir.display(),
                join_pids(&users)
            )));
        }
        std::thread::sleep(quit.poll);
    }
}

/// Turn the default folder into a link to the active profile. In order: quit every instance and wait for
/// the processes that run from the folder; clone the folder to `backups/claude-default-<date>/` and verify
/// file count and logical size; move the real folder to `claude-desktop/<account>/` (protected); create the
/// symlink. Any failure puts the real folder back.
pub fn enable_default_link(env: &Env, sys: &dyn System, quit: &QuitOpts, reask: Duration, dry_run: bool) -> Result<LinkReport> {
    let default = default_profile(env.paths);
    let registry = Registry::load(env.paths)?;
    let mut report = LinkReport { changed: false, dry_run, default: default.clone(), target: PathBuf::new(), backup: None, stats: None, notes: Vec::new() };
    match link_state(&default)? {
        LinkState::Symlink(to) if registry.link => {
            report.target = to;
            report.notes.push("the default folder is already a managed link".into());
            return Ok(report);
        }
        LinkState::Symlink(to) => {
            return Err(Error::Invalid(format!("{} is already a symlink to {} that agent-switch did not create; not touching it", default.display(), to.display())))
        }
        LinkState::Missing => return Err(Error::Invalid(format!("{} does not exist; there is nothing to move", default.display()))),
        LinkState::Real => {}
    }
    let (id, _) = registry.entry_for_dir(&default).ok_or_else(|| {
        Error::Invalid(format!(
            "{} is not registered as a profile. Register it first: `agent-switch add claude-desktop <account> --from-profile \"{}\"`",
            default.display(),
            default.display()
        ))
    })?;
    let id = id.clone();
    let target = profiles_root(env.paths).join(&id);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(Error::Invalid(format!("{} already exists; refusing to move the default folder onto it", target.display())));
    }
    ensure_private_dir(&backups_dir(env.paths))?;
    if !same_filesystem(&default, &target)? || !same_filesystem(&default, &backups_dir(env.paths))? {
        return Err(Error::Invalid("the default folder, the managed folder and the backup folder must be on one disk (clones and the move need it)".into()));
    }
    let backup = backups_dir(env.paths).join(format!("claude-default-{}", stamp(now_secs())));
    report.target = target.clone();
    report.backup = Some(backup.clone());
    if dry_run {
        report.stats = Some(tree_stats(&default)?);
        return Ok(report);
    }

    quiesce(sys, &default, quit, reask)?;
    let stats = clone_tree_verified(&default, &backup)?;
    report.stats = Some(stats);

    ensure_private_dir(&profiles_root(env.paths))?;
    fs::rename(&default, &target).map_err(|e| Error::io(format!("move {} to {}", default.display(), target.display()), e))?;
    let put_back = |why: Error| -> Error {
        let _ = fs::remove_file(&default); // only ever our own link
        match fs::rename(&target, &default) {
            Ok(()) => Error::Invalid(format!("{why}. The real folder is back at {}; the backup is {}", default.display(), backup.display())),
            Err(e) => Error::Invalid(format!(
                "{why}. Restoring failed too ({e}): the real folder is at {} and a verified backup is at {}. Move it back by hand",
                target.display(),
                backup.display()
            )),
        }
    };
    if let Err(e) = swap_symlink(&default, &target) {
        return Err(put_back(e));
    }
    match tree_stats(&target) {
        Ok(after) if after == stats => {}
        Ok(after) => {
            return Err(put_back(Error::Invalid(format!(
                "the moved folder holds {} files / {} bytes but the backup holds {} / {}",
                after.files, after.bytes, stats.files, stats.bytes
            ))))
        }
        Err(e) => return Err(put_back(e)),
    }
    let saved = Registry::update(env.paths, |r| {
        let entry = r.profiles.get_mut(&id).ok_or_else(|| Error::NotFound(format!("registry entry `{id}`")))?;
        entry.dir = target.clone();
        entry.protected = true;
        r.link = true;
        Ok(())
    });
    if let Err(e) = saved {
        return Err(put_back(e));
    }
    report.changed = true;
    Ok(report)
}

/// Undo `enable_default_link`: remove the link and move the protected folder back to the default path.
pub fn disable_default_link(env: &Env, sys: &dyn System, quit: &QuitOpts, reask: Duration, dry_run: bool) -> Result<LinkReport> {
    let default = default_profile(env.paths);
    let registry = Registry::load(env.paths)?;
    let mut report = LinkReport { changed: false, dry_run, default: default.clone(), target: PathBuf::new(), backup: None, stats: None, notes: Vec::new() };
    let link_target = match link_state(&default)? {
        LinkState::Symlink(to) if registry.link => to,
        LinkState::Symlink(_) => return Err(Error::Invalid(format!("{} is a symlink that agent-switch did not create; not touching it", default.display()))),
        LinkState::Real | LinkState::Missing => {
            report.notes.push("the default folder is not a managed link; nothing to do".into());
            return Ok(report);
        }
    };
    let protected: Vec<(&String, &ProfileEntry)> = registry.profiles.iter().filter(|(_, e)| e.protected).collect();
    let [(id, original)] = protected.as_slice() else {
        return Err(Error::Invalid(format!("expected exactly one protected profile to move back, found {}", protected.len())));
    };
    let (id, original_dir) = ((*id).clone(), original.dir.clone());
    if !original_dir.is_dir() {
        return Err(Error::Invalid(format!("the protected folder {} is missing; leaving the link alone", original_dir.display())));
    }
    report.target = original_dir.clone();
    if dry_run {
        return Ok(report);
    }

    quiesce(sys, &default, quit, reask)?;
    quiesce(sys, &original_dir, quit, reask)?;
    fs::remove_file(&default).map_err(|e| Error::io(format!("remove link {}", default.display()), e))?;
    if let Err(e) = fs::rename(&original_dir, &default) {
        // Put the link back so the app still finds a profile.
        let _ = std::os::unix::fs::symlink(&link_target, &default);
        return Err(Error::io(format!("move {} back to {}", original_dir.display(), default.display()), e));
    }
    let saved = Registry::update(env.paths, |r| {
        let entry = r.profiles.get_mut(&id).ok_or_else(|| Error::NotFound(format!("registry entry `{id}`")))?;
        entry.dir = default.clone();
        entry.protected = true;
        r.link = false;
        Ok(())
    });
    if let Err(e) = saved {
        return Err(Error::Invalid(format!(
            "the folder is back at {} but the registry could not be updated: {e}. Set `link` to false and the dir of `{id}` to that path in claude-desktop/profiles.json",
            default.display()
        )));
    }
    report.changed = true;
    Ok(report)
}

// ---------------------------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------------------------

/// Run before `remove` deletes an account: refuse while the profile is protected or in use. Returns the
/// entry whose folder `finish_remove` deletes afterwards.
pub fn plan_remove(paths: &Paths, sys: &dyn System, id: &str) -> Result<Option<ProfileEntry>> {
    let registry = Registry::load(paths)?;
    let Some(entry) = registry.profiles.get(id) else { return Ok(None) };
    if entry.protected {
        return Err(Error::Invalid(format!(
            "`{id}` owns the protected Claude Desktop folder {} (the original Claude data). agent-switch never deletes it; \
             leave the account in place, or remove the folder yourself after checking the backup",
            entry.dir.display()
        )));
    }
    let default = default_profile(paths);
    if let Ok(LinkState::Symlink(to)) = link_state(&default) {
        if same_dir(&to, &entry.dir) {
            return Err(Error::Invalid(format!(
                "the Claude default folder {} currently points at the profile of `{id}`. Deleting it would leave the link dangling. Switch Claude Desktop to another account first, then remove `{id}`",
                default.display()
            )));
        }
    }
    let busy = instances(&sys.processes()?, &CLAUDE).iter().any(|i| same_dir(i.profile.as_deref().unwrap_or(&default), &entry.dir));
    if busy {
        return Err(Error::Invalid(format!("Claude is running on the profile of `{id}`; quit it before removing the account")));
    }
    Ok(Some(entry.clone()))
}

/// Delete a managed, unprotected profile folder and its registry entry. The folder must sit directly inside
/// the managed folder and must not be a symlink.
pub fn finish_remove(paths: &Paths, id: &str, entry: &ProfileEntry) -> Result<()> {
    let root = canon(&profiles_root(paths));
    let managed = entry.dir.parent().is_some_and(|p| canon(p) == root);
    let real = fs::symlink_metadata(&entry.dir).map(|m| m.is_dir() && !m.file_type().is_symlink());
    if entry.protected || !managed {
        return Err(Error::Invalid(format!("refusing to delete {}: it is protected or outside the managed folder", entry.dir.display())));
    }
    match real {
        Ok(true) => fs::remove_dir_all(&entry.dir).map_err(|e| Error::io(format!("delete {}", entry.dir.display()), e))?,
        Ok(false) => return Err(Error::Invalid(format!("refusing to delete {}: not a plain folder", entry.dir.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(format!("stat {}", entry.dir.display()), e)),
    }
    Registry::update(paths, |r| {
        r.profiles.remove(id);
        Ok(())
    })
}
