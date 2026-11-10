//! "Add account…": open Terminal on the `agent-switch add` command of a provider.
//!
//! Adding an account is an interactive login, so it cannot run inside the menu bar app. The app writes a
//! small private `.command` script and asks Terminal to open it. The script holds only fixed text and the
//! provider's fixed subcommand. The one value the user types (the Antigravity account id) is read by the
//! script itself and passed quoted, so it never becomes part of the script text.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_switch_core::config::Provider;
use agent_switch_core::fsutil::{ensure_private_dir, safe_write, WriteOpts};
use agent_switch_core::paths::Paths;
use agent_switch_core::{Error, Result};

/// Where `scripts/install.sh` puts the CLI, relative to `$HOME`.
const INSTALLED_CLI: &str = ".local/bin/agent-switch";

/// The file name of the script for `provider`.
fn script_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "add-account-claude.command",
        Provider::Openai => "add-account-openai.command",
        Provider::Antigravity => "add-account-antigravity.command",
    }
}

/// The shell lines that run once the CLI is found in `$bin`.
fn provider_lines(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => {
            "echo \"Claude opens next. Type /login, finish signing in, then type /exit.\"\n\"$bin\" add claude-cli\n"
        }
        Provider::Openai => "\"$bin\" add openai\n",
        Provider::Antigravity => {
            "echo \"Sign in with agy first; this saves the login agy holds.\"\n\
             printf 'Account id (a-z, 0-9, - or _): '\n\
             read -r id\n\
             if [ -z \"$id\" ]; then\n  echo \"No id given; nothing was saved.\"\nelse\n  \"$bin\" add antigravity-cli --id \"$id\"\nfi\n"
        }
    }
}

/// The CLI inside the running app bundle (`Contents/MacOS/agent-switch`), when it is there.
fn bundled_cli() -> Option<PathBuf> {
    let cli = std::env::current_exe().ok()?.parent()?.join("agent-switch");
    cli.is_file().then_some(cli)
}

/// `text` as one single-quoted shell word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The text of the script for `provider`. It uses the CLI beside the app executable (`bundled`), else the one
/// that `install.sh` put in `~/.local/bin`, else the one on the login shell's `PATH`, and waits for Enter at
/// the end so the output can be read.
fn script_with(provider: Provider, bundled: Option<&Path>) -> String {
    let bundled = bundled.map(|p| shell_quote(&p.to_string_lossy())).unwrap_or_else(|| "''".to_string());
    format!(
        "#!/bin/zsh -l\n\
         bin={bundled}\n\
         [ -x \"$bin\" ] || bin=\"$HOME/{INSTALLED_CLI}\"\n\
         [ -x \"$bin\" ] || bin=\"$(command -v agent-switch)\"\n\
         if [ -z \"$bin\" ]; then\n  echo \"agent-switch was not found. Install agent-switch with Homebrew: brew install zeemrx0/tap/agent-switch\"\nelse\n{body}fi\n\
         echo\n\
         echo \"Done. You can close this window.\"\n\
         read -r reply\n",
        body = provider_lines(provider)
    )
}

/// The script of `provider` for this running app.
pub fn script(provider: Provider) -> String {
    script_with(provider, bundled_cli().as_deref())
}

/// Write the script of `provider` into the app's private `tmp` folder, executable by the user only.
fn write_script(paths: &Paths, provider: Provider) -> Result<PathBuf> {
    let dir = paths.support_dir().join("tmp");
    ensure_private_dir(&dir)?;
    let path = dir.join(script_name(provider));
    safe_write(&path, script(provider).as_bytes(), &WriteOpts::secret().with_mode(0o700))?;
    Ok(path)
}

/// Open Terminal on the add-account script of `provider`.
pub fn open_terminal(paths: &Paths, provider: Provider) -> Result<()> {
    let path = write_script(paths, provider)?;
    let status = Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(&path)
        .status()
        .map_err(|e| Error::io("run /usr/bin/open", e))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Invalid(format!("Terminal could not open {} ({status})", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn each_script_runs_its_providers_subcommand() {
        assert!(script(Provider::Claude).contains("\"$bin\" add claude-cli\n"));
        assert!(script(Provider::Openai).contains("\"$bin\" add openai\n"));
        assert!(script(Provider::Antigravity).contains("\"$bin\" add antigravity-cli --id"));
        assert!(!script(Provider::Claude).contains("openai") && !script(Provider::Openai).contains("claude-cli"));
    }

    #[test]
    fn the_cli_beside_the_app_comes_first_and_is_quoted() {
        let s = script_with(Provider::Claude, Some(Path::new("/Apps/Agent Switch.app/it's/agent-switch")));
        assert!(s.contains("bin='/Apps/Agent Switch.app/it'\\''s/agent-switch'\n[ -x \"$bin\" ] || bin=\"$HOME/"), "{s}");
        assert!(script_with(Provider::Claude, None).contains("bin=''\n"));
    }

    #[test]
    fn the_antigravity_id_is_read_by_the_script_and_passed_quoted() {
        let s = script(Provider::Antigravity);
        assert!(s.contains("sign in") || s.contains("Sign in with agy first"), "{s}");
        assert!(s.contains("read -r id"), "{s}");
        assert!(s.contains("--id \"$id\"\n"), "{s}");
        assert!(!s.contains("--id $id"), "{s}");
    }

    #[test]
    fn every_script_finds_the_cli_explains_a_missing_one_and_waits() {
        for p in Provider::ALL {
            let s = script(p);
            assert!(s.starts_with("#!/bin/zsh -l\n"), "{s}");
            assert!(s.contains("bin=\"$HOME/.local/bin/agent-switch\""), "{s}");
            assert!(s.contains("command -v agent-switch"), "{s}");
            assert!(s.contains("brew install zeemrx0/tap/agent-switch") && !s.contains("scripts/install.sh"), "{s}");
            assert!(s.contains("Done. You can close this window."), "{s}");
            assert!(s.trim_end().ends_with("read -r reply"), "{s}");
        }
    }

    #[test]
    fn the_script_is_written_private_and_executable_for_the_user_only() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(home.path());
        let path = write_script(&paths, Provider::Claude).unwrap();
        assert_eq!(path, paths.support_dir().join("tmp/add-account-claude.command"));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), script(Provider::Claude));
        // Writing again replaces the file.
        write_script(&paths, Provider::Claude).unwrap();
    }
}
