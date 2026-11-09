mod antigravity_add;
mod claude_add;
mod codex_commands;
mod commands;
mod desktop_commands;
mod list_command;
mod registry;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent-switch", version, about = "Switch Claude, ChatGPT and Antigravity accounts per surface")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show each surface: binding, active account, pending record and live identity
    Status,
    /// List saved accounts with their 5-hour and weekly usage and the surfaces they are bound to or active on
    List {
        /// Machine-readable output (emails are masked)
        #[arg(long)]
        json: bool,
    },
    /// Activate an account on a bound surface (one-off; does not change the binding)
    Switch {
        /// claude-cli, claude-desktop, codex-cli, chatgpt-desktop or antigravity-cli
        surface: String,
        account: String,
        /// Also restart the app (Desktop surfaces)
        #[arg(long)]
        restart: bool,
        /// Desktop: switch even if a session is mid-reply; that reply is lost
        #[arg(long)]
        force: bool,
    },
    /// Log in a new account on a surface and save it
    Add {
        surface: String,
        /// The account to give a Claude Desktop profile (claude-desktop only; `--id` also works)
        account: Option<String>,
        /// Account id (a-z, 0-9, `-`, `_`)
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        label: Option<String>,
        /// Save the login that already sits in this Keychain item instead of logging in (claude-cli); safe to repeat
        #[arg(long, value_name = "SERVICE")]
        from_keychain_item: Option<String>,
        /// The CLAUDE_CONFIG_DIR the `--from-keychain-item` login was created with (it supplies the identity)
        #[arg(long, value_name = "DIR", requires = "from_keychain_item")]
        config_dir: Option<std::path::PathBuf>,
        /// Save the login in this Codex `auth.json` (openai); repeat for several copies, the newest `last_refresh` wins
        #[arg(long, value_name = "PATH", num_args = 1..)]
        from_file: Vec<std::path::PathBuf>,
        /// Register an existing Claude Desktop profile folder instead of creating one (claude-desktop); the app's own
        /// folder is registered in place, any other folder is cloned into the managed folder
        #[arg(long, value_name = "DIR")]
        from_profile: Option<std::path::PathBuf>,
        /// Show what would be saved without writing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete a saved account (refused while bound or active)
    Remove { account: String },
    /// Bind a surface to an account, or to `unmanaged`, and activate it
    Bind {
        surface: String,
        /// Account id or `unmanaged`
        account: String,
    },
    /// Re-link the Codex shallow home to ~/.codex (run with Codex and the ChatGPT app closed)
    Repair {
        /// Only `codex`
        target: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Make the Claude Desktop default folder a link to the active profile, so a Dock launch opens the bound
    /// account (backs the folder up first), or put the real folder back
    DefaultLink {
        /// enable or disable
        action: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Claude Desktop session mirroring. `off` gives each profile back its own sessions and keeps mirroring off;
    /// `on` turns it back on for the next switch. Both need Claude closed
    DesktopSessions {
        /// on or off
        action: String,
        /// Check that the change is allowed (Claude closed, mirroring currently in the opposite state) without making it
        #[arg(long)]
        dry_run: bool,
    },
    /// Print shell code that points the Codex CLI at its own login (use: eval "$(agent-switch shell-init)")
    ShellInit,
    /// Check config, state, keychain backend and saved logins
    Doctor,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Status => commands::status(),
        Command::List { json } => list_command::list(json),
        Command::Switch { surface, account, restart, force } => commands::switch(&surface, &account, restart, force),
        Command::Add { surface, account, id, label, from_keychain_item, config_dir, from_file, from_profile, dry_run } => commands::add(
            &surface,
            commands::AddOptions { account, id, label, from_keychain_item, config_dir, from_file, from_profile, dry_run },
        ),
        Command::Remove { account } => commands::remove(&account),
        Command::Bind { surface, account } => commands::bind(&surface, &account),
        Command::Repair { target, dry_run } => codex_commands::repair(&target, dry_run),
        Command::DefaultLink { action, dry_run } => desktop_commands::default_link(&action, dry_run),
        Command::DesktopSessions { action, dry_run } => desktop_commands::desktop_sessions(&action, dry_run),
        Command::ShellInit => codex_commands::shell_init(),
        Command::Doctor => commands::doctor(),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
