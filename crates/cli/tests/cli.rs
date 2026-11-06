//! End-to-end checks of the commands phase 1 owns, against a temporary HOME. No account in these tests has
//! a saved login, so the keychain is only ever asked about items that do not exist.

use std::path::Path;
use std::process::{Command, Output};

fn run(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-switch"));
    cmd.args(args);
    cmd.env("HOME", home);
    // Point Claude Code's Keychain lookup at an item that cannot exist, so no test reads the real login.
    cmd.env("CLAUDE_SECURESTORAGE_CONFIG_DIR", "/nonexistent/agent-switch-test");
    cmd.output().expect("run agent-switch")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn config_path(home: &Path) -> std::path::PathBuf {
    home.join(".config/agent-switch/config.toml")
}

fn write_config(home: &Path, body: &str) {
    let file = config_path(home);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, body).unwrap();
}

const TWO_ACCOUNTS: &str = r#"
[[accounts]]
id = "personal"
provider = "claude"
label = "Personal"
email = "zee@example.com"

[[accounts]]
id = "gpt"
provider = "openai"
label = "GPT"
"#;

#[test]
fn status_on_a_fresh_home_lists_every_surface_as_unmanaged() {
    let home = tempfile::tempdir().unwrap();
    let o = run(home.path(), &["status"]);
    assert!(o.status.success(), "{}", text(&o));
    let out = text(&o);
    for s in ["claude-cli", "claude-desktop", "codex-cli", "chatgpt-desktop"] {
        assert!(out.contains(s), "{out}");
    }
    assert!(out.contains("unmanaged"));
    assert!(out.contains("logged out"), "claude-cli is served by its adapter now: {out}");
}

#[test]
fn bind_saves_the_binding_even_when_activation_fails() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), TWO_ACCOUNTS);
    // `personal` has no saved login, so activation fails after the binding was saved.
    let o = run(home.path(), &["bind", "claude-cli", "personal"]);
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("binding saved, but activation failed"), "{}", text(&o));
    let cfg = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert!(cfg.contains("claude-cli = \"personal\""), "{cfg}");

    let o = run(home.path(), &["status"]);
    assert!(text(&o).contains("personal"));
}

#[test]
fn bind_rejects_wrong_provider_unknown_account_and_unknown_surface() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), TWO_ACCOUNTS);
    for args in [["bind", "claude-cli", "gpt"], ["bind", "claude-cli", "ghost"], ["bind", "nope", "personal"]] {
        let o = run(home.path(), &args);
        assert!(!o.status.success(), "{args:?}: {}", text(&o));
    }
    let cfg = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert!(!cfg.contains("claude-cli ="), "a rejected bind must not change the config:\n{cfg}");
}

#[test]
fn bind_unmanaged_clears_the_binding() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), &format!("{TWO_ACCOUNTS}\n[surfaces]\nclaude-cli = \"personal\"\n"));
    let o = run(home.path(), &["bind", "claude-cli", "unmanaged"]);
    assert!(o.status.success(), "{}", text(&o));
    let cfg = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert!(cfg.contains("claude-cli = \"unmanaged\""), "{cfg}");
}

#[test]
fn remove_refuses_a_bound_account_then_removes_it_once_unbound() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), &format!("{TWO_ACCOUNTS}\n[surfaces]\nclaude-cli = \"personal\"\n"));
    let o = run(home.path(), &["remove", "personal"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("bound to claude-cli"), "{}", text(&o));

    assert!(run(home.path(), &["bind", "claude-cli", "unmanaged"]).status.success());
    let o = run(home.path(), &["remove", "personal"]);
    assert!(o.status.success(), "{}", text(&o));
    let cfg = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert!(!cfg.contains("personal"), "{cfg}");
}

#[test]
fn switch_needs_a_bound_surface_and_undo_needs_history() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), TWO_ACCOUNTS);
    let o = run(home.path(), &["switch", "claude-cli", "personal"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("unmanaged"), "{}", text(&o));
    let o = run(home.path(), &["undo"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("a switch to undo"), "{}", text(&o));
}

#[test]
fn add_claude_desktop_needs_an_existing_account() {
    let home = tempfile::tempdir().unwrap();
    let o = run(home.path(), &["add", "claude-desktop"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("name the account"), "{}", text(&o));

    write_config(home.path(), TWO_ACCOUNTS);
    let o = run(home.path(), &["add", "claude-desktop", "ghost", "--dry-run"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("account `ghost`"), "{}", text(&o));
    let o = run(home.path(), &["add", "claude-desktop", "gpt", "--dry-run"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("needs a claude account"), "{}", text(&o));
}

#[test]
fn default_link_rejects_unknown_actions_and_a_missing_folder() {
    let home = tempfile::tempdir().unwrap();
    let o = run(home.path(), &["default-link", "sideways"]);
    assert!(!o.status.success() && text(&o).contains("enable"), "{}", text(&o));
    let o = run(home.path(), &["default-link", "enable", "--dry-run"]);
    assert!(!o.status.success() && text(&o).contains("does not exist"), "{}", text(&o));
}

#[test]
fn doctor_flags_an_invalid_config_and_exits_nonzero() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), "[surfaces]\nclaude-cli = \"ghost\"\n");
    let o = run(home.path(), &["doctor"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("[FAIL] config"), "{}", text(&o));
}

#[test]
fn doctor_masks_emails_and_warns_about_a_missing_login() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), TWO_ACCOUNTS);
    let o = run(home.path(), &["doctor"]);
    let out = text(&o);
    assert!(out.contains("ze***@example.com") && !out.contains("zee@example.com"), "{out}");
    assert!(out.contains("no saved login"), "{out}");
    assert!(o.status.success(), "{out}");
}

#[test]
fn list_masks_emails() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), TWO_ACCOUNTS);
    let out = text(&run(home.path(), &["list"]));
    assert!(out.contains("personal") && out.contains("ze***@example.com") && !out.contains("zee@"), "{out}");
}

#[test]
fn list_json_masks_emails_and_reports_an_account_without_a_saved_login_as_unreadable() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), TWO_ACCOUNTS);
    let o = run(home.path(), &["list", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(&o)));
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["id"], "personal");
    assert_eq!(rows[0]["email"], "ze***@example.com");
    assert_eq!(rows[0]["state"], "unreadable");
    assert!(rows[0]["five_hour"].is_null() && rows[0]["weekly"].is_null());
    assert!(!String::from_utf8_lossy(&o.stdout).contains("zee@"));
}

#[test]
fn switch_offers_force_and_desktop_sessions_rejects_an_unknown_action() {
    let home = tempfile::tempdir().unwrap();
    let o = run(home.path(), &["switch", "--help"]);
    assert!(o.status.success() && text(&o).contains("--force"), "{}", text(&o));
    let o = run(home.path(), &["desktop-sessions", "sideways"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("use `on` or `off`"), "{}", text(&o));
    let o = run(home.path(), &["desktop-sessions", "--help"]);
    assert!(text(&o).contains("--dry-run"), "{}", text(&o));
}
