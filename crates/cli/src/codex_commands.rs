//! Codex-specific commands: `add openai`, `repair codex`, `shell-init`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use agent_switch_core::adapters::chatgpt_desktop::ChatgptDesktop;
use agent_switch_core::adapters::codex::{self, Copy};
use agent_switch_core::config::{mask_email, Config};
use agent_switch_core::macos_apps::SystemApps;
use agent_switch_core::{Error, Result};

use crate::commands::{AddOptions, App};

/// `agent-switch add openai [--from-file <auth.json>...]`.
pub fn add(opts: &AddOptions) -> Result<i32> {
    let app = App::open()?;
    let sources = if opts.from_file.is_empty() {
        if opts.dry_run {
            return Err(Error::Invalid("--dry-run needs --from-file; the interactive login has nothing to preview".into()));
        }
        let bin = codex::find_codex_bin()?;
        vec![codex::login_interactive(&app.paths, &bin)?]
    } else {
        opts.from_file.iter().map(|p| read_source(p)).collect::<Result<Vec<_>>>()?
    };
    let done = codex::import(&app.env(), sources, opts.id.as_deref(), opts.label.as_deref(), opts.dry_run)?;

    let who = done.login.email.as_deref().map(mask_email).unwrap_or_else(|| "unknown email".into());
    let plan = done.login.plan.as_deref().map(|p| format!(", plan {p}")).unwrap_or_default();
    let verb = match (opts.dry_run, done.created) {
        (true, true) => "would add",
        (true, false) => "would keep",
        (false, true) => "added",
        (false, false) => "kept",
    };
    println!("openai: {verb} `{}` ({who}{plan})", done.id);
    println!("  used: {}", done.chosen);
    if done.considered.len() > 1 {
        println!("  considered: {}", done.considered.join("; "));
    }
    println!(
        "  saved login: {}",
        match (opts.dry_run, done.vault_written) {
            (true, true) => "would be updated",
            (true, false) => "already newer or equal; would be left alone",
            (false, true) => "updated",
            (false, false) => "already newer or equal; left alone",
        }
    );
    if !opts.dry_run && done.created {
        println!("bind it with `agent-switch bind chatgpt-desktop {0}` or `agent-switch bind codex-cli {0}`", done.id);
    }
    Ok(0)
}

fn read_source(path: &Path) -> Result<Copy> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(format!("read {}", path.display()), e))?;
    Copy::parse(path.display().to_string(), text).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
}

/// `agent-switch repair codex [--dry-run]`.
pub fn repair(target: &str, dry_run: bool) -> Result<i32> {
    if target != "codex" {
        return Err(Error::Invalid(format!("unknown repair target `{target}`; the only one is `codex`")));
    }
    let app = App::open()?;
    let report = codex::repair(&SystemApps, &app.paths, dry_run)?;
    if report.fixed.is_empty() && report.attention.is_empty() {
        println!("codex: the shallow home is in sync; nothing to repair");
        return Ok(0);
    }
    for f in &report.fixed {
        println!("{} {f}", if dry_run { "would:" } else { "done: " });
    }
    for d in &report.attention {
        println!("needs a person: {d}");
    }
    if dry_run {
        println!("dry run: nothing was changed");
    }
    Ok(if report.attention.is_empty() { 0 } else { 2 })
}

/// `agent-switch shell-init`: the text a shell evaluates. Empty when the CLI shares `~/.codex`.
pub fn shell_init() -> Result<i32> {
    let app = App::open()?;
    let config = Config::load(&app.paths)?;
    print!("{}", codex::shell_init(&app.paths, &config));
    Ok(0)
}

/// The ChatGPT adapter for `switch chatgpt-desktop --restart`: the flag is consent to quit the app. Other
/// processes using `~/.codex` are stopped only after a yes on the terminal.
pub fn chatgpt_with_quit() -> ChatgptDesktop {
    ChatgptDesktop::production().with_quit(Box::new(|procs| {
        if !std::io::stdin().is_terminal() {
            return false;
        }
        let list: Vec<String> = procs.iter().map(|p| p.label()).collect();
        eprint!("These also use ~/.codex and must exit first: {}. Stop them (SIGTERM)? [y/N] ", list.join(", "));
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
    }))
}
