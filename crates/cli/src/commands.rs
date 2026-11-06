use std::path::PathBuf;
use std::sync::Arc;

use agent_switch_core::adapters::claude_desktop;
use agent_switch_core::config::{mask_email, Binding, Config, SurfaceKind};
use agent_switch_core::doctor::{self, DoctorInput, Level};
use agent_switch_core::macos_apps::SystemApps;
use agent_switch_core::paths::Paths;
use agent_switch_core::signing::detect_keychain;
use agent_switch_core::state::State;
use agent_switch_core::surface::{Env, Surface};
use agent_switch_core::switch;
use agent_switch_core::vault::Vault;
use agent_switch_core::{Error, Result};

use crate::registry::{surface_for, surface_for_switch, switch_note};

/// Process exit code on success paths that still report a problem (doctor failures).
type Code = i32;

pub(crate) struct App {
    pub(crate) paths: Paths,
    pub(crate) vault: Vault,
    exe: PathBuf,
}

impl App {
    pub(crate) fn open() -> Result<App> {
        let paths = Paths::from_env()?;
        let exe = std::env::current_exe().map_err(|e| Error::io("find the running executable", e))?;
        let vault = Vault::new(Arc::from(detect_keychain(&exe)));
        Ok(App { paths, vault, exe })
    }

    pub(crate) fn env(&self) -> Env<'_> {
        Env { paths: &self.paths, vault: &self.vault }
    }
}

fn dash(s: Option<&str>) -> String {
    s.unwrap_or("-").to_string()
}

pub fn status() -> Result<Code> {
    let app = App::open()?;
    let config = Config::load(&app.paths)?;
    let state = State::load(&app.paths)?;
    println!("{:<16} {:<12} {:<12} {:<28} live", "surface", "bound", "active", "pending");
    for kind in SurfaceKind::ALL {
        let binding = config.surfaces.get(kind).account_id();
        let pending = state
            .pending(kind)
            .map(|p| format!("{} ({})", p.target_account, p.reason))
            .unwrap_or_else(|| "-".into());
        let live = match surface_for(kind).current_identity(&app.env()) {
            Ok(Some(id)) => {
                let who = id.email.as_deref().map(mask_email).or(id.account_id).unwrap_or_else(|| "unknown".into());
                match id.note {
                    Some(note) => format!("{who} [{note}]"),
                    None => who,
                }
            }
            Ok(None) => "logged out".into(),
            Err(e) => format!("n/a ({e})"),
        };
        println!(
            "{:<16} {:<12} {:<12} {:<28} {}",
            kind.name(),
            binding.unwrap_or("unmanaged"),
            dash(state.active(kind)),
            pending,
            live
        );
    }
    Ok(0)
}

pub fn switch(surface: &str, account: &str, restart: bool, force: bool) -> Result<Code> {
    let kind: SurfaceKind = surface.parse()?;
    let app = App::open()?;
    let config = Config::load(&app.paths)?;
    if config.surfaces.get(kind).account_id().is_none() {
        return Err(Error::Invalid(format!("`{kind}` is unmanaged; run `agent-switch bind {kind} {account}` first")));
    }
    // `--force` only matters to Claude Desktop (a session mid-reply); other surfaces ignore it.
    let desktop = (kind == SurfaceKind::ClaudeDesktop).then(|| {
        let d = claude_desktop::ClaudeDesktop::production();
        let d = if restart { d.with_restart() } else { d };
        if force { d.with_force() } else { d }
    });
    let other;
    let surf: &dyn Surface = match &desktop {
        Some(d) => d,
        None => {
            other = surface_for_switch(kind, restart);
            other.as_ref()
        }
    };
    let env = app.env();
    switch::activate(&env, &config, surf, account)?;
    // A manual switch is the user's choice: auto-switch should return to it, not away from it. The surface
    // is on the account once it is activated, so a failed restart does not change that, and a failed
    // promote must not leave a quit app closed.
    let promoted = switch::promote(&app.paths, kind, account);
    if restart {
        surf.restart(&env)?;
    }
    promoted?;
    println!("{kind}: switched to {account}");
    if let Some(d) = &desktop {
        if let Some(line) = d.mirror_summary() {
            println!("{line}");
        }
        if let Some(warning) = d.mirror_warning() {
            println!("warning: {warning}");
        }
    }
    if let Some(note) = switch_note(kind) {
        println!("{note}");
    }
    if config.surfaces.get(kind).account_id() != Some(account) {
        println!("note: the binding is still `{}`; `bind` changes it", config.surfaces.get(kind).account_id().unwrap_or(""));
    }
    Ok(0)
}

pub fn undo(surface: Option<&str>) -> Result<Code> {
    let app = App::open()?;
    let config = Config::load(&app.paths)?;
    let kind = match surface {
        Some(s) => s.parse()?,
        None => switch::undo_record(&app.paths, None)?.surface,
    };
    let restored = switch::undo(&app.env(), &config, surface_for(kind).as_ref())?;
    println!("{kind}: restored {restored}");
    if let Some(note) = switch_note(kind) {
        println!("{note}");
    }
    Ok(0)
}

/// Options of `agent-switch add`. Each surface reads the ones it understands.
pub struct AddOptions {
    /// The account for `add claude-desktop <account>`.
    pub account: Option<String>,
    pub id: Option<String>,
    pub label: Option<String>,
    pub from_keychain_item: Option<String>,
    pub config_dir: Option<PathBuf>,
    pub from_file: Vec<PathBuf>,
    pub from_profile: Option<PathBuf>,
    pub dry_run: bool,
}

pub fn add(surface: &str, opts: AddOptions) -> Result<Code> {
    if surface == "openai" {
        return crate::codex_commands::add(&opts);
    }
    let kind: SurfaceKind = surface.parse()?;
    match kind {
        SurfaceKind::ClaudeCli => crate::claude_add::run(&opts),
        SurfaceKind::CodexCli | SurfaceKind::ChatgptDesktop => crate::codex_commands::add(&opts),
        SurfaceKind::ClaudeDesktop => crate::desktop_commands::add(&opts),
    }
}

pub fn remove(account: &str) -> Result<Code> {
    let app = App::open()?;
    if switch::remove_quarantine(&app.paths, &app.vault, account)? {
        println!("removed the recovery copy {account}");
        return Ok(0);
    }
    // A protected Claude Desktop folder (the original data) stops the removal before anything is deleted.
    let profile = claude_desktop::manage::plan_remove(&app.paths, &SystemApps, account)?;
    let removed = switch::remove_account(&app.paths, &app.vault, account)?;
    println!("removed {} ({}): config entry and saved login deleted", removed.id, removed.provider);
    if let Some(entry) = profile {
        claude_desktop::manage::finish_remove(&app.paths, account, &entry)?;
        println!("claude-desktop: deleted the profile folder {}", entry.dir.display());
    }
    Ok(0)
}

pub fn bind(surface: &str, account: &str) -> Result<Code> {
    let kind: SurfaceKind = surface.parse()?;
    let binding = Binding::parse(account);
    let app = App::open()?;
    // Bound and made the first priority of the surface, even if the activation below fails.
    let config = switch::choose(&app.paths, kind, binding.clone())?;
    let Some(id) = binding.account_id() else {
        println!("{kind}: unmanaged (agent-switch will not touch it)");
        return Ok(0);
    };
    println!("{kind}: bound to {id}");
    match switch::activate(&app.env(), &config, surface_for(kind).as_ref(), id) {
        Ok(()) => {
            println!("{kind}: {id} is now active");
            if let Some(note) = switch_note(kind) {
                println!("{note}");
            }
            Ok(0)
        }
        // The binding is the user's choice and stays, even when the activation failed.
        Err(e) => Err(Error::Invalid(format!("binding saved, but activation failed: {e}"))),
    }
}

pub fn doctor() -> Result<Code> {
    let app = App::open()?;
    let mut checks = doctor::run(&DoctorInput { paths: &app.paths, vault: &app.vault, exe: Some(&app.exe) });
    // The core lists the Claude storage check as skipped; the adapter owns the real one.
    if let Some(at) = checks.iter().position(|c| c.name == "claude storage") {
        let real = agent_switch_core::adapters::claude_code::ClaudeCode::production().doctor_checks(&app.env());
        checks.splice(at..=at, real);
    }
    // Likewise the Codex storage and app-version checks.
    if let Some(at) = checks.iter().position(|c| c.name == "codex storage") {
        checks.splice(at..=at, agent_switch_core::adapters::codex::doctor_checks(&app.env()));
        checks.retain(|c| !(c.name == "app versions" && c.level == Level::Skip));
    }
    checks.extend(claude_desktop::doctor_checks(&app.env()));
    let mut failed = false;
    for c in &checks {
        let tag = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => {
                failed = true;
                "FAIL"
            }
            Level::Skip => "skip",
        };
        println!("[{tag}] {}: {}", c.name, c.detail);
    }
    Ok(if failed { 1 } else { 0 })
}
