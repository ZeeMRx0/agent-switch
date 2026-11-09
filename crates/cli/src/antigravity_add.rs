//! `agent-switch add antigravity-cli`: save the login `agy` holds in its Keychain item.

use agent_switch_core::adapters::antigravity_cli::AntigravityCli;
use agent_switch_core::config::mask_email;
use agent_switch_core::{Error, Result};

use crate::commands::{AddOptions, App};

pub fn run(opts: &AddOptions) -> Result<i32> {
    let id = opts
        .id
        .as_deref()
        .ok_or_else(|| Error::Invalid("give the account a name with --id (sign in with `agy` first; this saves that login)".into()))?;
    let app = App::open()?;
    let done = AntigravityCli::production().import_login(&app.env(), id, opts.label.as_deref(), opts.dry_run)?;
    let verb = match (done.dry_run, done.created, done.vault_written) {
        (_, false, false) => "already saved",
        (true, true, _) => "would add",
        (true, false, true) => "would refresh the saved login of",
        (false, true, _) => "added",
        (false, false, true) => "refreshed the saved login of",
    };
    println!("antigravity-cli: {verb} `{}` ({})", done.id, mask_email(&done.email));
    if !done.dry_run && done.created {
        println!("bind it with `agent-switch bind antigravity-cli {}`, or use it once with `agent-switch switch antigravity-cli {}`", done.id, done.id);
    }
    Ok(0)
}
