//! Login handling shared by the two OpenAI surfaces (the ChatGPT app and the Codex CLI).
//!
//! The ChatGPT app and the CLI read `auth.json` from a Codex home: the app always uses `~/.codex`, the CLI
//! uses `~/.codex` or, when it has its own account, the shallow home (`shallow_home.rs`). Codex rotates
//! the refresh token whenever it refreshes, so every operation here follows two rules:
//!
//! * before a login is written anywhere, every live copy of it is saved back to the vault (newest
//!   `last_refresh` wins), and
//! * a login never lives in two homes at once, because two holders of one token chain invalidate each other.
//!
//! `auth.lock` (the file Codex locks with `flock` around every read-modify-write of `auth.json`, see
//! openai/codex `codex-rs/login/src/auth/storage.rs`, `lock_for_write`) is taken with the same `flock`
//! before every write, so a write cannot interleave with a Codex refresh that is committing.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Account, Config, Provider};
use crate::doctor::{Check, Level};
use crate::error::{Error, Result};
use crate::fsutil::{ensure_private_dir, safe_write, WriteOpts};
use crate::locks::{AccountLock, FlockGuard};
use crate::macos_apps::{bundle_pids, join_pids, ProcInfo, System, CHATGPT};
use crate::paths::Paths;
use crate::providers::openai::Login;
use crate::shallow_home::{self, primary_dir, shallow_dir, Drift, RepairReport};
use crate::state::now_secs;
use crate::surface::{Env, Identity};
use crate::vault::{normalize_blob, Slot, Vault};

pub const STORE: &str = "openai-auth";
pub const AUTH_FILE: &str = "auth.json";
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const PIN_KEY: &str = "cli_auth_credentials_store";

/// Versions the behavior was checked against (2026-11-06). `doctor` warns when the installed ones differ.
pub const TESTED_CHATGPT_APP: &str = "26.1103.31037";
pub const TESTED_BUNDLED_CODEX: &str = "0.167.0-alpha.3.2";
pub const TESTED_CODEX_CLI: &str = "0.160.0";

pub struct Homes {
    pub primary: PathBuf,
    pub shallow: PathBuf,
}

impl Homes {
    pub fn new(paths: &Paths) -> Homes {
        Homes { primary: primary_dir(paths), shallow: shallow_dir(paths) }
    }
}

/// One copy of a login: where it was found, its text and whose it is. `Debug` never shows the text.
#[derive(Clone)]
pub struct Copy {
    pub source: String,
    pub text: String,
    pub login: Login,
}

impl fmt::Debug for Copy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Copy").field("source", &self.source).field("login", &self.login).field("text", &"<redacted>").finish()
    }
}

impl Copy {
    pub fn parse(source: impl Into<String>, text: String) -> Result<Copy> {
        let login = Login::parse(&text)?;
        Ok(Copy { source: source.into(), text, login })
    }

    /// The saved form: compact printable-ASCII JSON of the whole file, so a write-back is byte-compatible.
    pub fn slot(&self) -> Result<Slot> {
        let refreshed_at = self.login.last_refresh_us.map(|us| (us / 1_000_000).max(0) as u64).unwrap_or_else(now_secs);
        Ok(Slot { blob: normalize_blob(&self.text)?, fingerprint: self.login.fingerprint.clone(), refreshed_at })
    }

    fn last_refresh(&self) -> i64 {
        self.login.last_refresh_us.unwrap_or(i64::MIN)
    }
}

/// The copy with the newest `last_refresh`; the first one wins a tie.
pub fn newest(copies: &[Copy]) -> Option<&Copy> {
    copies.iter().fold(None, |best: Option<&Copy>, c| match best {
        Some(b) if b.last_refresh() >= c.last_refresh() => Some(b),
        _ => Some(c),
    })
}

/// Read `<dir>/auth.json`. `Ok(None)` when the file does not exist.
pub fn read_copy(dir: &Path, source: &str) -> Result<Option<Copy>> {
    let file = dir.join(AUTH_FILE);
    match std::fs::read_to_string(&file) {
        Ok(text) => Copy::parse(source, text).map(Some).map_err(|e| Error::Invalid(format!("{}: {e}", file.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(format!("read {}", file.display()), e)),
    }
}

/// Write `<dir>/auth.json` (mode 0600) while holding Codex's own `auth.lock`. A symlinked `auth.json` is
/// refused: it belongs to the user's dotfile manager.
pub fn write_auth(dir: &Path, text: &str) -> Result<()> {
    write_auth_with(dir, text, WriteOpts::secret())
}

/// Like `write_auth` for the shallow home, where `auth.json` is a per-home file. A link left there from the
/// shared state (it points into `~/.codex`) is replaced by this home's own copy.
pub fn write_shallow_auth(dir: &Path, text: &str) -> Result<()> {
    write_auth_with(dir, text, WriteOpts::secret().replace_symlink())
}

fn write_auth_with(dir: &Path, text: &str, opts: WriteOpts) -> Result<()> {
    Login::parse(text)?; // never write something Codex could not read
    let _lock = FlockGuard::lock(&dir.join("auth.lock"), LOCK_TIMEOUT)?;
    safe_write(&dir.join(AUTH_FILE), text.as_bytes(), &opts)
}

/// The live copies on disk. `shallow` is `None` when it is absent or is a link to the primary file.
pub struct Live {
    pub primary: Option<Copy>,
    pub shallow: Option<Copy>,
}

pub fn read_live(homes: &Homes) -> Result<Live> {
    let primary = read_copy(&homes.primary, "~/.codex")?;
    let same_file = std::fs::canonicalize(homes.shallow.join(AUTH_FILE)).ok().is_some_and(|s| Some(s) == std::fs::canonicalize(homes.primary.join(AUTH_FILE)).ok());
    let shallow = if same_file { None } else { read_copy(&homes.shallow, "the shallow home")? };
    Ok(Live { primary, shallow })
}

pub fn find_account<'a>(config: &'a Config, login: &Login) -> Option<&'a Account> {
    config.accounts.iter().find(|a| a.provider == Provider::Openai && login.is_account(a))
}

/// Save `copy` into its account's vault slot when it is newer than the slot. Returns whether it wrote.
/// The caller holds the account lock.
pub fn save_if_newer(vault: &Vault, account: &Account, copy: &Copy) -> Result<bool> {
    if let Some(existing) = vault.get(&account.id, STORE)? {
        if let Ok(old) = Login::parse(&existing.blob) {
            if old.last_refresh_us.unwrap_or(i64::MIN) >= copy.last_refresh() {
                return Ok(false);
            }
        }
    }
    vault.put(&account.id, STORE, &copy.slot()?)?;
    Ok(true)
}

#[derive(Debug, Default)]
pub struct SaveBack {
    /// Account ids whose vault slot was updated from a live copy.
    pub saved: Vec<String>,
    /// Sources of copies that belong to no saved account.
    pub unknown: Vec<String>,
}

/// Save live copies back to the vault. `held` are the accounts the caller already locked; any other
/// account gets its lock taken here, so a refresh elsewhere cannot interleave.
pub fn save_back(env: &Env, config: &Config, held: &[&str], copies: &[&Copy]) -> Result<SaveBack> {
    let mut out = SaveBack::default();
    for copy in copies {
        let Some(account) = find_account(config, &copy.login) else {
            out.unknown.push(copy.source.clone());
            continue;
        };
        let _lock = if held.contains(&account.id.as_str()) { None } else { Some(AccountLock::acquire(env.paths, &account.id, LOCK_TIMEOUT)?) };
        if save_if_newer(env.vault, account, copy)? {
            out.saved.push(account.id.clone());
        }
    }
    Ok(out)
}

/// The vault copy of `account` as a `Copy`, checked to be that account's login.
pub fn vault_copy(vault: &Vault, account: &Account) -> Result<Copy> {
    let slot = vault
        .get(&account.id, STORE)?
        .ok_or_else(|| Error::NotFound(format!("saved login for `{}`; run `agent-switch add openai`", account.id)))?;
    let copy = Copy::parse(format!("the saved login of `{}`", account.id), slot.blob)?;
    if !copy.login.is_account(account) {
        return Err(Error::Invalid(format!("the saved login of `{}` belongs to a different ChatGPT account; remove and add it again", account.id)));
    }
    Ok(copy)
}

/// Where the Codex CLI reads its login: its own shallow home when it is bound to an account that the
/// ChatGPT app is not bound to, otherwise `~/.codex`.
pub fn cli_uses_shallow(config: &Config) -> bool {
    match config.surfaces.codex_cli.account_id() {
        None => false,
        Some(cli) => config.surfaces.chatgpt_desktop.account_id() != Some(cli),
    }
}

/// Like `cli_uses_shallow`, and also true while a split is pending: the CLI temporarily moved to its own
/// home with another account and rejoins `~/.codex` when the app moves to that account.
pub fn cli_separate(paths: &Paths, config: &Config) -> bool {
    cli_uses_shallow(config) || crate::state::State::load(paths).is_ok_and(|s| s.codex_split())
}

/// What `status` shows for a live copy.
pub fn identity(copy: &Copy, config: Option<&Config>, note: Option<&str>) -> Identity {
    Identity {
        account_id: config.and_then(|c| find_account(c, &copy.login)).map(|a| a.id.clone()),
        email: copy.login.email.clone(),
        uuid: Some(copy.login.account_id.clone()),
        note: note.map(str::to_string),
    }
}

/// The account `id` must be an OpenAI account of this config.
pub fn openai_account<'a>(config: &'a Config, id: &str) -> Result<&'a Account> {
    config
        .account(id)
        .filter(|a| a.provider == Provider::Openai)
        .ok_or_else(|| Error::NotFound(format!("OpenAI account `{id}`")))
}

// ---------------------------------------------------------------------------------------------
// config.toml pin and backups
// ---------------------------------------------------------------------------------------------

/// Copy `file` to `~/Library/Application Support/agent-switch/backups/<label>-<unix seconds>` (mode 0600).
pub fn backup_file(paths: &Paths, file: &Path, label: &str) -> Result<PathBuf> {
    let dir = paths.support_dir().join("backups");
    ensure_private_dir(&dir)?;
    let bytes = std::fs::read(file).map_err(|e| Error::io(format!("read {}", file.display()), e))?;
    let dest = dir.join(format!("{label}-{}", now_secs()));
    safe_write(&dest, &bytes, &WriteOpts::secret())?;
    Ok(dest)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Pin {
    Already,
    Pinned { backup: Option<PathBuf> },
}

/// Read `<home>/config.toml`: its text (`None` when the file does not exist) and whether it already pins the
/// file store. A different explicit value is the user's choice, so it is an error rather than a rewrite.
fn read_pin(home: &Path) -> Result<(Option<String>, bool)> {
    let file = home.join("config.toml");
    let existing = match std::fs::read_to_string(&file) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(Error::io(format!("read {}", file.display()), e)),
    };
    let table: toml::Table = toml::from_str(existing.as_deref().unwrap_or("")).map_err(|e| Error::Config(format!("{}: {e}", file.display())))?;
    match table.get(PIN_KEY) {
        Some(toml::Value::String(v)) if v == "file" => Ok((existing, true)),
        Some(other) => Err(Error::Config(format!(
            "{} sets {PIN_KEY} = {other}; agent-switch needs \"file\" so that swapping auth.json takes effect. Change it, then run the command again",
            file.display()
        ))),
        None => Ok((existing, false)),
    }
}

/// Fail early, before anything is quit, when `config.toml` cannot be pinned.
pub fn check_pin(home: &Path) -> Result<()> {
    let (_, pinned) = read_pin(home)?;
    let file = home.join("config.toml");
    if !pinned && std::fs::symlink_metadata(&file).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::SymlinkedFile(file));
    }
    Ok(())
}

/// Make sure `<home>/config.toml` says `cli_auth_credentials_store = "file"`, so an `auth.json` swap cannot
/// be silently ignored by a keyring default. The first change backs the file up.
pub fn pin_file_store(paths: &Paths, home: &Path) -> Result<Pin> {
    let file = home.join("config.toml");
    let (existing, pinned) = read_pin(home)?;
    if pinned {
        return Ok(Pin::Already);
    }
    // A top-level key must come before the first table, so insert it as the first line.
    let updated = format!("{PIN_KEY} = \"file\"\n{}", existing.as_deref().unwrap_or(""));
    let check: toml::Table = toml::from_str(&updated).map_err(|e| Error::Config(format!("could not add {PIN_KEY} to {}: {e}", file.display())))?;
    if check.get(PIN_KEY).and_then(|v| v.as_str()) != Some("file") {
        return Err(Error::Config(format!("could not add {PIN_KEY} to {}", file.display())));
    }
    let backup = if existing.is_some() { Some(backup_file(paths, &file, "codex-config")?) } else { None };
    let mode = std::fs::metadata(&file).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777).unwrap_or(0o600);
    safe_write(&file, updated.as_bytes(), &WriteOpts::secret().with_mode(mode))?;
    Ok(Pin::Pinned { backup })
}

// ---------------------------------------------------------------------------------------------
// processes
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexProc {
    pub pid: u32,
    pub command: String,
    /// The home it uses: `CODEX_HOME` from its environment, else `~/.codex`.
    pub home: PathBuf,
    /// Belongs to the ChatGPT app bundle (the app's own `codex app-server`).
    pub in_app: bool,
}

impl CodexProc {
    /// A short label without the full command line, for messages.
    pub fn label(&self) -> String {
        let mut words = self.command.split_whitespace();
        let prog = words.next().map(|p| Path::new(p).file_name().and_then(|n| n.to_str()).unwrap_or(p)).unwrap_or("");
        format!("pid {} ({prog} {})", self.pid, words.next().unwrap_or("")).trim_end().to_string()
    }
}

fn is_codex_binary(p: &ProcInfo) -> bool {
    let b = p.basename();
    b == "codex" || b.starts_with("codex-")
}

/// Every Codex process and the home it uses. When the environment cannot be read the default home is
/// assumed, which errs on the side of treating the process as using `~/.codex`.
pub fn codex_processes(sys: &dyn System, homes: &Homes) -> Result<Vec<CodexProc>> {
    Ok(codex_processes_in(sys, &sys.processes()?, homes))
}

/// The same, from a process list that was already read.
pub fn codex_processes_in(sys: &dyn System, procs: &[ProcInfo], homes: &Homes) -> Vec<CodexProc> {
    let app: HashSet<u32> = bundle_pids(procs, &CHATGPT).into_iter().collect();
    procs
        .iter()
        .filter(|p| is_codex_binary(p) || app.contains(&p.pid) && p.command.contains("codex"))
        .map(|p| {
            let home = match sys.env_var(p.pid, "CODEX_HOME").ok().flatten() {
                Some(h) if !h.is_empty() => PathBuf::from(h.trim_end_matches('/')),
                _ => homes.primary.clone(),
            };
            CodexProc { pid: p.pid, command: p.command.clone(), home, in_app: app.contains(&p.pid) }
        })
        .collect()
}

/// Run `repair` after checking that nothing uses either home. With `dry_run` it only reports.
pub fn repair(sys: &dyn System, paths: &Paths, dry_run: bool) -> Result<RepairReport> {
    if !dry_run {
        let homes = Homes::new(paths);
        let procs = sys.processes()?;
        let mut busy: Vec<u32> = bundle_pids(&procs, &CHATGPT);
        busy.extend(codex_processes(sys, &homes)?.into_iter().filter(|p| p.home == homes.primary || p.home == homes.shallow).map(|p| p.pid));
        busy.sort_unstable();
        busy.dedup();
        if !busy.is_empty() {
            return Err(Error::Invalid(format!(
                "Codex is running (pids {}). `repair` only runs while no Codex process or ChatGPT app uses either home; quit them and run it again",
                join_pids(&busy)
            )));
        }
    }
    shallow_home::repair(paths, dry_run)
}

// ---------------------------------------------------------------------------------------------
// shell-init
// ---------------------------------------------------------------------------------------------

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// What `eval "$(agent-switch shell-init)"` runs: the `CODEX_HOME` export when the CLI has its own account
/// (only in shells with a terminal), and nothing otherwise.
pub fn shell_init(paths: &Paths, config: &Config) -> String {
    if !cli_separate(paths, config) {
        return String::new();
    }
    format!(
        "# agent-switch: the Codex CLI uses its own login; history stays linked to ~/.codex.\n\
         # Shells that are already open keep their old environment: open a new shell or run this again.\n\
         # Shells without a terminal (such as the ChatGPT app reading your environment) are skipped, so the app keeps ~/.codex.\n\
         if [ -t 0 ]; then export CODEX_HOME={}; fi\n",
        sh_quote(&shallow_dir(paths).display().to_string())
    )
}

// ---------------------------------------------------------------------------------------------
// add openai
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct Imported {
    pub id: String,
    pub created: bool,
    pub vault_written: bool,
    /// Where the chosen copy came from, and how it compares with the others.
    pub chosen: String,
    pub considered: Vec<String>,
    pub login: Login,
}

fn derive_id(config: &Config, login: &Login) -> String {
    let local: String = login
        .email
        .as_deref()
        .and_then(|e| e.split('@').next())
        .unwrap_or("openai")
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .take(14)
        .collect();
    let plan: String = login.plan.as_deref().unwrap_or("chatgpt").to_ascii_lowercase().chars().filter(char::is_ascii_alphanumeric).take(8).collect();
    let base = format!("{}-{plan}", if local.is_empty() { "openai" } else { &local });
    (1..)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|id| config.account(id).is_none())
        .expect("an unused id exists")
}

fn describe(c: &Copy) -> String {
    let when = c.login.last_refresh_us.map(|us| format!("last_refresh {}", rfc3339_secs(us / 1_000_000))).unwrap_or_else(|| "no last_refresh".into());
    format!("{} ({when})", c.source)
}

/// `2026-11-06T03:04:59Z` from Unix seconds.
fn rfc3339_secs(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Save one OpenAI login (given as one or more copies of it) into the vault and the config.
///
/// Idempotent: running it again with the same login changes nothing, and an older copy never replaces a
/// newer saved one. With several copies of one login the newest `last_refresh` is used and reported.
/// Copies of different logins are an error: import them one at a time.
pub fn import(env: &Env, sources: Vec<Copy>, id: Option<&str>, label: Option<&str>, dry_run: bool) -> Result<Imported> {
    let chosen = newest(&sources).ok_or_else(|| Error::Invalid("no login given".into()))?.clone();
    if let Some(other) = sources.iter().find(|c| !c.login.same_identity(&chosen.login)) {
        return Err(Error::Invalid(format!(
            "{} and {} are different ChatGPT logins; import them one at a time",
            chosen.source, other.source
        )));
    }
    let considered: Vec<String> = sources.iter().map(describe).collect();

    let config = Config::load(env.paths)?;
    let existing = find_account(&config, &chosen.login).cloned();
    let id = match (&existing, id) {
        (Some(e), Some(want)) if e.id != want => {
            return Err(Error::Invalid(format!("this login is already saved as `{}`; use that id or remove it first", e.id)))
        }
        (Some(e), _) => e.id.clone(),
        (None, Some(want)) => {
            crate::config::validate_account_id(want)?;
            if config.account(want).is_some() {
                return Err(Error::Invalid(format!("account id `{want}` already belongs to a different login")));
            }
            want.to_string()
        }
        (None, None) => derive_id(&config, &chosen.login),
    };

    let _lock = if dry_run { None } else { Some(AccountLock::acquire(env.paths, &id, LOCK_TIMEOUT)?) };
    let saved_is_newer = env
        .vault
        .get(&id, STORE)?
        .and_then(|s| Login::parse(&s.blob).ok())
        .is_some_and(|old| old.last_refresh_us.unwrap_or(i64::MIN) >= chosen.last_refresh());
    let vault_written = !saved_is_newer;
    if !dry_run {
        if vault_written {
            env.vault.put(&id, STORE, &chosen.slot()?)?;
        }
        if existing.is_none() {
            let plan = chosen.login.plan.clone().unwrap_or_else(|| "ChatGPT".into());
            Config::update(env.paths, |config| {
                config.add_account(Account {
                    id: id.clone(),
                    provider: Provider::Openai,
                    label: label.map(str::to_string).unwrap_or_else(|| format!("ChatGPT {plan}")),
                    email: chosen.login.email.clone(),
                    uuid: Some(chosen.login.account_id.clone()),
                })
            })?;
        }
    }
    Ok(Imported { id, created: existing.is_none(), vault_written, chosen: describe(&chosen), considered, login: chosen.login })
}

/// The Codex binary used for `codex login`: `AGENT_SWITCH_CODEX_BIN`, else `codex` on `PATH`, else the one
/// bundled in the ChatGPT app.
pub fn find_codex_bin() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("AGENT_SWITCH_CODEX_BIN").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let on_path = std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join("codex")).find(|c| c.is_file()));
    on_path
        .or_else(|| Some(PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex")).filter(|p| p.is_file()))
        .ok_or_else(|| Error::NotFound("the `codex` command (install Codex or the ChatGPT app)".into()))
}

/// Run `codex login` in a throwaway home (created first, because Codex refuses a missing `CODEX_HOME`),
/// read the result and remove the home again, even when the login fails.
pub fn login_interactive(paths: &Paths, codex_bin: &Path) -> Result<Copy> {
    let tmp = paths.support_dir().join("tmp").join(format!("codex-login-{}-{}", std::process::id(), now_secs()));
    ensure_private_dir(&tmp)?;
    let result = (|| -> Result<Copy> {
        safe_write(&tmp.join("config.toml"), format!("{PIN_KEY} = \"file\"\n").as_bytes(), &WriteOpts::secret())?;
        let status = std::process::Command::new(codex_bin)
            .arg("login")
            .env("CODEX_HOME", &tmp)
            .status()
            .map_err(|e| Error::io(format!("start {}", codex_bin.display()), e))?;
        if !status.success() {
            return Err(Error::Invalid(format!("`codex login` did not finish ({status}); nothing was saved")));
        }
        read_copy(&tmp, "the new login")?.ok_or_else(|| Error::Invalid("`codex login` finished but wrote no auth.json".into()))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

// ---------------------------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------------------------

fn check(level: Level, name: &'static str, detail: impl Into<String>) -> Check {
    Check { level, name, detail: detail.into() }
}

fn plist_version(app: &str) -> Option<String> {
    let plist = format!("{app}/Contents/Info.plist");
    let out = crate::proc::run(Path::new("/usr/bin/plutil"), &["-extract", "CFBundleShortVersionString", "raw", "-o", "-", &plist], None, Duration::from_secs(5)).ok()?;
    (out.code == 0).then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn cli_version(bin: &Path) -> Option<String> {
    let out = crate::proc::run(bin, &["--version"], None, Duration::from_secs(5)).ok()?;
    (out.code == 0).then(|| String::from_utf8_lossy(&out.stdout).trim().trim_start_matches("codex-cli ").to_string())
}

/// Codex storage checks for `doctor`: login format, the file-store pin, shallow-home drift, versions.
pub fn doctor_checks(env: &Env) -> Vec<Check> {
    let homes = Homes::new(env.paths);
    let config = Config::load(env.paths).ok();
    let mut out = Vec::new();

    match read_live(&homes) {
        Ok(Live { primary: Some(c), shallow }) => {
            let known = config.as_ref().and_then(|cfg| find_account(cfg, &c.login)).map(|a| format!("saved as `{}`", a.id)).unwrap_or_else(|| "not saved in agent-switch".into());
            let who = c.login.email.as_deref().map(crate::config::mask_email).unwrap_or_else(|| "unknown email".into());
            out.push(check(Level::Ok, "codex auth.json", format!("~/.codex holds {who} ({}), {known}", c.login.plan.as_deref().unwrap_or("plan unknown"))));
            if let Some(s) = shallow {
                if s.login.same_identity(&c.login) {
                    out.push(check(Level::Warn, "codex auth.json", "the shallow home holds a second live copy of the same login as ~/.codex; run `agent-switch switch codex-cli <account>` to re-join them"));
                }
            }
        }
        Ok(Live { primary: None, .. }) => out.push(check(Level::Warn, "codex auth.json", "~/.codex has no auth.json (logged out)")),
        Err(e) => out.push(check(Level::Fail, "codex auth.json", e.to_string())),
    }

    match std::fs::read_to_string(homes.primary.join("config.toml")).ok().and_then(|t| t.parse::<toml::Table>().ok()) {
        Some(t) if t.get(PIN_KEY).and_then(|v| v.as_str()) == Some("file") => out.push(check(Level::Ok, "codex storage", format!("{PIN_KEY} = \"file\" in ~/.codex/config.toml"))),
        Some(t) => out.push(check(Level::Warn, "codex storage", match t.get(PIN_KEY) {
            Some(v) => format!("{PIN_KEY} = {v}: auth.json swaps will not take effect; set it to \"file\""),
            None => format!("{PIN_KEY} is not pinned; it is added on the first switch"),
        })),
        None => out.push(check(Level::Warn, "codex storage", "~/.codex/config.toml is missing or unreadable")),
    }

    if homes.shallow.is_dir() {
        match shallow_home::scan(env.paths) {
            Ok(d) if d.is_empty() => out.push(check(Level::Ok, "shallow home", format!("{} is in sync", homes.shallow.display()))),
            Ok(d) => {
                let list: Vec<String> = d.iter().take(6).map(Drift::to_string).collect();
                out.push(check(Level::Warn, "shallow home", format!("{} drifted entries ({}); with Codex closed, run `agent-switch repair codex`", d.len(), list.join("; "))));
            }
            Err(e) => out.push(check(Level::Fail, "shallow home", e.to_string())),
        }
    }

    let app = plist_version(CHATGPT.bundle_path);
    let bundled = cli_version(&Path::new(CHATGPT.bundle_path).join("Contents/Resources/codex-cli/bin/codex"));
    let cli = find_codex_bin().ok().and_then(|b| cli_version(&b));
    let note = |name: &str, got: &Option<String>, tested: &str| match got {
        Some(v) if v == tested => format!("{name} {v}"),
        Some(v) => format!("{name} {v} (tested with {tested})"),
        None => format!("{name} not found"),
    };
    let untested = [(&app, TESTED_CHATGPT_APP), (&bundled, TESTED_BUNDLED_CODEX), (&cli, TESTED_CODEX_CLI)].iter().any(|(got, t)| got.as_deref().is_some_and(|v| v != *t));
    out.push(check(
        if untested { Level::Warn } else { Level::Ok },
        "app versions",
        format!("{}; {}; {}", note("ChatGPT", &app, TESTED_CHATGPT_APP), note("bundled codex", &bundled, TESTED_BUNDLED_CODEX), note("codex CLI", &cli, TESTED_CODEX_CLI)),
    ));
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::keychain::MemoryKeychain;
    use crate::macos_apps::fake::FakeSystem;
    use crate::providers::openai::tests::auth_json;
    use std::sync::Arc;

    pub struct Fixture {
        pub _d: tempfile::TempDir,
        pub paths: Paths,
        pub vault: Vault,
        pub homes: Homes,
    }

    impl Default for Fixture {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Fixture {
        pub fn new() -> Fixture {
            let d = tempfile::tempdir().unwrap();
            let paths = Paths::from_home(d.path());
            let homes = Homes::new(&paths);
            std::fs::create_dir_all(&homes.primary).unwrap();
            std::fs::write(homes.primary.join("config.toml"), "model = \"x\"\n[projects.\"/a\"]\ntrust_level = \"trusted\"\n").unwrap();
            Fixture { _d: d, paths, vault: Vault::new(Arc::new(MemoryKeychain::new())), homes }
        }
        pub fn env(&self) -> Env<'_> {
            Env { paths: &self.paths, vault: &self.vault }
        }
        /// Save `email`'s login under `id` in config and vault; returns its auth.json text.
        pub fn account(&self, id: &str, email: &str, acct: &str, rt: &str, last_refresh: &str) -> String {
            let text = auth_json(email, acct, "team", rt, last_refresh);
            let imported = import(&self.env(), vec![Copy::parse("test", text.clone()).unwrap()], Some(id), None, false).unwrap();
            assert_eq!(imported.id, id);
            text
        }
    }

    const T0: &str = "2026-10-27T07:32:17.000000Z";
    const T1: &str = "2026-11-06T03:04:59.000000Z";

    #[test]
    fn import_saves_the_login_and_the_account_and_is_idempotent() {
        let f = Fixture::new();
        let text = auth_json("zee@x.com", "acct-1", "team", "rt-1", T0);
        let r = import(&f.env(), vec![Copy::parse("a.json", text.clone()).unwrap()], Some("team-openai"), None, false).unwrap();
        assert!(r.created && r.vault_written && r.id == "team-openai");
        let cfg = Config::load(&f.paths).unwrap();
        let acct = cfg.account("team-openai").unwrap();
        assert_eq!((acct.email.as_deref(), acct.uuid.as_deref(), acct.label.as_str()), (Some("zee@x.com"), Some("acct-1"), "ChatGPT team"));
        assert_eq!(Login::parse(&f.vault.get("team-openai", STORE).unwrap().unwrap().blob).unwrap().account_id, "acct-1");

        let again = import(&f.env(), vec![Copy::parse("a.json", text).unwrap()], Some("team-openai"), None, false).unwrap();
        assert!(!again.created && !again.vault_written);
        assert_eq!(Config::load(&f.paths).unwrap().accounts.len(), 1);
    }

    #[test]
    fn import_dry_run_writes_nothing() {
        let f = Fixture::new();
        let c = Copy::parse("a.json", auth_json("zee@x.com", "acct-1", "plus", "rt-1", T0)).unwrap();
        let r = import(&f.env(), vec![c], None, None, true).unwrap();
        assert!(r.created && r.vault_written);
        assert_eq!(r.id, "zee-plus");
        assert!(Config::load(&f.paths).unwrap().accounts.is_empty());
        assert!(f.vault.get("zee-plus", STORE).unwrap().is_none());
    }

    #[test]
    fn import_picks_the_newest_copy_and_says_which() {
        let f = Fixture::new();
        let old = Copy::parse("old.json", auth_json("zee@x.com", "acct-1", "plus", "rt-old", T0)).unwrap();
        let new = Copy::parse("new.json", auth_json("zee@x.com", "acct-1", "plus", "rt-new", T1)).unwrap();
        let r = import(&f.env(), vec![old.clone(), new.clone()], Some("personal-openai"), None, false).unwrap();
        assert!(r.chosen.starts_with("new.json") && r.chosen.contains("2026-11-06T03:04:59Z"), "{}", r.chosen);
        assert_eq!(r.considered.len(), 2);
        assert_eq!(f.vault.get("personal-openai", STORE).unwrap().unwrap().fingerprint, new.login.fingerprint);
        // An older copy offered later never replaces the newer saved one.
        let r = import(&f.env(), vec![old], Some("personal-openai"), None, false).unwrap();
        assert!(!r.vault_written);
        assert_eq!(f.vault.get("personal-openai", STORE).unwrap().unwrap().fingerprint, new.login.fingerprint);
    }

    #[test]
    fn import_refuses_mixed_logins_and_id_clashes() {
        let f = Fixture::new();
        let a = Copy::parse("a.json", auth_json("zee@x.com", "acct-1", "plus", "rt-1", T0)).unwrap();
        let b = Copy::parse("b.json", auth_json("zee@x.com", "acct-2", "team", "rt-2", T0)).unwrap();
        assert!(import(&f.env(), vec![a.clone(), b.clone()], None, None, false).unwrap_err().to_string().contains("different ChatGPT logins"));
        import(&f.env(), vec![a.clone()], Some("one"), None, false).unwrap();
        assert!(import(&f.env(), vec![a], Some("two"), None, false).unwrap_err().to_string().contains("already saved as `one`"));
        assert!(import(&f.env(), vec![b], Some("one"), None, false).unwrap_err().to_string().contains("different login"));
    }

    #[test]
    fn newest_prefers_the_latest_refresh_and_the_first_on_a_tie() {
        let mk = |src: &str, t: &str| Copy::parse(src, auth_json("a@b.c", "x", "plus", src, t)).unwrap();
        let list = [mk("a", T0), mk("b", T1), mk("c", T1)];
        assert_eq!(newest(&list).unwrap().source, "b");
        assert!(newest(&[]).is_none());
    }

    #[test]
    fn save_back_updates_only_when_the_live_copy_is_newer_and_reports_unknown_logins() {
        let f = Fixture::new();
        f.account("team", "zee@x.com", "acct-1", "rt-1", T0);
        let cfg = Config::load(&f.paths).unwrap();
        let rotated = Copy::parse("~/.codex", auth_json("zee@x.com", "acct-1", "team", "rt-2", T1)).unwrap();
        let stale = Copy::parse("shallow", auth_json("zee@x.com", "acct-1", "team", "rt-0", "2026-01-01T00:00:00Z")).unwrap();
        let stranger = Copy::parse("other", auth_json("who@x.com", "acct-9", "plus", "rt-9", T1)).unwrap();

        let r = save_back(&f.env(), &cfg, &[], &[&stale, &stranger]).unwrap();
        assert!(r.saved.is_empty());
        assert_eq!(r.unknown, vec!["other".to_string()]);

        let r = save_back(&f.env(), &cfg, &[], &[&rotated]).unwrap();
        assert_eq!(r.saved, vec!["team".to_string()]);
        assert_eq!(f.vault.get("team", STORE).unwrap().unwrap().fingerprint, rotated.login.fingerprint);
    }

    #[test]
    fn write_auth_takes_the_lock_writes_0600_and_rejects_junk() {
        let f = Fixture::new();
        let text = auth_json("zee@x.com", "acct-1", "team", "rt-1", T0);
        write_auth(&f.homes.primary, &text).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(f.homes.primary.join("auth.json")).unwrap().permissions()) & 0o777;
        assert_eq!(mode, 0o600);
        assert!(f.homes.primary.join("auth.lock").exists());
        assert!(write_auth(&f.homes.primary, "not json").is_err());
        assert_eq!(read_copy(&f.homes.primary, "x").unwrap().unwrap().text, text);
    }

    #[test]
    fn pin_adds_the_key_before_any_table_and_keeps_everything_else() {
        let f = Fixture::new();
        assert!(matches!(pin_file_store(&f.paths, &f.homes.primary).unwrap(), Pin::Pinned { backup: Some(_) }));
        let text = std::fs::read_to_string(f.homes.primary.join("config.toml")).unwrap();
        assert!(text.starts_with("cli_auth_credentials_store = \"file\"\nmodel = \"x\"\n[projects.\"/a\"]"), "{text}");
        let t: toml::Table = text.parse().unwrap();
        assert_eq!(t["cli_auth_credentials_store"].as_str(), Some("file"));
        assert_eq!(pin_file_store(&f.paths, &f.homes.primary).unwrap(), Pin::Already);
        let backups: Vec<_> = std::fs::read_dir(f.paths.support_dir().join("backups")).unwrap().collect();
        assert_eq!(backups.len(), 1, "only the first pin makes a backup");
    }

    #[test]
    fn pin_refuses_a_different_explicit_value_and_a_broken_file() {
        let f = Fixture::new();
        std::fs::write(f.homes.primary.join("config.toml"), "cli_auth_credentials_store = \"keyring\"\n").unwrap();
        assert!(pin_file_store(&f.paths, &f.homes.primary).unwrap_err().to_string().contains("keyring"));
        std::fs::write(f.homes.primary.join("config.toml"), "= broken").unwrap();
        assert!(pin_file_store(&f.paths, &f.homes.primary).is_err());
        std::fs::remove_file(f.homes.primary.join("config.toml")).unwrap();
        assert!(matches!(pin_file_store(&f.paths, &f.homes.primary).unwrap(), Pin::Pinned { backup: None }));
    }

    #[test]
    fn shell_init_prints_the_export_only_for_a_separate_binding() {
        let f = Fixture::new();
        f.account("team", "zee@x.com", "acct-1", "rt-1", T0);
        let personal = auth_json("me@x.com", "acct-2", "plus", "rt-2", T0);
        import(&f.env(), vec![Copy::parse("p", personal).unwrap()], Some("personal"), None, false).unwrap();
        let mut cfg = Config::load(&f.paths).unwrap();
        assert_eq!(shell_init(&f.paths, &cfg), "", "unmanaged: nothing");

        cfg.bind(crate::config::SurfaceKind::ChatgptDesktop, crate::config::Binding::parse("team")).unwrap();
        cfg.bind(crate::config::SurfaceKind::CodexCli, crate::config::Binding::parse("personal")).unwrap();
        let out = shell_init(&f.paths, &cfg);
        assert!(out.contains("if [ -t 0 ]; then export CODEX_HOME='") && out.contains(".agent-switch/codex-cli'; fi"), "{out}");
        assert!(out.contains("skipped, so the app keeps ~/.codex"));
        assert!(out.contains("already open keep their old environment"));

        cfg.bind(crate::config::SurfaceKind::CodexCli, crate::config::Binding::parse("team")).unwrap();
        assert_eq!(shell_init(&f.paths, &cfg), "", "bound together: nothing");
    }

    #[test]
    fn shell_init_quotes_a_hostile_home() {
        assert_eq!(sh_quote("/a b/it's"), "'/a b/it'\\''s'");
    }

    #[test]
    fn codex_processes_use_codex_home_from_the_environment_and_default_to_the_primary() {
        let f = Fixture::new();
        let sys = FakeSystem::new(true);
        sys.add(1, "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex app-server");
        sys.add(2, "/Users/me/.local/bin/codex");
        sys.add(3, "/Users/me/.vscode/codex app-server");
        sys.add(4, "/usr/bin/vim codex.txt");
        let shallow = f.homes.shallow.display().to_string();
        sys.env.borrow_mut().insert((2, "CODEX_HOME".into()), shallow);
        let procs = codex_processes(&sys, &f.homes).unwrap();
        let find = |pid| procs.iter().find(|p| p.pid == pid).unwrap();
        assert_eq!(procs.len(), 3);
        assert!(find(1).in_app && find(1).home == f.homes.primary);
        assert!(!find(2).in_app && find(2).home == f.homes.shallow);
        assert!(!find(3).in_app && find(3).home == f.homes.primary);
        assert_eq!(find(3).label(), "pid 3 (codex app-server)");
    }

    #[test]
    fn repair_refuses_while_codex_or_the_app_runs_but_a_dry_run_still_reports() {
        let f = Fixture::new();
        shallow_home::ensure(&f.paths).unwrap();
        let sys = FakeSystem::new(true);
        sys.add(7, "/Users/me/.local/bin/codex");
        let err = repair(&sys, &f.paths, false).unwrap_err().to_string();
        assert!(err.contains("Codex is running") && err.contains('7'), "{err}");
        assert!(repair(&sys, &f.paths, true).is_ok());

        let calm = FakeSystem::new(true);
        calm.add(8, "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT");
        assert!(repair(&calm, &f.paths, false).unwrap_err().to_string().contains('8'));
        let other_home = FakeSystem::new(true);
        other_home.add(9, "/x/codex");
        other_home.env.borrow_mut().insert((9, "CODEX_HOME".into()), "/tmp/elsewhere".into());
        assert!(repair(&other_home, &f.paths, false).is_ok(), "a Codex on a different home does not block repair");
    }

    #[test]
    fn rfc3339_formatting_round_trips() {
        assert_eq!(rfc3339_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_secs(1_793_086_337), "2026-10-27T07:32:17Z");
    }

    #[test]
    fn vault_copy_rejects_a_slot_of_another_identity() {
        let f = Fixture::new();
        f.account("team", "zee@x.com", "acct-1", "rt-1", T0);
        let mut cfg = Config::load(&f.paths).unwrap();
        assert!(vault_copy(&f.vault, cfg.account("team").unwrap()).is_ok());
        cfg.accounts[0].uuid = Some("acct-other".into());
        assert!(vault_copy(&f.vault, &cfg.accounts[0]).unwrap_err().to_string().contains("different ChatGPT account"));
        assert!(matches!(vault_copy(&f.vault, &Account { id: "ghost".into(), provider: Provider::Openai, label: "g".into(), email: None, uuid: None }), Err(Error::NotFound(_))));
    }

    #[test]
    fn doctor_reports_unsaved_logins_and_the_pin() {
        let f = Fixture::new();
        write_auth(&f.homes.primary, &auth_json("zee@x.com", "acct-1", "team", "rt-1", T0)).unwrap();
        let checks = doctor_checks(&f.env());
        let auth = checks.iter().find(|c| c.name == "codex auth.json").unwrap();
        assert!(auth.detail.contains("ze***@x.com") && auth.detail.contains("not saved in agent-switch") && !auth.detail.contains("zee@"), "{}", auth.detail);
        let storage = checks.iter().find(|c| c.name == "codex storage").unwrap();
        assert_eq!(storage.level, Level::Warn);
        pin_file_store(&f.paths, &f.homes.primary).unwrap();
        assert_eq!(doctor_checks(&f.env()).iter().find(|c| c.name == "codex storage").unwrap().level, Level::Ok);
    }
}
