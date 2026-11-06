//! `agent-switch add claude-cli`: log in a new account in a throwaway profile, or save a login that
//! already sits in a Keychain item (`--from-keychain-item`).

use agent_switch_core::adapters::claude_code::ClaudeCode;
use agent_switch_core::config::mask_email;
use agent_switch_core::{Error, Result};

use crate::commands::{AddOptions, App};

pub fn run(opts: &AddOptions) -> Result<i32> {
    let app = App::open()?;
    let adapter = ClaudeCode::production();
    let env = app.env();
    let done = match &opts.from_keychain_item {
        Some(service) => {
            let source = adapter.login_source(&env, service, opts.config_dir.as_deref())?;
            adapter.import_login(&env, &source, opts.id.as_deref(), opts.label.as_deref(), opts.dry_run)?
        }
        None if opts.dry_run => {
            return Err(Error::Invalid("--dry-run needs --from-keychain-item; the interactive login has nothing to preview".into()))
        }
        None => adapter.add_interactive(&env, opts.id.as_deref(), opts.label.as_deref())?,
    };

    let who = done.email.as_deref().map(mask_email).unwrap_or_else(|| "unknown email".into());
    let plan = done.plan.as_deref().map(|p| format!(", plan {p}")).unwrap_or_default();
    let verb = match (done.dry_run, done.created) {
        (true, true) => "would add",
        (true, false) => "would refresh the saved login of",
        (false, true) => "added",
        (false, false) => "refreshed the saved login of",
    };
    println!("claude-cli: {verb} `{}` ({who}{plan})", done.id);
    if !done.dry_run && done.created {
        println!("bind it with `agent-switch bind claude-cli {}`, or use it once with `agent-switch switch claude-cli {}`", done.id, done.id);
    }
    Ok(0)
}
