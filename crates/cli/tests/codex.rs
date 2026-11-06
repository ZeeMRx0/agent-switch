//! End-to-end checks of the Codex commands against a temporary HOME. Nothing here writes a saved login: the
//! real Keychain is only asked about items that do not exist, and `add` runs with `--dry-run`.

use std::path::Path;
use std::process::{Command, Output};

const JWT: &str = "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ICJ6ZWVAd29yay5jb20iLCAiZXhwIjogMTg5MzQ1NjAwMCwgImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6IHsiY2hhdGdwdF9hY2NvdW50X2lkIjogImFjY3QtdGVhbSIsICJjaGF0Z3B0X3BsYW5fdHlwZSI6ICJ0ZWFtIiwgImNoYXRncHRfdXNlcl9pZCI6ICJ1c2VyLTEifX0.c2ln";

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent-switch"))
        .args(args)
        .env("HOME", home)
        .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", "/nonexistent/agent-switch-test")
        .output()
        .expect("run agent-switch")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn auth(refresh_token: &str, last_refresh: &str) -> String {
    format!(
        r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{"id_token":"{JWT}","access_token":"{JWT}","refresh_token":"{refresh_token}","account_id":"acct-team"}},"last_refresh":"{last_refresh}"}}"#
    )
}

fn write_config(home: &Path, body: &str) {
    let file = home.join(".config/agent-switch/config.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, body).unwrap();
}

const ACCOUNTS: &str = r#"
[[accounts]]
id = "team-openai"
provider = "openai"
label = "Team"

[[accounts]]
id = "personal-openai"
provider = "openai"
label = "Personal"
"#;

#[test]
fn shell_init_exports_codex_home_only_when_the_cli_has_its_own_account() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), &format!("{ACCOUNTS}\n[surfaces]\nchatgpt-desktop = \"team-openai\"\ncodex-cli = \"personal-openai\"\n"));
    let o = run(home.path(), &["shell-init"]);
    assert!(o.status.success(), "{}", text(&o));
    let out = text(&o);
    let want = format!("export CODEX_HOME='{}'", home.path().join(".agent-switch/codex-cli").display());
    assert!(out.contains(&want), "{out}");
    assert!(!out.contains("refresh_token"));

    write_config(home.path(), &format!("{ACCOUNTS}\n[surfaces]\nchatgpt-desktop = \"team-openai\"\ncodex-cli = \"team-openai\"\n"));
    let o = run(home.path(), &["shell-init"]);
    assert!(o.status.success() && o.stdout.is_empty(), "bound together: nothing to export: {}", text(&o));

    write_config(home.path(), ACCOUNTS);
    let o = run(home.path(), &["shell-init"]);
    assert!(o.status.success() && o.stdout.is_empty(), "unmanaged: nothing to export: {}", text(&o));
}

#[test]
fn add_openai_dry_run_picks_the_newest_copy_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let old = home.path().join("old.json");
    let new = home.path().join("new.json");
    std::fs::write(&old, auth("rt-old", "2026-10-27T07:32:17.000000Z")).unwrap();
    std::fs::write(&new, auth("rt-new", "2026-11-06T03:04:59.000000Z")).unwrap();
    let o = run(
        home.path(),
        &["add", "openai", "--id", "team-openai", "--from-file", old.to_str().unwrap(), new.to_str().unwrap(), "--dry-run"],
    );
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("would add `team-openai`") && out.contains("used:") && out.contains("new.json"), "{out}");
    assert!(out.contains("considered:") && out.contains("old.json"), "{out}");
    assert!(!out.contains("rt-new") && !out.contains(JWT), "no token in the output: {out}");
    assert!(!home.path().join(".config/agent-switch/config.toml").exists(), "dry run must not create the config");
}

#[test]
fn add_openai_rejects_a_file_that_is_not_a_chatgpt_login() {
    let home = tempfile::tempdir().unwrap();
    let bad = home.path().join("bad.json");
    std::fs::write(&bad, r#"{"OPENAI_API_KEY":"sk-secret-value"}"#).unwrap();
    let o = run(home.path(), &["add", "openai", "--from-file", bad.to_str().unwrap(), "--dry-run"]);
    assert!(!o.status.success());
    assert!(!text(&o).contains("sk-secret-value"), "{}", text(&o));
}

#[test]
fn repair_dry_run_reports_without_changing_the_shallow_home() {
    let home = tempfile::tempdir().unwrap();
    let primary = home.path().join(".codex");
    let shallow = home.path().join(".agent-switch/codex-cli");
    std::fs::create_dir_all(&primary).unwrap();
    std::fs::create_dir_all(&shallow).unwrap();
    std::fs::write(primary.join("history.jsonl"), "a\n").unwrap();
    let o = run(home.path(), &["repair", "codex", "--dry-run"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("history.jsonl") && out.contains("dry run"), "{out}");
    assert!(std::fs::symlink_metadata(shallow.join("history.jsonl")).is_err(), "dry run linked nothing");

    let o = run(home.path(), &["repair", "claude"]);
    assert!(!o.status.success());
}
