//! Claude Desktop commands: `add claude-desktop` and `default-link`.

use std::time::Duration;

use agent_switch_core::adapters::claude_desktop::manage::{self, AddRequest};
use agent_switch_core::config::Config;
use agent_switch_core::macos_apps::{QuitOpts, SystemApps};
use agent_switch_core::{Error, Result};

use crate::commands::{AddOptions, App};

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(600);
const SIGN_IN_POLL: Duration = Duration::from_secs(2);
const REASK: Duration = Duration::from_secs(2);

fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// `agent-switch add claude-desktop <account> [--from-profile <dir>] [--dry-run]`.
pub fn add(opts: &AddOptions) -> Result<i32> {
    let account = opts
        .account
        .as_deref()
        .or(opts.id.as_deref())
        .ok_or_else(|| Error::Invalid("name the account: `agent-switch add claude-desktop <account>` (add it first with `add claude-cli`)".into()))?;
    let app = App::open()?;
    let env = app.env();
    let report = manage::add_profile(&env, &SystemApps, &AddRequest { account, from_profile: opts.from_profile.as_deref(), dry_run: opts.dry_run })?;

    let id = &report.account_id;
    if report.already_registered {
        println!("claude-desktop: `{id}` already has the profile {} (nothing changed)", report.dir.display());
        if let Some(s) = &report.session {
            println!("  session: {}", s.describe());
        }
        return Ok(0);
    }
    let verb = if report.dry_run { "would register" } else { "registered" };
    if report.in_place {
        println!("claude-desktop: {verb} the app's own profile {} as `{id}` (in place, protected: it is never deleted or rewritten)", report.dir.display());
    } else if let Some(from) = &report.cloned_from {
        println!("claude-desktop: {verb} `{id}` at {} (cloned from {}; the original is left in place)", report.dir.display(), from.display());
        if let Some(s) = report.clone_stats {
            println!("  {} files, {} logical", s.files, gib(s.bytes));
        }
    } else {
        println!("claude-desktop: {} a new profile for `{id}` at {}", if report.dry_run { "would create" } else { "created" }, report.dir.display());
    }
    if let Some(s) = &report.session {
        println!("  session: {}", s.describe());
    }
    if report.config_copied {
        println!("  copied claude_desktop_config.json as is (mode 0600)");
        if !report.mcp_env_keys.is_empty() {
            println!("  warning: these MCP `env` variables were copied with their values, so their tokens are now usable from this account's chats: {}", report.mcp_env_keys.join(", "));
        }
    }
    if let Some(rt) = &report.runtime {
        let cost = rt.on_disk.map(|b| format!(", {} of free space used", gib(b))).unwrap_or_default();
        println!("  runtime: cloned {} item(s), {} logical{cost}; {} already present", rt.cloned, gib(rt.logical), rt.skipped_existing);
    }
    for n in &report.notes {
        println!("  note: {n}");
    }
    if report.opened_for_sign_in {
        let config = Config::load(&app.paths)?;
        let acct = config.account(id).ok_or_else(|| Error::NotFound(format!("account `{id}`")))?;
        println!("Claude opened on the new profile. Sign in with the account for `{id}` in that window; waiting for the sign-in (up to 10 minutes)...");
        let session = manage::confirm_sign_in(acct, &report.dir, SIGN_IN_TIMEOUT, SIGN_IN_POLL)?;
        println!("  confirmed: the profile matches `{id}` and {}", session.describe());
    }
    if !report.dry_run {
        println!("bind it with `agent-switch bind claude-desktop {id}`");
    }
    Ok(0)
}

/// `agent-switch default-link enable|disable [--dry-run]`.
pub fn default_link(action: &str, dry_run: bool) -> Result<i32> {
    let app = App::open()?;
    let env = app.env();
    let quit = QuitOpts::default();
    let report = match action {
        "enable" => manage::enable_default_link(&env, &SystemApps, &quit, REASK, dry_run)?,
        "disable" => manage::disable_default_link(&env, &SystemApps, &quit, REASK, dry_run)?,
        other => return Err(Error::Invalid(format!("unknown action `{other}`; use `enable` or `disable`"))),
    };
    for n in &report.notes {
        println!("claude-desktop: {n}");
    }
    let verb = |done: &str, plan: &str| if report.dry_run { plan.to_string() } else { done.to_string() };
    if report.changed || report.dry_run {
        if action == "enable" {
            if let (Some(backup), Some(stats)) = (&report.backup, report.stats) {
                println!("claude-desktop: {} {} ({} files, {} logical)", verb("backed up to", "would back up to"), backup.display(), stats.files, gib(stats.bytes));
            }
            println!("claude-desktop: {} {} -> {}", verb("linked", "would link"), report.default.display(), report.target.display());
            println!("Restore by hand if ever needed: quit Claude, remove the link {0}, and move {1} back to {0}.", report.default.display(), report.target.display());
        } else {
            println!("claude-desktop: {} {} back to {}", verb("moved", "would move"), report.target.display(), report.default.display());
        }
    }
    Ok(0)
}
