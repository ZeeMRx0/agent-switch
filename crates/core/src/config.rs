//! `~/.config/agent-switch/config.toml`: accounts, the binding of each surface, the per-surface account
//! priority (`[priority]`) and the `[auto]` settings.
//!
//! The binding is the user's choice and lives only here. Which account is currently live, and any pending
//! record, live in `state.json` (see `state.rs`); auto-switch never edits this file.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fsutil::{ensure_private_dir, safe_write, WriteOpts};
use crate::locks::FlockGuard;
use crate::paths::Paths;

const CONFIG_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Openai,
}

impl Provider {
    /// Keychain slot name for this provider's login (`<id>:<store>`).
    pub fn store(self) -> &'static str {
        match self {
            Provider::Claude => "claude-oauth",
            Provider::Openai => "openai-auth",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Provider::Claude => "claude",
            Provider::Openai => "openai",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SurfaceKind {
    ClaudeCli,
    ClaudeDesktop,
    CodexCli,
    ChatgptDesktop,
}

impl SurfaceKind {
    pub const ALL: [SurfaceKind; 4] =
        [SurfaceKind::ClaudeCli, SurfaceKind::ClaudeDesktop, SurfaceKind::CodexCli, SurfaceKind::ChatgptDesktop];

    pub fn name(self) -> &'static str {
        match self {
            SurfaceKind::ClaudeCli => "claude-cli",
            SurfaceKind::ClaudeDesktop => "claude-desktop",
            SurfaceKind::CodexCli => "codex-cli",
            SurfaceKind::ChatgptDesktop => "chatgpt-desktop",
        }
    }

    pub fn provider(self) -> Provider {
        match self {
            SurfaceKind::ClaudeCli | SurfaceKind::ClaudeDesktop => Provider::Claude,
            SurfaceKind::CodexCli | SurfaceKind::ChatgptDesktop => Provider::Openai,
        }
    }

    /// Desktop surfaces are only ever restarted on an explicit user action.
    pub fn is_desktop(self) -> bool {
        matches!(self, SurfaceKind::ClaudeDesktop | SurfaceKind::ChatgptDesktop)
    }
}

impl fmt::Display for SurfaceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for SurfaceKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        SurfaceKind::ALL
            .into_iter()
            .find(|k| k.name() == s)
            .ok_or_else(|| Error::Invalid(format!("unknown surface `{s}` (expected one of: {})", surface_names())))
    }
}

pub fn surface_names() -> String {
    SurfaceKind::ALL.map(|k| k.name()).join(", ")
}

const UNMANAGED: &str = "unmanaged";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Binding {
    #[default]
    Unmanaged,
    Account(String),
}

impl Binding {
    pub fn account_id(&self) -> Option<&str> {
        match self {
            Binding::Unmanaged => None,
            Binding::Account(id) => Some(id),
        }
    }

    /// Parse the CLI/TOML spelling: `unmanaged` or an account id.
    pub fn parse(s: &str) -> Binding {
        if s == UNMANAGED {
            Binding::Unmanaged
        } else {
            Binding::Account(s.to_string())
        }
    }
}

impl Serialize for Binding {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.account_id().unwrap_or(UNMANAGED))
    }
}

impl<'de> Deserialize<'de> for Binding {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(Binding::parse(&String::deserialize(d)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub provider: Provider,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Provider-side account identifier (Claude `accountUuid`, OpenAI `account_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

/// Account ids become Keychain account names and lock file names, so keep them boring.
pub fn validate_account_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 32
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok || id == UNMANAGED {
        return Err(Error::Config(format!(
            "invalid account id `{id}`: use 1-32 chars of a-z, 0-9, `-`, `_`, starting with a letter or digit (`{UNMANAGED}` is reserved)"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Surfaces {
    pub claude_cli: Binding,
    pub claude_desktop: Binding,
    pub codex_cli: Binding,
    pub chatgpt_desktop: Binding,
}

impl Surfaces {
    pub fn get(&self, kind: SurfaceKind) -> &Binding {
        match kind {
            SurfaceKind::ClaudeCli => &self.claude_cli,
            SurfaceKind::ClaudeDesktop => &self.claude_desktop,
            SurfaceKind::CodexCli => &self.codex_cli,
            SurfaceKind::ChatgptDesktop => &self.chatgpt_desktop,
        }
    }

    fn slot(&mut self, kind: SurfaceKind) -> &mut Binding {
        match kind {
            SurfaceKind::ClaudeCli => &mut self.claude_cli,
            SurfaceKind::ClaudeDesktop => &mut self.claude_desktop,
            SurfaceKind::CodexCli => &mut self.codex_cli,
            SurfaceKind::ChatgptDesktop => &mut self.chatgpt_desktop,
        }
    }
}

/// The order in which auto-switch tries the accounts of each surface, highest first. A surface without a list
/// puts its bound account first, then config order. Ids that are unknown or belong to the other provider are
/// ignored, so the file never fails to load because of this table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Priority {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub claude_cli: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub claude_desktop: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub codex_cli: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chatgpt_desktop: Vec<String>,
}

impl Priority {
    pub fn get(&self, kind: SurfaceKind) -> &[String] {
        match kind {
            SurfaceKind::ClaudeCli => &self.claude_cli,
            SurfaceKind::ClaudeDesktop => &self.claude_desktop,
            SurfaceKind::CodexCli => &self.codex_cli,
            SurfaceKind::ChatgptDesktop => &self.chatgpt_desktop,
        }
    }

    fn slot(&mut self, kind: SurfaceKind) -> &mut Vec<String> {
        match kind {
            SurfaceKind::ClaudeCli => &mut self.claude_cli,
            SurfaceKind::ClaudeDesktop => &mut self.claude_desktop,
            SurfaceKind::CodexCli => &mut self.codex_cli,
            SurfaceKind::ChatgptDesktop => &mut self.chatgpt_desktop,
        }
    }

    fn is_empty(&self) -> bool {
        SurfaceKind::ALL.into_iter().all(|k| self.get(k).is_empty())
    }
}

/// Auto-switch on or off per surface. A surface without a value follows `auto.enabled`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct AutoSurfaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_cli: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_desktop: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_cli: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chatgpt_desktop: Option<bool>,
}

impl AutoSurfaces {
    pub fn get(&self, kind: SurfaceKind) -> Option<bool> {
        match kind {
            SurfaceKind::ClaudeCli => self.claude_cli,
            SurfaceKind::ClaudeDesktop => self.claude_desktop,
            SurfaceKind::CodexCli => self.codex_cli,
            SurfaceKind::ChatgptDesktop => self.chatgpt_desktop,
        }
    }

    fn slot(&mut self, kind: SurfaceKind) -> &mut Option<bool> {
        match kind {
            SurfaceKind::ClaudeCli => &mut self.claude_cli,
            SurfaceKind::ClaudeDesktop => &mut self.claude_desktop,
            SurfaceKind::CodexCli => &mut self.codex_cli,
            SurfaceKind::ChatgptDesktop => &mut self.chatgpt_desktop,
        }
    }

    fn is_unset(&self) -> bool {
        SurfaceKind::ALL.into_iter().all(|k| self.get(k).is_none())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexCliMode {
    /// Switch the login now; running sessions keep the old account until restarted.
    #[default]
    NewSessions,
    /// Never switch automatically; notify instead.
    Notify,
    Force,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Auto {
    /// Auto-switch for every surface that has no value in `surfaces`.
    pub enabled: bool,
    pub threshold: u8,
    pub hysteresis: u8,
    pub cooldown_minutes: u32,
    pub refresh_inactive: bool,
    pub codex_cli_mode: CodexCliMode,
    /// `[auto.surfaces]`: auto-switch per surface. Old files have no such table.
    #[serde(skip_serializing_if = "AutoSurfaces::is_unset")]
    pub surfaces: AutoSurfaces,
}

impl Auto {
    /// Whether auto-switch acts on `kind`.
    pub fn enabled_for(&self, kind: SurfaceKind) -> bool {
        self.surfaces.get(kind).unwrap_or(self.enabled)
    }

    /// Turn auto-switch on or off for `kind` only. Every surface first gets its current value written out,
    /// so a later change of `enabled` does not silently change the other surfaces.
    pub fn set_enabled_for(&mut self, kind: SurfaceKind, on: bool) {
        for k in SurfaceKind::ALL {
            let current = self.enabled_for(k);
            *self.surfaces.slot(k) = Some(current);
        }
        *self.surfaces.slot(kind) = Some(on);
    }

    /// Whether auto-switch acts on at least one surface.
    pub fn any_enabled(&self) -> bool {
        SurfaceKind::ALL.into_iter().any(|k| self.enabled_for(k))
    }
}

impl Default for Auto {
    fn default() -> Self {
        Auto {
            enabled: false,
            threshold: 90,
            hysteresis: 5,
            cooldown_minutes: 5,
            refresh_inactive: false,
            codex_cli_mode: CodexCliMode::default(),
            surfaces: AutoSurfaces::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub accounts: Vec<Account>,
    pub surfaces: Surfaces,
    #[serde(skip_serializing_if = "Priority::is_empty")]
    pub priority: Priority,
    pub auto: Auto,
}

impl Config {
    /// Missing file means an empty config; a present but invalid file is an error.
    pub fn load(paths: &Paths) -> Result<Config> {
        let file = paths.config_file();
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(Error::io(format!("read {}", file.display()), e)),
        };
        let config: Config = toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", file.display())))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        self.validate()?;
        let file = paths.config_file();
        if let Some(dir) = file.parent() {
            ensure_private_dir(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        safe_write(&file, text.as_bytes(), &WriteOpts::secret())
    }

    /// Load, change and save under a cross-process lock, so two writers (the CLI and the menu bar) cannot
    /// lose each other's change. Nothing is saved when `change` fails.
    pub fn update<T>(paths: &Paths, change: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        let _guard = FlockGuard::lock(&paths.config_lock_file(), CONFIG_LOCK_TIMEOUT)?;
        let mut config = Config::load(paths)?;
        let out = change(&mut config)?;
        config.save(paths)?;
        Ok(out)
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for a in &self.accounts {
            validate_account_id(&a.id)?;
            if !seen.insert(a.id.as_str()) {
                return Err(Error::Config(format!("duplicate account id `{}`", a.id)));
            }
        }
        for kind in SurfaceKind::ALL {
            if let Some(id) = self.surfaces.get(kind).account_id() {
                self.check_bindable(kind, id)?;
            }
        }
        let a = &self.auto;
        if !(1..=100).contains(&a.threshold) || a.hysteresis >= a.threshold {
            return Err(Error::Config("auto.threshold must be 1-100 and larger than auto.hysteresis".into()));
        }
        Ok(())
    }

    fn check_bindable(&self, kind: SurfaceKind, id: &str) -> Result<()> {
        let account = self
            .account(id)
            .ok_or_else(|| Error::Config(format!("surface `{kind}` is bound to unknown account `{id}`")))?;
        if account.provider != kind.provider() {
            return Err(Error::Config(format!(
                "surface `{kind}` needs a {} account, but `{id}` is a {} account",
                kind.provider(),
                account.provider
            )));
        }
        Ok(())
    }

    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn add_account(&mut self, account: Account) -> Result<()> {
        validate_account_id(&account.id)?;
        if self.account(&account.id).is_some() {
            return Err(Error::Config(format!("account `{}` already exists", account.id)));
        }
        self.accounts.push(account);
        Ok(())
    }

    /// Surfaces currently bound to `id`.
    pub fn surfaces_bound_to(&self, id: &str) -> Vec<SurfaceKind> {
        SurfaceKind::ALL.into_iter().filter(|k| self.surfaces.get(*k).account_id() == Some(id)).collect()
    }

    pub fn bind(&mut self, kind: SurfaceKind, binding: Binding) -> Result<()> {
        if let Some(id) = binding.account_id() {
            self.check_bindable(kind, id)?;
        }
        *self.surfaces.slot(kind) = binding;
        Ok(())
    }

    /// Remove an account entry. The caller must also have checked state (active accounts).
    pub fn remove_account(&mut self, id: &str) -> Result<Account> {
        let bound = self.surfaces_bound_to(id);
        if !bound.is_empty() {
            let names: Vec<_> = bound.iter().map(|k| k.name()).collect();
            return Err(Error::Invalid(format!("account `{id}` is bound to {}; bind them elsewhere first", names.join(", "))));
        }
        let idx = self.accounts.iter().position(|a| a.id == id).ok_or_else(|| Error::NotFound(format!("account `{id}`")))?;
        for kind in SurfaceKind::ALL {
            self.priority.slot(kind).retain(|p| p != id);
        }
        Ok(self.accounts.remove(idx))
    }

    /// The accounts of `kind`'s provider in auto-switch order, highest first: the saved priority list (known
    /// ids of the right provider, each once), then every other account of the provider in config order.
    /// Without a saved list the surface's bound account comes first, so an old config file never makes
    /// auto-switch move a surface away from the account the user bound it to.
    pub fn priority(&self, kind: SurfaceKind) -> Vec<String> {
        let provider = kind.provider();
        let saved = self.priority.get(kind);
        let seed: Vec<String> = if saved.is_empty() { self.surfaces.get(kind).account_id().map(str::to_string).into_iter().collect() } else { saved.to_vec() };
        let mut out: Vec<String> = Vec::new();
        for id in &seed {
            if self.account(id).is_some_and(|a| a.provider == provider) && !out.contains(id) {
                out.push(id.clone());
            }
        }
        for a in self.accounts.iter().filter(|a| a.provider == provider) {
            if !out.contains(&a.id) {
                out.push(a.id.clone());
            }
        }
        out
    }

    /// Make `id` the first account in `kind`'s priority and save the full order. A manual choice of an
    /// account does this, so auto-switch returns to what the user picked rather than away from it.
    pub fn promote(&mut self, kind: SurfaceKind, id: &str) -> Result<()> {
        let mut order = self.priority(kind);
        let i = self.priority_index(kind, &order, id)?;
        let first = order.remove(i);
        order.insert(0, first);
        *self.priority.slot(kind) = order;
        Ok(())
    }

    fn priority_index(&self, kind: SurfaceKind, order: &[String], id: &str) -> Result<usize> {
        order
            .iter()
            .position(|p| p == id)
            .ok_or_else(|| Error::NotFound(format!("{} account `{id}` for the priority of {kind}", kind.provider())))
    }

    /// Move `id` one place up (toward the top) or down in `kind`'s priority, and save the full order. An
    /// account already at that end stays where it is.
    pub fn move_priority(&mut self, kind: SurfaceKind, id: &str, up: bool) -> Result<()> {
        let mut order = self.priority(kind);
        let i = self.priority_index(kind, &order, id)?;
        let j = if up { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < order.len()) };
        if let Some(j) = j {
            order.swap(i, j);
            *self.priority.slot(kind) = order;
        }
        Ok(())
    }
}

/// `ze***@domain`: enough to recognise an account without printing the address.
pub fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            let head: String = local.chars().take(2).collect();
            format!("{head}***@{domain}")
        }
        None => "***".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(id: &str, provider: Provider) -> Account {
        Account { id: id.into(), provider, label: id.into(), email: None, uuid: None }
    }

    fn sample() -> Config {
        let mut c = Config::default();
        c.add_account(acct("personal", Provider::Claude)).unwrap();
        c.add_account(acct("work-gpt", Provider::Openai)).unwrap();
        c
    }

    #[test]
    fn concurrent_updates_do_not_lose_each_others_changes() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        sample().save(&paths).unwrap();
        std::thread::scope(|scope| {
            for n in 0..8 {
                let paths = &paths;
                scope.spawn(move || Config::update(paths, |c| c.add_account(acct(&format!("extra-{n}"), Provider::Claude))).unwrap());
            }
        });
        assert_eq!(Config::load(&paths).unwrap().accounts.len(), 2 + 8);
    }

    #[test]
    fn a_failed_update_saves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        sample().save(&paths).unwrap();
        let err = Config::update(&paths, |c| {
            c.add_account(acct("fresh", Provider::Claude))?;
            c.bind(SurfaceKind::ClaudeCli, Binding::parse("ghost"))
        });
        assert!(err.is_err());
        assert!(Config::load(&paths).unwrap().account("fresh").is_none());
    }

    #[test]
    fn binding_requires_existing_account() {
        let mut c = sample();
        let err = c.bind(SurfaceKind::ClaudeCli, Binding::parse("ghost")).unwrap_err();
        assert!(err.to_string().contains("unknown account"), "{err}");
    }

    #[test]
    fn binding_requires_matching_provider() {
        let mut c = sample();
        assert!(c.bind(SurfaceKind::ClaudeCli, Binding::parse("work-gpt")).is_err());
        assert!(c.bind(SurfaceKind::ChatgptDesktop, Binding::parse("personal")).is_err());
        c.bind(SurfaceKind::ClaudeCli, Binding::parse("personal")).unwrap();
        c.bind(SurfaceKind::CodexCli, Binding::parse("work-gpt")).unwrap();
        assert_eq!(c.surfaces_bound_to("personal"), vec![SurfaceKind::ClaudeCli]);
    }

    #[test]
    fn unmanaged_round_trips_and_is_the_default() {
        let mut c = sample();
        assert_eq!(*c.surfaces.get(SurfaceKind::ClaudeDesktop), Binding::Unmanaged);
        c.bind(SurfaceKind::ClaudeCli, Binding::parse("personal")).unwrap();
        c.bind(SurfaceKind::ClaudeCli, Binding::Unmanaged).unwrap();
        assert_eq!(*c.surfaces.get(SurfaceKind::ClaudeCli), Binding::Unmanaged);
    }

    #[test]
    fn load_rejects_a_file_with_a_bad_binding() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        std::fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[[accounts]]\nid = \"a\"\nprovider = \"openai\"\n[surfaces]\nclaude-cli = \"a\"\n",
        )
        .unwrap();
        assert!(matches!(Config::load(&paths), Err(Error::Config(_))));
    }

    #[test]
    fn save_then_load_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        let mut c = sample();
        c.bind(SurfaceKind::ClaudeCli, Binding::parse("personal")).unwrap();
        c.auto.enabled = true;
        c.auto.codex_cli_mode = CodexCliMode::Notify;
        c.save(&paths).unwrap();
        assert_eq!(Config::load(&paths).unwrap(), c);
        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(text.contains("claude-cli = \"personal\""), "{text}");
        assert!(text.contains("claude-desktop = \"unmanaged\""), "{text}");
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&Paths::from_home(d.path())).unwrap(), Config::default());
    }

    #[test]
    fn rejects_duplicate_and_unsafe_ids() {
        let mut c = sample();
        assert!(c.add_account(acct("personal", Provider::Claude)).is_err());
        for bad in ["", "Upper", "a/b", "a:b", "../x", "unmanaged", "-x", &"a".repeat(33)] {
            assert!(validate_account_id(bad).is_err(), "{bad}");
        }
        assert!(validate_account_id("team_2-b").is_ok());
    }

    #[test]
    fn rejects_bad_auto_values() {
        let mut c = sample();
        c.auto.threshold = 0;
        assert!(c.validate().is_err());
        c.auto.threshold = 90;
        c.auto.hysteresis = 90;
        assert!(c.validate().is_err());
    }

    #[test]
    fn remove_refuses_while_bound() {
        let mut c = sample();
        c.bind(SurfaceKind::ClaudeCli, Binding::parse("personal")).unwrap();
        assert!(c.remove_account("personal").is_err());
        c.bind(SurfaceKind::ClaudeCli, Binding::Unmanaged).unwrap();
        assert_eq!(c.remove_account("personal").unwrap().id, "personal");
        assert!(c.remove_account("personal").is_err());
    }

    fn claude3() -> Config {
        let mut c = Config::default();
        for id in ["a", "b", "c"] {
            c.add_account(acct(id, Provider::Claude)).unwrap();
        }
        c.add_account(acct("gpt", Provider::Openai)).unwrap();
        c
    }

    #[test]
    fn priority_is_the_saved_list_then_the_rest_in_config_order() {
        let mut c = claude3();
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["a", "b", "c"], "no list: config order");
        assert_eq!(c.priority(SurfaceKind::CodexCli), ["gpt"]);
        // Unknown ids, ids of the other provider and repeats are ignored; missing accounts go last.
        c.priority.claude_cli = vec!["c".into(), "ghost".into(), "gpt".into(), "c".into(), "a".into()];
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["c", "a", "b"]);
        assert_eq!(c.priority(SurfaceKind::ClaudeDesktop), ["a", "b", "c"], "each surface has its own list");
        c.validate().unwrap();
    }

    #[test]
    fn moving_an_account_swaps_it_with_its_neighbour_and_stops_at_the_ends() {
        let mut c = claude3();
        c.move_priority(SurfaceKind::ClaudeCli, "c", true).unwrap();
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["a", "c", "b"]);
        assert_eq!(c.priority.claude_cli, ["a", "c", "b"], "the full order is saved");
        c.move_priority(SurfaceKind::ClaudeCli, "a", false).unwrap();
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["c", "a", "b"]);
        // At the ends nothing moves.
        c.move_priority(SurfaceKind::ClaudeCli, "c", true).unwrap();
        c.move_priority(SurfaceKind::ClaudeCli, "b", false).unwrap();
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["c", "a", "b"]);
        // Unknown ids and accounts of the other provider are refused.
        assert!(matches!(c.move_priority(SurfaceKind::ClaudeCli, "ghost", true), Err(Error::NotFound(_))));
        assert!(c.move_priority(SurfaceKind::ClaudeCli, "gpt", true).is_err());
        assert!(c.priority.claude_desktop.is_empty(), "other surfaces are untouched");
    }

    #[test]
    fn without_a_saved_list_the_bound_account_comes_first() {
        let mut c = claude3();
        c.bind(SurfaceKind::ClaudeCli, Binding::parse("b")).unwrap();
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["b", "a", "c"]);
        assert_eq!(c.priority(SurfaceKind::ClaudeDesktop), ["a", "b", "c"], "an unmanaged surface uses config order");
        assert!(c.priority.claude_cli.is_empty(), "the default is not written to the file");
        // A saved list wins over the binding.
        c.priority.claude_cli = vec!["c".into()];
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["c", "a", "b"]);
    }

    #[test]
    fn promoting_an_account_puts_it_first_and_keeps_the_rest_in_order() {
        let mut c = claude3();
        c.priority.claude_cli = vec!["b".into(), "c".into(), "a".into()];
        c.promote(SurfaceKind::ClaudeCli, "a").unwrap();
        assert_eq!(c.priority.claude_cli, ["a", "b", "c"]);
        c.promote(SurfaceKind::ClaudeCli, "a").unwrap();
        assert_eq!(c.priority.claude_cli, ["a", "b", "c"], "already first: no change");
        c.promote(SurfaceKind::ClaudeDesktop, "c").unwrap();
        assert_eq!(c.priority.claude_desktop, ["c", "a", "b"]);
        assert!(c.promote(SurfaceKind::ClaudeCli, "gpt").is_err());
        assert!(c.promote(SurfaceKind::ClaudeCli, "ghost").is_err());
    }

    #[test]
    fn a_new_account_goes_last_and_a_removed_one_leaves_every_list() {
        let mut c = claude3();
        c.move_priority(SurfaceKind::ClaudeCli, "b", true).unwrap();
        c.move_priority(SurfaceKind::ClaudeDesktop, "b", true).unwrap();
        c.add_account(acct("d", Provider::Claude)).unwrap();
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["b", "a", "c", "d"]);
        c.remove_account("b").unwrap();
        assert_eq!(c.priority.claude_cli, ["a", "c"]);
        assert_eq!(c.priority.claude_desktop, ["a", "c"]);
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["a", "c", "d"]);
    }

    #[test]
    fn auto_per_surface_falls_back_to_enabled_until_one_is_set() {
        let mut auto = Auto::default();
        assert!(!auto.any_enabled());
        auto.enabled = true;
        assert!(SurfaceKind::ALL.into_iter().all(|k| auto.enabled_for(k)), "no table: every surface follows enabled");
        auto.set_enabled_for(SurfaceKind::ClaudeDesktop, false);
        assert!(!auto.enabled_for(SurfaceKind::ClaudeDesktop));
        assert!(auto.enabled_for(SurfaceKind::ClaudeCli) && auto.enabled_for(SurfaceKind::CodexCli) && auto.enabled_for(SurfaceKind::ChatgptDesktop));
        // The other surfaces were written out, so `enabled` no longer changes them.
        auto.enabled = false;
        assert!(auto.enabled_for(SurfaceKind::ClaudeCli));
        for k in SurfaceKind::ALL {
            auto.set_enabled_for(k, false);
        }
        assert!(!auto.any_enabled());
    }

    #[test]
    fn an_old_config_file_loads_with_auto_on_everywhere_and_config_order() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        std::fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
        let old = "[[accounts]]\nid = \"a\"\nprovider = \"claude\"\n[[accounts]]\nid = \"b\"\nprovider = \"claude\"\n\
                   [surfaces]\nclaude-cli = \"a\"\n[auto]\nenabled = true\nthreshold = 90\n";
        std::fs::write(paths.config_file(), old).unwrap();
        let c = Config::load(&paths).unwrap();
        assert!(SurfaceKind::ALL.into_iter().all(|k| c.auto.enabled_for(k)));
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["a", "b"]);
        // Saving it again adds neither table.
        c.save(&paths).unwrap();
        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(!text.contains("priority") && !text.contains("[auto.surfaces]"), "{text}");
    }

    #[test]
    fn priority_and_auto_surfaces_round_trip_through_the_file() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        let mut c = claude3();
        c.move_priority(SurfaceKind::ClaudeCli, "c", true).unwrap();
        c.auto.set_enabled_for(SurfaceKind::CodexCli, true);
        c.save(&paths).unwrap();
        assert_eq!(Config::load(&paths).unwrap(), c);
        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(text.contains("[priority]") && text.contains("claude-cli = ["), "{text}");
        assert!(text.contains("[auto.surfaces]") && text.contains("codex-cli = true") && text.contains("claude-cli = false"), "{text}");
        // A file with only some surfaces listed, and unknown ids in a list, still loads.
        std::fs::write(
            paths.config_file(),
            "[[accounts]]\nid = \"a\"\nprovider = \"claude\"\n[priority]\nclaude-cli = [\"ghost\", \"a\"]\n[auto]\nenabled = true\n[auto.surfaces]\nclaude-desktop = false\n",
        )
        .unwrap();
        let c = Config::load(&paths).unwrap();
        assert_eq!(c.priority(SurfaceKind::ClaudeCli), ["a"]);
        assert!(c.auto.enabled_for(SurfaceKind::ClaudeCli) && !c.auto.enabled_for(SurfaceKind::ClaudeDesktop));
    }

    #[test]
    fn surface_names_parse() {
        assert_eq!("codex-cli".parse::<SurfaceKind>().unwrap(), SurfaceKind::CodexCli);
        assert!("codex".parse::<SurfaceKind>().is_err());
    }

    #[test]
    fn masks_emails() {
        assert_eq!(mask_email("zee@example.com"), "ze***@example.com");
        assert_eq!(mask_email("nope"), "***");
    }
}
