//! Claude Code CLI adapter.
//!
//! A switch replaces only the login: the account-bound keys of the Keychain item `Claude Code-credentials`
//! and the `oauthAccount` field of `~/.claude.json`. History, settings, skills, MCP servers and the MCP
//! logins are never touched.
//!
//! Ported from claude-swap (MIT): the shared-key allowlist (`credentials.py:232-300`), the lock protocol
//! (`claude_locks.py`), the hashed item name (`session.py:232`, `keychain_service_name`), the `rewrite
//! credentials file when present` rule (`credentials.py:1033`) and the ownership check before save-back
//! (`switcher.py:7488-7507`, `oauth.py:60`).
//!
//! * The live item is read and written only through `/usr/bin/security` on stdin (`SecurityCli`). If the
//!   merged login does not fit its line limit the switch fails closed, before any write.
//! * The vault slot of an account holds `{"v":1,"login":{<account-bound keys>},"oauthAccount":{...}}`.
//!   `login` is everything in the live item outside the machine-shared allowlist (`claudeAiOauth`,
//!   `trustedDeviceToken` and any unknown key). Unknown keys are account-bound.
//! * `apply` runs the whole transaction while holding Claude Code's locks (`.oauth_refresh.lock`, then
//!   `~/.claude.lock`, and `~/.claude.json.lock` for the config write): classify who owns the live login,
//!   save it back or quarantine it, merge into the live item, replace `oauthAccount`, and roll the live
//!   item back if a later step fails. The switch is journaled in `state.json` by `switch::activate`, only
//!   after `apply` succeeds, so a failed switch leaves no record.
//! * Refresh tokens rotate, so a live login that Claude Code refreshed no longer matches its slot's
//!   fingerprint. `stage` (outside the locks, because it uses the network) asks the profile endpoint who
//!   owns the live access token; `apply` uses that answer only when the live access token is unchanged.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::config::{Account, Config, Provider, SurfaceKind};
use crate::doctor::{Check, Level};
use crate::error::{Error, Result};
use crate::fsutil::{ensure_private_dir, safe_write, WriteOpts};
use crate::keychain::Keychain;
use crate::locks::{AccountLock, DirLock, DirLockOpts};
use crate::paths::Paths;
use crate::providers::anthropic::{AnthropicApi, Profile, ProfileSource};
use crate::security_cli::{encode_add_line, SecurityCli};
use crate::state::now_secs;
use crate::surface::{Env, Identity, Staged, Surface};
use crate::vault::{lineage_fingerprint, normalize_blob, Slot};

/// Claude Code's own item for the default profile.
pub const LIVE_SERVICE: &str = "Claude Code-credentials";
/// Keys of the live item that belong to the machine, not to an account. They rotate independently of any
/// login, so on a switch the live copy wins (claude-swap `credentials.py:232-251`).
pub const SHARED_KEYS: [&str; 5] = ["mcpOAuth", "mcpOAuthClientConfig", "mcpXaaIdp", "mcpXaaIdpConfig", "pluginSecrets"];
/// The transaction must not hold Claude Code's locks longer than this (their stale limit is 60 s).
const HOLD_BUDGET: Duration = Duration::from_secs(5);
const STORE: &str = "claude-oauth";
const ENV_REFUSED: [&str; 3] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"];

type Obj = Map<String, Value>;
type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
type WriteFn = Arc<dyn Fn(&Path, &[u8]) -> Result<()> + Send + Sync>;

// ---------------------------------------------------------------------------------------------
// Item names and environment
// ---------------------------------------------------------------------------------------------

/// Keychain service Claude Code derives for a config dir: SHA-256 of the NFC-normalized raw path string,
/// first 8 hex characters (claude-swap `session.py:232`). The raw string is hashed, never a resolved path.
pub fn keychain_service_name(config_dir: &str) -> String {
    let normalized: String = config_dir.nfc().collect();
    let digest = Sha256::digest(normalized.as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{LIVE_SERVICE}-{hex}")
}

/// Where Claude Code keeps its login for the current environment.
#[derive(Debug, Clone)]
struct Setup {
    config_home: PathBuf,
    global_config: PathBuf,
    /// Candidate Keychain services in try order.
    services: Vec<String>,
    /// Keychain account name (`$USER`).
    user: String,
}

impl Setup {
    fn resolve(paths: &Paths, getenv: &EnvFn) -> Setup {
        let default_home = paths.home().join(".claude");
        let config_dir = getenv("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty());
        let (config_home, global_config) = match &config_dir {
            Some(d) => (PathBuf::from(d), PathBuf::from(d).join(".claude.json")),
            None => (default_home.clone(), paths.home().join(".claude.json")),
        };
        // Claude sources secure storage from CLAUDE_SECURESTORAGE_CONFIG_DIR when it is defined (empty
        // selects the default store), else from CLAUDE_CONFIG_DIR (claude-swap `credentials.py:75-110`).
        let services = match getenv("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
            Some(s) if s.is_empty() => vec![LIVE_SERVICE.to_string()],
            Some(s) => vec![keychain_service_name(&s)],
            None => match &config_dir {
                None => vec![LIVE_SERVICE.to_string()],
                Some(d) => {
                    let mut v = vec![keychain_service_name(d)];
                    let same = match (std::fs::canonicalize(d), std::fs::canonicalize(&default_home)) {
                        (Ok(a), Ok(b)) => a == b,
                        _ => false,
                    };
                    if same {
                        v.push(LIVE_SERVICE.to_string());
                    }
                    v
                }
            },
        };
        let user = getenv("USER")
            .filter(|s| !s.is_empty())
            .or_else(|| getenv("LOGNAME").filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "claude-code-user".to_string());
        Setup { config_home, global_config, services, user }
    }

    fn sibling_lock(path: &Path) -> PathBuf {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        path.parent().unwrap_or_else(|| Path::new(".")).join(format!("{name}.lock"))
    }

    /// `<config home>/.oauth_refresh.lock`
    fn refresh_lock(&self) -> PathBuf {
        self.config_home.join(".oauth_refresh.lock")
    }

    /// `~/.claude.lock`, kept by Claude Code for external tools.
    fn legacy_lock(&self) -> PathBuf {
        Self::sibling_lock(&self.config_home)
    }

    /// `~/.claude.json.lock`
    fn config_lock(&self) -> PathBuf {
        Self::sibling_lock(&self.global_config)
    }

    /// Claude Code reads this instead of the Keychain when it exists.
    fn credentials_file(&self) -> PathBuf {
        self.config_home.join(".credentials.json")
    }
}

// ---------------------------------------------------------------------------------------------
// Login blobs (pure functions)
// ---------------------------------------------------------------------------------------------

fn format_error(detail: &str) -> Error {
    Error::Invalid(format!(
        "Claude Code's stored login has an unexpected format ({detail}); agent-switch will not touch it. Run `agent-switch doctor`, and update agent-switch if Claude Code changed its storage"
    ))
}

/// Parse the live item text. `security ... -w` prints data it considers non-printable as hex, so accept
/// a hex-encoded JSON object too.
fn parse_login(text: &str) -> Result<Obj> {
    let trimmed = text.trim();
    let json_text = if trimmed.starts_with('{') {
        trimmed.to_string()
    } else if !trimmed.is_empty() && trimmed.len() % 2 == 0 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        let bytes: Vec<u8> = (0..trimmed.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&trimmed[i..i + 2], 16).unwrap_or(0))
            .collect();
        String::from_utf8(bytes).map_err(|_| format_error("not UTF-8"))?
    } else {
        return Err(format_error("not a JSON object"));
    };
    match serde_json::from_str::<Value>(&json_text) {
        Ok(Value::Object(o)) => Ok(o),
        _ => Err(format_error("not a JSON object")),
    }
}

fn oauth_of(login: &Obj) -> Result<&Obj> {
    login.get("claudeAiOauth").and_then(Value::as_object).ok_or_else(|| format_error("no claudeAiOauth object"))
}

fn token<'a>(login: &'a Obj, key: &str) -> Result<&'a str> {
    oauth_of(login)?
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format_error(&format!("claudeAiOauth has no {key}")))
}

/// Fingerprint of the refresh-token chain; also proves the blob carries a usable login.
fn fingerprint_of(login: &Obj) -> Result<String> {
    Ok(lineage_fingerprint(token(login, "refreshToken")?))
}

fn is_shared(key: &str) -> bool {
    SHARED_KEYS.contains(&key)
}

/// Everything outside the machine-shared allowlist.
fn account_bound(login: &Obj) -> Obj {
    login.iter().filter(|(k, _)| !is_shared(k)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Compose a target login with the machine's shared keys: shared keys are wholly live-owned (a key the
/// live item lacks is absent from the result), every other key comes from the target (claude-swap
/// `merge_shared_credential_fields`, `credentials.py:282`).
fn merge_login(live: Option<&Obj>, target_login: &Obj) -> Obj {
    let mut merged = account_bound(target_login);
    if let Some(live) = live {
        for (k, v) in live.iter().filter(|(k, _)| is_shared(k)) {
            merged.insert(k.clone(), v.clone());
        }
    }
    merged
}

/// Compact JSON restricted to printable ASCII, the form `security -i` accepts.
fn compact_ascii(obj: &Obj) -> Result<String> {
    let text = serde_json::to_string(obj).map_err(|e| Error::Invalid(e.to_string()))?;
    normalize_blob(&text)
}

/// The vault slot payload of one Claude account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeSlot {
    v: u8,
    /// Account-bound keys of the live item.
    pub login: Obj,
    /// The `oauthAccount` object of `~/.claude.json`.
    #[serde(rename = "oauthAccount")]
    pub oauth_account: Value,
}

impl ClaudeSlot {
    pub fn new(login: Obj, oauth_account: Value) -> ClaudeSlot {
        ClaudeSlot { v: 1, login, oauth_account }
    }

    pub fn parse(blob: &str) -> Result<ClaudeSlot> {
        let slot: ClaudeSlot =
            serde_json::from_str(blob).map_err(|e| Error::Invalid(format!("saved Claude login is not in the expected format: {e}")))?;
        fingerprint_of(&slot.login)?;
        if !slot.oauth_account.is_object() {
            return Err(Error::Invalid("saved Claude login has no oauthAccount".into()));
        }
        Ok(slot)
    }

    pub fn to_slot(&self, refreshed_at: u64) -> Result<Slot> {
        let text = serde_json::to_string(self).map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(Slot { blob: normalize_blob(&text)?, fingerprint: fingerprint_of(&self.login)?, refreshed_at })
    }

    pub fn account_uuid(&self) -> Option<&str> {
        self.oauth_account.get("accountUuid").and_then(Value::as_str)
    }

    pub fn email(&self) -> Option<&str> {
        self.oauth_account.get("emailAddress").and_then(Value::as_str)
    }

    /// OAuth access token, for the profile and usage endpoints.
    pub fn access_token(&self) -> Result<&str> {
        token(&self.login, "accessToken")
    }
}

// ---------------------------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------------------------

/// Who owns the live login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The live login is a saved account's (by fingerprint, or by profile identity after a rotation).
    Account(String),
    /// No saved account owns it. `resolved` is false when the identity check gave no answer.
    Unknown { fp: String, resolved: bool },
}

struct SavedLogin {
    id: String,
    fp: String,
    slot: Option<ClaudeSlot>,
}

/// What `stage` learned about the live login while no lock was held.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Probe {
    /// Fingerprint of the live access token the answer is about.
    access_fp: String,
    uuid: Option<String>,
    error: Option<String>,
}

fn classify(live_fp: &str, saved: &[SavedLogin], probe_uuid: Option<&str>) -> Owner {
    if let Some(s) = saved.iter().find(|s| s.fp == live_fp) {
        return Owner::Account(s.id.clone());
    }
    if let Some(uuid) = probe_uuid {
        if let Some(s) = saved.iter().find(|s| s.slot.as_ref().and_then(ClaudeSlot::account_uuid) == Some(uuid)) {
            return Owner::Account(s.id.clone());
        }
    }
    Owner::Unknown { fp: live_fp.to_string(), resolved: probe_uuid.is_some() }
}

// ---------------------------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------------------------

/// Payload that travels from `stage` to `apply`. Holds a token, so it only lives in memory.
#[derive(Serialize, Deserialize)]
struct StagePayload {
    slot: String,
    probe: Option<Probe>,
}

pub struct ClaudeCode {
    live: Arc<dyn Keychain>,
    profiles: Arc<dyn ProfileSource + Send + Sync>,
    getenv: EnvFn,
    write_file: WriteFn,
    claude_bin: PathBuf,
}

/// An account saved by `add`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub id: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// False when the account already existed and only its saved login was refreshed.
    pub created: bool,
    pub dry_run: bool,
}

/// Where `add` reads a login from.
#[derive(Debug, Clone)]
pub struct LoginSource {
    pub service: String,
    /// The `.claude.json` that holds the matching `oauthAccount`, when there is one.
    pub config_json: Option<PathBuf>,
}

impl ClaudeCode {
    pub fn production() -> ClaudeCode {
        ClaudeCode::new(
            Arc::new(SecurityCli::default()),
            Arc::new(AnthropicApi::default()),
            Arc::new(|k| std::env::var(k).ok()),
        )
    }

    pub fn new(live: Arc<dyn Keychain>, profiles: Arc<dyn ProfileSource + Send + Sync>, getenv: EnvFn) -> ClaudeCode {
        ClaudeCode {
            live,
            profiles,
            getenv,
            write_file: Arc::new(|path, bytes| safe_write(path, bytes, &WriteOpts::secret())),
            claude_bin: PathBuf::from("claude"),
        }
    }

    /// Replace how `~/.claude.json` and `.credentials.json` are written (fault injection in tests).
    pub fn with_file_writer(mut self, w: WriteFn) -> ClaudeCode {
        self.write_file = w;
        self
    }

    pub fn with_claude_binary(mut self, bin: impl Into<PathBuf>) -> ClaudeCode {
        self.claude_bin = bin.into();
        self
    }

    fn setup(&self, env: &Env) -> Setup {
        Setup::resolve(env.paths, &self.getenv)
    }

    /// These variables override the OAuth login, so a switch would change nothing Claude Code uses.
    fn refuse_overrides(&self) -> Result<()> {
        let set: Vec<&str> = ENV_REFUSED.into_iter().filter(|k| (self.getenv)(k).is_some_and(|v| !v.is_empty())).collect();
        if set.is_empty() {
            return Ok(());
        }
        Err(Error::Invalid(format!(
            "{} is set. Claude Code uses it instead of the saved OAuth login, so switching accounts would have no effect. Unset it (and restart this shell) and run the command again",
            set.join(", ")
        )))
    }

    /// The Keychain service in use and its current text.
    fn read_live(&self, setup: &Setup) -> Result<(String, Option<String>)> {
        for s in &setup.services {
            if let Some(text) = self.live.get(s, &setup.user)? {
                return Ok((s.clone(), Some(text)));
            }
        }
        Ok((setup.services[0].clone(), None))
    }

    fn saved_logins(&self, env: &Env) -> Result<Vec<SavedLogin>> {
        let config = Config::load(env.paths)?;
        let mut out = Vec::new();
        for a in config.accounts.iter().filter(|a| a.provider == Provider::Claude) {
            if let Some(slot) = env.vault.get(&a.id, STORE)? {
                out.push(SavedLogin { id: a.id.clone(), fp: slot.fingerprint.clone(), slot: ClaudeSlot::parse(&slot.blob).ok() });
            }
        }
        Ok(out)
    }

    fn oauth_account_in_config(&self, setup: &Setup) -> Option<Value> {
        let text = std::fs::read_to_string(&setup.global_config).ok()?;
        let obj: Obj = serde_json::from_str(&text).ok()?;
        obj.get("oauthAccount").filter(|v| v.is_object()).cloned()
    }

    // ----- apply internals -----

    fn check_budget(started: Instant, step: &str) -> Result<()> {
        if started.elapsed() > HOLD_BUDGET {
            return Err(Error::Invalid(format!("took longer than {HOLD_BUDGET:?} while holding Claude Code's locks; stopped before {step}")));
        }
        Ok(())
    }

    fn missing_global_config(setup: &Setup) -> Error {
        Error::Invalid(format!("{} does not exist; start Claude Code once so it creates it", setup.global_config.display()))
    }

    /// Every file the switch writes after the Keychain must be writable as a regular file. Checked before the
    /// Keychain changes, so a missing or symlinked file stops the switch without a rollback.
    fn check_file_targets(setup: &Setup, creds_file: &Path, creds_file_exists: bool) -> Result<()> {
        let is_link = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
        match std::fs::symlink_metadata(&setup.global_config) {
            Ok(_) if is_link(&setup.global_config) => return Err(Error::SymlinkedFile(setup.global_config.clone())),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Self::missing_global_config(setup)),
            Err(e) => return Err(Error::io(format!("read {}", setup.global_config.display()), e)),
        }
        if creds_file_exists && is_link(creds_file) {
            return Err(Error::SymlinkedFile(creds_file.to_path_buf()));
        }
        Ok(())
    }

    /// Write `~/.claude.json` with only the `oauthAccount` value replaced; every other byte stays as it was.
    fn write_identity(&self, setup: &Setup, oauth_account: &Value) -> Result<()> {
        let _lock = DirLock::acquire(&setup.config_lock(), DirLockOpts::config())?;
        let text = std::fs::read_to_string(&setup.global_config).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Self::missing_global_config(setup)
            } else {
                Error::io(format!("read {}", setup.global_config.display()), e)
            }
        })?;
        serde_json::from_str::<Obj>(&text)
            .map_err(|_| Error::Invalid(format!("{} is not a JSON object; refusing to rewrite it", setup.global_config.display())))?;
        let out = splice_oauth_account(&text, oauth_account)?;
        (self.write_file)(&setup.global_config, out.as_bytes())
    }

    fn restore_live(&self, service: &str, user: &str, original: Option<&str>) -> Result<()> {
        match original {
            Some(text) => self.live.set(service, user, &compact_ascii(&parse_login(text)?)?),
            None => self.live.delete(service, user).map(|_| ()),
        }
    }

    /// Keep a recovery copy of a live login nobody saved. One copy per login, at most a few in total, listed
    /// by `doctor` and deleted by `remove`.
    fn quarantine(&self, env: &Env, setup: &Setup, live: &Obj, fp: &str) -> Result<String> {
        let id = format!("quarantine-{fp}");
        let oauth_account = self.oauth_account_in_config(setup).unwrap_or_else(|| Value::Object(Obj::new()));
        let slot = ClaudeSlot::new(account_bound(live), oauth_account).to_slot(now_secs())?;
        crate::switch::put_quarantine(env.paths, env.vault, &id, &slot)?;
        Ok(id)
    }

    /// The saved account that owns the live login right now, judged from the stage-time probe. Called before
    /// Claude Code's locks are taken, so the caller can lock that account first and wait for it without
    /// holding them. `None` when nothing is live or no saved account owns it.
    fn live_owner(&self, env: &Env, setup: &Setup, payload: &StagePayload) -> Result<Option<String>> {
        let (_, live_text) = self.read_live(setup)?;
        let Some(live) = live_text.as_deref().map(parse_login).transpose()? else { return Ok(None) };
        let fp = fingerprint_of(&live)?;
        let probe = payload.probe.as_ref().filter(|p| token(&live, "accessToken").is_ok_and(|t| lineage_fingerprint(t) == p.access_fp));
        match classify(&fp, &self.saved_logins(env)?, probe.and_then(|p| p.uuid.as_deref())) {
            Owner::Account(id) => Ok(Some(id)),
            Owner::Unknown { .. } => Ok(None),
        }
    }

    /// `locked` are the account ids whose account locks the caller holds. The owner's slot is read and
    /// written only under one of them; if the live login changed to another owner after the lock was
    /// taken, the switch stops and can be retried.
    fn transact(&self, env: &Env, setup: &Setup, payload: &StagePayload, target_id: &str, locked: &[String]) -> Result<()> {
        let started = Instant::now();
        let target = ClaudeSlot::parse(&payload.slot)?;
        let (service, live_text) = self.read_live(setup)?;
        let live = live_text.as_deref().map(parse_login).transpose()?;

        // Step 1: who owns the live login?
        let mut owner_slot: Option<(String, ClaudeSlot)> = None;
        let mut owner_account: Option<String> = None;
        if let Some(live_obj) = &live {
            let fp = fingerprint_of(live_obj)?;
            let saved = self.saved_logins(env)?;
            let probe = payload.probe.as_ref().filter(|p| token(live_obj, "accessToken").is_ok_and(|t| lineage_fingerprint(t) == p.access_fp));
            let owner = classify(&fp, &saved, probe.and_then(|p| p.uuid.as_deref()));
            match owner {
                Owner::Account(id) => {
                    let existing = saved.iter().find(|s| s.id == id).and_then(|s| s.slot.clone()).ok_or_else(|| {
                        Error::Invalid(format!("the saved login of `{id}` is unreadable; run `agent-switch doctor`"))
                    })?;
                    if !locked.contains(&id) {
                        return Err(Error::Invalid(format!(
                            "the live Claude Code login changed to `{id}` while switching. Nothing was changed; run the command again"
                        )));
                    }
                    owner_slot = Some((id.clone(), existing));
                    owner_account = Some(id);
                }
                Owner::Unknown { fp, resolved } => {
                    let stash = self.quarantine(env, setup, live_obj, &fp)?;
                    let why = if resolved {
                        "does not belong to any saved account".to_string()
                    } else {
                        let reason = probe.and_then(|p| p.error.clone()).unwrap_or_else(|| "the login changed while switching".into());
                        format!("could not be matched to a saved account ({reason}). Use Claude Code once so it refreshes the login, then retry")
                    };
                    return Err(Error::Invalid(format!(
                        "the live Claude Code login {why}. The switch did not run and no saved account was changed, but a recovery copy of the live login was kept as `{stash}` (listed by `agent-switch doctor`; at most {} are kept). To save it as an account, run `agent-switch add claude-cli --from-keychain-item '{service}' --id <name>`; delete the copy with `agent-switch remove {stash}`",
                        crate::state::QUARANTINE_LIMIT
                    )));
                }
            }
        }

        // The login that will be written: the target slot, or the live login when the target already owns it.
        let owner_is_target = owner_account.as_deref() == Some(target_id);
        let target_login = match (&live, owner_is_target) {
            (Some(l), true) => account_bound(l),
            _ => target.login.clone(),
        };
        let merged = merge_login(live.as_ref(), &target_login);
        let merged_text = compact_ascii(&merged)?;
        // Fail closed before any write when the live item's stdin limit cannot carry the result.
        encode_add_line(&service, &setup.user, &merged_text).map_err(|e| match e {
            Error::TooLarge { len, limit } => Error::Invalid(format!(
                "the merged Claude login is too large for /usr/bin/security's stdin limit ({len} > {limit} bytes), and agent-switch never puts tokens on a command line. Nothing was changed. Remove unused MCP logins from Claude Code to shrink it"
            )),
            other => other,
        })?;

        // Step 1 (continued): save the outgoing login back, so a refreshed token is not lost.
        if let (Some(live_obj), Some((id, existing))) = (&live, &owner_slot) {
            let updated = ClaudeSlot::new(account_bound(live_obj), existing.oauth_account.clone());
            if updated != *existing {
                env.vault.put(id, STORE, &updated.to_slot(now_secs())?)?;
            }
        }

        // Steps 3 and 4 with rollback. `original_*` are restored if a later step fails.
        Self::check_budget(started, "writing the login")?;
        let creds_file = setup.credentials_file();
        let original_creds_file = match std::fs::read(&creds_file) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::io(format!("read {}", creds_file.display()), e)),
        };

        Self::check_file_targets(setup, &creds_file, original_creds_file.is_some())?;
        let write_result = (|| -> Result<()> {
            self.live.set(&service, &setup.user, &merged_text)?;
            if original_creds_file.is_some() {
                // Claude Code reads this file instead of the Keychain when it exists, and drops its cached
                // token when the file's mtime changes (claude-swap `credentials.py:1033`). Never created.
                (self.write_file)(&creds_file, merged_text.as_bytes())?;
            }
            Self::check_budget(started, "writing the identity")?;
            self.write_identity(setup, &target.oauth_account)
        })();

        if let Err(e) = write_result {
            let mut problems = Vec::new();
            if let Err(re) = self.restore_live(&service, &setup.user, live_text.as_deref()) {
                problems.push(format!("restoring the Keychain login failed: {re}"));
            }
            if let Some(bytes) = &original_creds_file {
                if let Err(re) = (self.write_file)(&creds_file, bytes) {
                    problems.push(format!("restoring {} failed: {re}", creds_file.display()));
                }
            }
            return Err(if problems.is_empty() {
                Error::Invalid(format!("the switch failed and was rolled back: {e}"))
            } else {
                Error::Invalid(format!("the switch failed ({e}) and the rollback was incomplete: {}. Run `agent-switch doctor`", problems.join("; ")))
            });
        }
        Ok(())
    }

    // ----- add flows -----

    /// Resolve `--from-keychain-item` and `--config-dir`. The default item reads `~/.claude.json`; a hashed
    /// item needs the config dir it came from, whose path must hash to that item name.
    pub fn login_source(&self, env: &Env, service: &str, config_dir: Option<&Path>) -> Result<LoginSource> {
        let config_json = match config_dir {
            Some(dir) => {
                let raw = dir.to_str().ok_or_else(|| Error::Invalid("config dir is not UTF-8".into()))?;
                if keychain_service_name(raw) != service {
                    return Err(Error::Invalid(format!("`{raw}` does not hash to the Keychain item `{service}`; pass the exact CLAUDE_CONFIG_DIR value it was created with")));
                }
                Some(dir.join(".claude.json"))
            }
            None if service == LIVE_SERVICE => Some(env.paths.home().join(".claude.json")),
            None => None,
        };
        Ok(LoginSource { service: service.to_string(), config_json })
    }

    /// Save the login stored under `source` as an account. `--dry-run` reports without writing.
    pub fn import_login(&self, env: &Env, source: &LoginSource, id: Option<&str>, label: Option<&str>, dry_run: bool) -> Result<Imported> {
        self.refuse_overrides()?;
        let setup = self.setup(env);
        let text = self.live.get(&source.service, &setup.user)?.ok_or_else(|| Error::NotFound(format!("Keychain item `{}`", source.service)))?;
        let login = account_bound(&parse_login(&text)?);
        let access = token(&login, "accessToken")?.to_string();
        fingerprint_of(&login)?;

        let config_account = source.config_json.as_ref().and_then(|p| {
            let obj: Obj = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
            obj.get("oauthAccount").filter(|v| v.is_object()).cloned()
        });
        let profile = self.profiles.profile(&access);
        let (oauth_account, uuid, email, plan) = match (config_account, &profile) {
            (Some(acc), Ok(p)) => {
                if acc.get("accountUuid").and_then(Value::as_str) != Some(&p.account_uuid) {
                    return Err(Error::Invalid(
                        "the login's profile and the oauthAccount in its .claude.json are different accounts; refusing to save a mixed identity".into(),
                    ));
                }
                let email = p.email.clone().or_else(|| acc.get("emailAddress").and_then(Value::as_str).map(str::to_string));
                (acc, p.account_uuid.clone(), email, p.plan())
            }
            (Some(acc), Err(_)) => {
                let uuid = acc.get("accountUuid").and_then(Value::as_str).map(str::to_string).ok_or_else(|| Error::Invalid("oauthAccount has no accountUuid".into()))?;
                let email = acc.get("emailAddress").and_then(Value::as_str).map(str::to_string);
                (acc, uuid, email, oauth_of(&login)?.get("subscriptionType").and_then(Value::as_str).map(str::to_string))
            }
            (None, Ok(p)) => (synth_oauth_account(p), p.account_uuid.clone(), p.email.clone(), p.plan()),
            (None, Err(e)) => {
                return Err(Error::Invalid(format!("cannot identify this login: no oauthAccount to read and the profile request failed ({e})")));
            }
        };

        let config = Config::load(env.paths)?;
        let id = match id {
            Some(i) => i.to_string(),
            None => suggest_id(email.as_deref(), &uuid),
        };
        if let Some(other) = config.accounts.iter().find(|a| a.uuid.as_deref() == Some(uuid.as_str()) && a.id != id) {
            return Err(Error::Invalid(format!("this Claude account is already saved as `{}`", other.id)));
        }
        let created = match config.account(&id) {
            Some(a) if a.provider != Provider::Claude => return Err(Error::Invalid(format!("`{id}` is a {} account", a.provider))),
            Some(a) if a.uuid.as_deref().is_some_and(|u| u != uuid) => {
                return Err(Error::Invalid(format!("`{id}` is a different Claude account; choose another --id")));
            }
            Some(_) => false,
            None => true,
        };
        let result = Imported { id: id.clone(), email: email.clone(), plan, created, dry_run };
        if dry_run {
            return Ok(result);
        }

        let slot = ClaudeSlot::new(login, oauth_account).to_slot(now_secs())?;
        let _lock = AccountLock::acquire(env.paths, &id, crate::switch::LOCK_TIMEOUT)?;
        env.vault.put(&id, STORE, &slot)?;
        if created {
            Config::update(env.paths, |config| {
                config.add_account(Account {
                    id: id.clone(),
                    provider: Provider::Claude,
                    label: label.map(str::to_string).unwrap_or_else(|| id.clone()),
                    email,
                    uuid: Some(uuid),
                })
            })?;
        }
        Ok(result)
    }

    /// Log in a new account without logging out: run `claude` in a fresh `CLAUDE_CONFIG_DIR`, then save
    /// the login it creates and delete the temporary folder and its Keychain item.
    pub fn add_interactive(&self, env: &Env, id: Option<&str>, label: Option<&str>) -> Result<Imported> {
        self.refuse_overrides()?;
        let setup = self.setup(env);
        let tmp_root = env.paths.support_dir().join("tmp");
        ensure_private_dir(&tmp_root)?;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let dir = tmp_root.join(format!("claude-add-{}-{nanos:x}", std::process::id()));
        ensure_private_dir(&dir)?;
        let raw = dir.to_str().ok_or_else(|| Error::Invalid("the temporary folder path is not UTF-8".into()))?.to_string();
        let service = keychain_service_name(&raw);

        let result = (|| -> Result<Imported> {
            println!("Claude Code will open with a fresh, empty profile. In it:");
            println!("  1. type /login and finish the sign-in in your browser");
            println!("  2. when it says you are logged in, type /exit");
            println!("Your current Claude login is not touched, and you must not run /logout.");
            let status = Command::new(&self.claude_bin)
                .env("CLAUDE_CONFIG_DIR", &raw)
                .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
                .env_remove("ANTHROPIC_API_KEY")
                .env_remove("ANTHROPIC_AUTH_TOKEN")
                .env_remove("ANTHROPIC_BASE_URL")
                .status()
                .map_err(|e| Error::io(format!("start `{}`", self.claude_bin.display()), e))?;
            if !status.success() {
                return Err(Error::Invalid(format!("claude exited with {status}; no login was saved")));
            }
            // The item can appear a moment after the process ends.
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.live.get(&service, &setup.user)?.is_none() {
                if Instant::now() >= deadline {
                    return Err(Error::NotFound("a login in the new profile (did /login finish?)".into()));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            let source = LoginSource { service: service.clone(), config_json: Some(dir.join(".claude.json")) };
            self.import_login(env, &source, id, label, false)
        })();

        // Cleanup runs on success and failure; only items and folders this call created are removed.
        let item = self.live.delete(&service, &setup.user);
        let folder = std::fs::remove_dir_all(&dir);
        match (&result, item, folder) {
            (Ok(_), Err(e), _) => Err(Error::Keychain(format!("the account was saved, but deleting the temporary Keychain item `{service}` failed: {e}"))),
            (Ok(_), _, Err(e)) => Err(Error::io(format!("the account was saved, but removing {} failed", dir.display()), e)),
            _ => result,
        }
    }

    // ----- doctor -----

    /// Checks for `agent-switch doctor`: storage format, overrides, split state.
    pub fn doctor_checks(&self, env: &Env) -> Vec<Check> {
        let mk = |level, detail: String| Check { level, name: "claude storage", detail };
        let mut out = Vec::new();
        if let Err(e) = self.refuse_overrides() {
            out.push(mk(Level::Fail, e.to_string()));
        }
        let setup = self.setup(env);
        match self.read_live(&setup) {
            Err(e) => out.push(mk(Level::Fail, format!("cannot read the live login without UI: {e}"))),
            Ok((service, None)) => out.push(mk(Level::Warn, format!("no login in Keychain item `{service}` (logged out)"))),
            Ok((service, Some(text))) => match parse_login(&text).and_then(|o| fingerprint_of(&o).map(|fp| (o, fp))) {
                Err(e) => out.push(mk(Level::Fail, format!("item `{service}`: {e}"))),
                Ok((obj, fp)) => {
                    let size = compact_ascii(&obj).map(|t| t.len()).unwrap_or(0);
                    out.push(mk(Level::Ok, format!("item `{service}` holds a parseable login (fingerprint {fp}, {size} bytes compact)")));
                    if size > 3800 {
                        out.push(mk(Level::Warn, "the live login is close to /usr/bin/security's stdin limit; a switch may be refused until unused MCP logins are removed".into()));
                    }
                }
            },
        }
        if setup.credentials_file().exists() {
            out.push(mk(Level::Ok, format!("{} exists: switches rewrite it so a running session notices", setup.credentials_file().display())));
        }
        match self.current_identity(env) {
            Ok(Some(id)) if id.note.as_deref().is_some_and(|n| n.starts_with("split state")) => {
                out.push(mk(Level::Warn, format!("{}. Fix: run `agent-switch switch claude-cli {}` to rewrite both halves", id.note.unwrap_or_default(), id.account_id.unwrap_or_else(|| "<account>".into()))));
            }
            Ok(Some(Identity { account_id: None, .. })) => out.push(mk(
                Level::Warn,
                "the live login is not saved in agent-switch. Fix: `agent-switch add claude-cli --from-keychain-item 'Claude Code-credentials' --id <name>`".into(),
            )),
            Ok(_) => {}
            Err(e) => out.push(mk(Level::Warn, format!("identity check failed: {e}"))),
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// ~/.claude.json: replace one top-level value in place
// ---------------------------------------------------------------------------------------------

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Index just past the string that starts at `i` (which holds the opening quote).
fn skip_string(b: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// Index just past the JSON value that starts at `i`.
fn skip_value(b: &[u8], mut i: usize) -> usize {
    match b[i] {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = skip_string(b, i);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            b.len()
        }
        _ => {
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']') && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            i
        }
    }
}

/// Replace the top-level `oauthAccount` value of a JSON object text, or add it when missing. Everything
/// else, including key order, number spelling and the trailing newline, is left byte for byte. Re-serializing
/// the whole file would not do: Claude Code's number spelling is not what `serde_json` prints.
/// `text` must already be a valid JSON object.
fn splice_oauth_account(text: &str, value: &Value) -> Result<String> {
    const KEY: &str = "oauthAccount";
    let b = text.as_bytes();
    let bad = || Error::Invalid("could not locate the top level of the config file".into());
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return Err(bad());
    }
    i += 1;
    let mut first_indent: Option<String> = None;
    let mut last_end: Option<usize> = None;
    // Indentation of the line a key starts on, when only whitespace precedes it on that line.
    let line_indent = |at: usize| -> Option<String> {
        let line_start = text[..at].rfind('\n')? + 1;
        let lead = &text[line_start..at];
        lead.chars().all(|c| c == ' ' || c == '\t').then(|| lead.to_string())
    };
    let render = |indent: Option<&str>| -> Result<String> {
        let (pretty, indent) = match indent {
            Some(ind) => (serde_json::to_string_pretty(value), ind),
            None => (serde_json::to_string(value), ""),
        };
        Ok(pretty.map_err(|e| Error::Invalid(e.to_string()))?.replace('\n', &format!("\n{indent}")))
    };
    loop {
        i = skip_ws(b, i);
        match b.get(i) {
            None => return Err(bad()),
            Some(b'}') => break,
            Some(b',') => {
                i += 1;
                continue;
            }
            Some(b'"') => {}
            Some(_) => return Err(bad()),
        }
        let key_start = i;
        let key_end = skip_string(b, i);
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return Err(bad());
        }
        i = skip_ws(b, i + 1);
        let value_start = i;
        let value_end = skip_value(b, i);
        let indent = line_indent(key_start);
        if first_indent.is_none() {
            first_indent = indent.clone();
        }
        if &text[key_start + 1..key_end - 1] == KEY {
            return Ok(format!("{}{}{}", &text[..value_start], render(indent.as_deref())?, &text[value_end..]));
        }
        last_end = Some(value_end);
        i = value_end;
    }
    // Missing: append as the last member, matching the file's layout.
    let close = i;
    let multi_line = text.contains('\n');
    let indent = first_indent.unwrap_or_else(|| "  ".to_string());
    let member = |ind: Option<&str>| -> Result<String> { Ok(format!("\"{KEY}\": {}", render(ind)?)) };
    Ok(match last_end {
        Some(end) if multi_line => format!("{},\n{indent}{}{}", &text[..end], member(Some(&indent))?, &text[end..]),
        Some(end) => format!("{},{}{}", &text[..end], member(None)?, &text[end..]),
        None if multi_line => format!("{{\n{indent}{}\n}}{}", member(Some(&indent))?, &text[close + 1..]),
        None => format!("{{{}}}{}", member(None)?, &text[close + 1..]),
    })
}

/// The `oauthAccount` shape Claude Code writes, as far as the profile endpoint can fill it.
fn synth_oauth_account(p: &Profile) -> Value {
    let mut m = Obj::new();
    m.insert("accountUuid".into(), Value::String(p.account_uuid.clone()));
    let optional = [
        ("emailAddress", &p.email),
        ("organizationUuid", &p.organization_uuid),
        ("displayName", &p.display_name),
        ("fullName", &p.full_name),
        ("organizationName", &p.organization_name),
        ("organizationType", &p.organization_type),
        ("organizationRateLimitTier", &p.rate_limit_tier),
    ];
    for (k, v) in optional {
        if let Some(v) = v {
            m.insert(k.to_string(), Value::String(v.clone()));
        }
    }
    Value::Object(m)
}

/// Default account id from the email's local part, or the uuid prefix.
fn suggest_id(email: Option<&str>, uuid: &str) -> String {
    let base: String = email
        .and_then(|e| e.split('@').next())
        .unwrap_or("")
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' })
        .collect();
    let base = base.trim_matches('-');
    let base = if base.is_empty() { format!("claude-{}", uuid.chars().take(6).collect::<String>()) } else { format!("claude-{base}") };
    base.chars().take(32).collect::<String>().trim_end_matches('-').to_string()
}

impl Surface for ClaudeCode {
    fn kind(&self) -> SurfaceKind {
        SurfaceKind::ClaudeCli
    }

    fn current_identity(&self, env: &Env) -> Result<Option<Identity>> {
        let setup = self.setup(env);
        let (_, text) = self.read_live(&setup)?;
        let Some(text) = text else { return Ok(None) };
        let live = parse_login(&text)?;
        let fp = fingerprint_of(&live)?;
        let saved = self.saved_logins(env)?;
        let config_account = self.oauth_account_in_config(&setup);
        let config_uuid = config_account.as_ref().and_then(|a| a.get("accountUuid")).and_then(Value::as_str).map(str::to_string);
        let config_email = config_account.as_ref().and_then(|a| a.get("emailAddress")).and_then(Value::as_str).map(str::to_string);

        let by_fp = saved.iter().find(|s| s.fp == fp);
        let mut note = None;
        let found = match by_fp {
            Some(s) => Some(s),
            // A refreshed login has a new refresh token; the identity half still names the account.
            None => saved.iter().find(|s| config_uuid.is_some() && s.slot.as_ref().and_then(ClaudeSlot::account_uuid) == config_uuid.as_deref()),
        };
        let Some(found) = found else {
            let mut id = Identity { email: config_email, uuid: config_uuid, ..Identity::default() };
            id.note = Some("login not saved in agent-switch".into());
            return Ok(Some(id));
        };
        let slot_uuid = found.slot.as_ref().and_then(ClaudeSlot::account_uuid).map(str::to_string);
        if by_fp.is_some() && config_uuid.is_some() && slot_uuid.is_some() && config_uuid != slot_uuid {
            note = Some(format!(
                "split state: the login belongs to `{}` but ~/.claude.json names another account",
                found.id
            ));
        } else if by_fp.is_none() {
            note = Some("login refreshed since it was saved".into());
        }
        Ok(Some(Identity {
            account_id: Some(found.id.clone()),
            email: found.slot.as_ref().and_then(|s| s.email().map(str::to_string)).or(config_email),
            uuid: slot_uuid,
            note,
        }))
    }

    /// Read the live login into a slot shape (account-bound keys plus the `oauthAccount` of `~/.claude.json`).
    fn capture(&self, env: &Env, _account: &Account) -> Result<Option<Slot>> {
        let setup = self.setup(env);
        let (_, text) = self.read_live(&setup)?;
        let Some(text) = text else { return Ok(None) };
        let login = account_bound(&parse_login(&text)?);
        let oauth_account = self.oauth_account_in_config(&setup).ok_or_else(|| Error::Invalid("~/.claude.json has no oauthAccount".into()))?;
        Ok(Some(ClaudeSlot::new(login, oauth_account).to_slot(now_secs())?))
    }

    fn stage(&self, env: &Env, account: &Account) -> Result<Staged> {
        self.refuse_overrides()?;
        let slot = env
            .vault
            .get(&account.id, STORE)?
            .ok_or_else(|| Error::NotFound(format!("saved login for `{}`; run `agent-switch add claude-cli`", account.id)))?;
        ClaudeSlot::parse(&slot.blob)?;

        // Ask who owns the live login now, before any lock is held (network under locks is forbidden).
        let setup = self.setup(env);
        let mut probe = None;
        if let (_, Some(text)) = self.read_live(&setup)? {
            let live = parse_login(&text)?;
            let fp = fingerprint_of(&live)?;
            if !self.saved_logins(env)?.iter().any(|s| s.fp == fp) {
                let access = token(&live, "accessToken")?;
                let answer = self.profiles.profile(access);
                probe = Some(Probe {
                    access_fp: lineage_fingerprint(access),
                    uuid: answer.as_ref().ok().map(|p| p.account_uuid.clone()),
                    error: answer.err().map(|e| e.to_string()),
                });
            }
        }
        let payload = serde_json::to_vec(&StagePayload { slot: slot.blob, probe }).map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(Staged::new(account.id.clone(), payload))
    }

    fn apply(&self, env: &Env, staged: Staged) -> Result<()> {
        self.refuse_overrides()?;
        let payload: StagePayload = serde_json::from_slice(&staged.payload).map_err(|e| Error::Invalid(format!("bad staged payload: {e}")))?;
        let setup = self.setup(env);
        // The owner of the live login is saved back under its account lock. Take that lock before Claude
        // Code's directory locks, so waiting for another process never happens while those are held.
        let mut locked = staged.held.clone();
        let _owner_lock = match self.live_owner(env, &setup, &payload)? {
            Some(id) if !locked.contains(&id) => {
                let lock = AccountLock::acquire(env.paths, &id, crate::switch::LOCK_TIMEOUT)?;
                locked.push(id);
                Some(lock)
            }
            _ => None,
        };
        // Claude Code's order: the primary refresh lock, then the legacy one (claude-swap `claude_locks.py`).
        let _refresh = DirLock::acquire(&setup.refresh_lock(), DirLockOpts::credentials())?;
        let _legacy = DirLock::acquire(&setup.legacy_lock(), DirLockOpts::credentials())?;
        self.transact(env, &setup, &payload, &staged.account_id, &locked)
    }
}

#[cfg(test)]
mod tests;
