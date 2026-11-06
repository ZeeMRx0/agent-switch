//! The shallow `CODEX_HOME` (`~/.agent-switch/codex-cli`) that lets the Codex CLI use its own login while
//! the ChatGPT app keeps `~/.codex`. Every shared entry in it is a symlink into `~/.codex`, so sessions and
//! history stay in one place. Only per-home entries are real files of the shallow home.
//!
//! Ported from codex-switcher (`src/codex/default-home.ts`, `src/account/paths.ts`: per-home socket
//! handling) and its `repair-sessions` command (re-link, append-only merge), with the red-team limits:
//! nothing here runs on a timer, and the per-home entries are never linked, moved or merged.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fsutil::ensure_private_dir;
use crate::paths::Paths;

/// Files that only ever grow by whole lines, so two copies can be merged by appending missing lines.
pub const APPEND_ONLY: [&str; 2] = ["history.jsonl", "session_index.jsonl"];

pub fn primary_dir(paths: &Paths) -> PathBuf {
    paths.home().join(".codex")
}

pub fn shallow_dir(paths: &Paths) -> PathBuf {
    paths.home().join(".agent-switch/codex-cli")
}

/// Entries that belong to one home and are never linked into the shallow home, moved or merged: the login
/// and its lock, lock files, the app's socket folders (`ipc/`, `app-server-control*`), MCP OAuth state, and
/// SQLite sidecar files that come and go while a database is open.
pub fn is_per_home(name: &str) -> bool {
    matches!(name, "auth.json" | "auth.lock" | "ipc")
        || name.starts_with("app-server-control")
        || name.starts_with("mcp-oauth-")
        || name.ends_with(".lock")
        || name.ends_with("-wal")
        || name.ends_with("-shm")
        || name.ends_with("-journal")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftKind {
    /// A shared entry of `~/.codex` that the shallow home does not link yet.
    Missing,
    /// A real file where a link belongs (Codex replaced the link when it rewrote the file).
    ReplacedByFile,
    ReplacedByDir,
    /// A link that points somewhere other than the same name in `~/.codex`.
    WrongTarget,
    /// A link to a per-home entry (for example `ipc` or a `-wal` file). Harmful: it shares state that must
    /// stay per home, and `ipc` would let the CLI attach to the app's server.
    ForbiddenLink,
    /// A real entry of the shallow home that `~/.codex` does not have and that is not per-home.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub name: String,
    pub kind: DriftKind,
}

impl fmt::Display for Drift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            DriftKind::Missing => "not linked yet",
            DriftKind::ReplacedByFile => "a real file replaced the link",
            DriftKind::ReplacedByDir => "a real folder replaced the link",
            DriftKind::WrongTarget => "the link points somewhere else",
            DriftKind::ForbiddenLink => "per-home entry is linked into ~/.codex (must not be shared)",
            DriftKind::Unknown => "unknown entry that ~/.codex does not have (left in place)",
        };
        write!(f, "{}: {what}", self.name)
    }
}

fn entry_names(dir: &Path) -> Result<BTreeSet<String>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(Error::io(format!("list {}", dir.display()), e)),
    };
    let mut names = BTreeSet::new();
    for entry in rd {
        let entry = entry.map_err(|e| Error::io(format!("list {}", dir.display()), e))?;
        if let Some(n) = entry.file_name().to_str() {
            names.insert(n.to_string());
        }
    }
    Ok(names)
}

fn link_target(path: &Path) -> Option<PathBuf> {
    std::fs::read_link(path).ok()
}

/// Compare the shallow home with `~/.codex`. Read-only.
pub fn scan(paths: &Paths) -> Result<Vec<Drift>> {
    let (primary, shallow) = (primary_dir(paths), shallow_dir(paths));
    let mut names = entry_names(&primary)?;
    names.extend(entry_names(&shallow)?);
    let mut out = Vec::new();
    for name in names {
        let in_primary = std::fs::symlink_metadata(primary.join(&name)).is_ok();
        let meta = std::fs::symlink_metadata(shallow.join(&name)).ok();
        let drift = |kind| Drift { name: name.clone(), kind };
        let expected = primary.join(&name);
        match meta {
            None if in_primary && !is_per_home(&name) => out.push(drift(DriftKind::Missing)),
            None => {}
            Some(m) if m.file_type().is_symlink() => {
                let target = link_target(&shallow.join(&name));
                if is_per_home(&name) {
                    if target.as_deref().is_some_and(|t| t.starts_with(&primary)) {
                        out.push(drift(DriftKind::ForbiddenLink));
                    }
                } else if target.as_deref() != Some(expected.as_path()) && in_primary {
                    out.push(drift(DriftKind::WrongTarget));
                }
            }
            Some(_) if is_per_home(&name) => {}
            Some(_) if !in_primary => out.push(drift(DriftKind::Unknown)),
            Some(m) => out.push(drift(if m.is_dir() { DriftKind::ReplacedByDir } else { DriftKind::ReplacedByFile })),
        }
    }
    Ok(out)
}

/// Create the shallow home when missing and link every shared entry that is not linked yet. Touches
/// nothing else, so it is safe while Codex runs. Returns the names it linked.
pub fn ensure(paths: &Paths) -> Result<Vec<String>> {
    let (primary, shallow) = (primary_dir(paths), shallow_dir(paths));
    if !primary.is_dir() {
        return Err(Error::Invalid(format!("{} does not exist; open the ChatGPT app or run `codex` once first", primary.display())));
    }
    ensure_private_dir(&shallow)?;
    let mut linked = Vec::new();
    for d in scan(paths)?.into_iter().filter(|d| d.kind == DriftKind::Missing) {
        symlink(primary.join(&d.name), shallow.join(&d.name)).map_err(|e| Error::io(format!("link {}", d.name), e))?;
        linked.push(d.name);
    }
    Ok(linked)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fixed {
    Linked(String),
    Relinked(String),
    Merged { name: String, lines: usize },
    RemovedLink(String),
}

impl fmt::Display for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fixed::Linked(n) => write!(f, "linked {n}"),
            Fixed::Relinked(n) => write!(f, "re-linked {n} (the copy was identical)"),
            Fixed::Merged { name, lines } => write!(f, "re-linked {name} after appending {lines} new line(s) to the shared file"),
            Fixed::RemovedLink(n) => write!(f, "removed the link {n} (per-home entry; the file in ~/.codex is untouched)"),
        }
    }
}

#[derive(Debug, Default)]
pub struct RepairReport {
    pub fixed: Vec<Fixed>,
    /// Drift that `repair` leaves for a person: differing copies, folders, unknown entries.
    pub attention: Vec<Drift>,
}

/// Replace `shallow/name` with a link to `primary/name` in one rename.
fn relink(primary: &Path, shallow: &Path, name: &str) -> Result<()> {
    let tmp = shallow.join(format!(".{name}.agent-switch-link"));
    let _ = std::fs::remove_file(&tmp);
    symlink(primary.join(name), &tmp).map_err(|e| Error::io(format!("link {name}"), e))?;
    std::fs::rename(&tmp, shallow.join(name)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(format!("replace {name} with a link"), e)
    })
}

/// Append the lines of `copy` that `target` does not have, in order. Returns how many were added.
fn append_missing_lines(copy: &Path, target: &Path) -> Result<usize> {
    let read = |p: &Path| std::fs::read_to_string(p).map_err(|e| Error::io(format!("read {}", p.display()), e));
    let (copy_text, target_text) = (read(copy)?, read(target)?);
    let have: HashSet<&str> = target_text.lines().collect();
    let mut seen = HashSet::new();
    let missing: Vec<&str> = copy_text.lines().filter(|l| !l.is_empty() && !have.contains(l) && seen.insert(*l)).collect();
    if missing.is_empty() {
        return Ok(0);
    }
    let mut out = String::new();
    if !target_text.is_empty() && !target_text.ends_with('\n') {
        out.push('\n');
    }
    for l in &missing {
        out.push_str(l);
        out.push('\n');
    }
    let mut f = std::fs::OpenOptions::new().append(true).open(target).map_err(|e| Error::io(format!("open {}", target.display()), e))?;
    f.write_all(out.as_bytes()).map_err(|e| Error::io(format!("append to {}", target.display()), e))?;
    f.sync_all().map_err(|e| Error::io(format!("sync {}", target.display()), e))?;
    Ok(missing.len())
}

/// Fix the drift that can be fixed without losing data. The caller has already checked that no Codex
/// process uses either home. With `dry_run` nothing changes and the report says what would.
pub fn repair(paths: &Paths, dry_run: bool) -> Result<RepairReport> {
    let (primary, shallow) = (primary_dir(paths), shallow_dir(paths));
    let mut report = RepairReport::default();
    for d in scan(paths)? {
        let path = shallow.join(&d.name);
        match d.kind {
            DriftKind::Missing => {
                if !dry_run {
                    symlink(primary.join(&d.name), &path).map_err(|e| Error::io(format!("link {}", d.name), e))?;
                }
                report.fixed.push(Fixed::Linked(d.name));
            }
            DriftKind::ForbiddenLink => {
                if !dry_run {
                    std::fs::remove_file(&path).map_err(|e| Error::io(format!("remove link {}", d.name), e))?;
                }
                report.fixed.push(Fixed::RemovedLink(d.name));
            }
            DriftKind::ReplacedByFile if APPEND_ONLY.contains(&d.name.as_str()) => {
                let target = primary.join(&d.name);
                if !target.is_file() {
                    report.attention.push(d);
                    continue;
                }
                let lines = if dry_run { 0 } else { append_missing_lines(&path, &target)? };
                if !dry_run {
                    relink(&primary, &shallow, &d.name)?;
                }
                report.fixed.push(Fixed::Merged { name: d.name, lines });
            }
            DriftKind::ReplacedByFile => {
                let same = std::fs::read(&path).ok().zip(std::fs::read(primary.join(&d.name)).ok()).is_some_and(|(a, b)| a == b);
                if !same {
                    report.attention.push(d);
                    continue;
                }
                if !dry_run {
                    relink(&primary, &shallow, &d.name)?;
                }
                report.fixed.push(Fixed::Relinked(d.name));
            }
            DriftKind::ReplacedByDir | DriftKind::WrongTarget | DriftKind::Unknown => report.attention.push(d),
        }
    }
    Ok(report)
}

/// Make the shallow `auth.json` a link to `~/.codex/auth.json`, so a shell that still exports the old
/// `CODEX_HOME` keeps using the one login file instead of a second copy of the same token chain.
pub fn join_auth(paths: &Paths) -> Result<()> {
    let (primary, shallow) = (primary_dir(paths), shallow_dir(paths));
    if std::fs::symlink_metadata(shallow.join("auth.json")).is_ok_and(|m| m.file_type().is_symlink() && link_target(&shallow.join("auth.json")).as_deref() == Some(primary.join("auth.json").as_path())) {
        return Ok(());
    }
    relink(&primary, &shallow, "auth.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        _d: tempfile::TempDir,
        paths: Paths,
    }

    impl Fixture {
        fn new() -> Fixture {
            let d = tempfile::tempdir().unwrap();
            let paths = Paths::from_home(d.path());
            let p = primary_dir(&paths);
            fs::create_dir_all(p.join("sessions/2026")).unwrap();
            fs::create_dir_all(p.join("ipc")).unwrap();
            for (n, c) in [
                ("auth.json", "{}"),
                ("auth.lock", ""),
                ("config.toml", "model = \"x\"\n"),
                ("history.jsonl", "{\"a\":1}\n{\"a\":2}\n"),
                ("session_index.jsonl", ""),
                ("state_5.sqlite", "db"),
            ] {
                fs::write(p.join(n), c).unwrap();
            }
            Fixture { _d: d, paths }
        }
        fn primary(&self) -> PathBuf {
            primary_dir(&self.paths)
        }
        fn shallow(&self) -> PathBuf {
            shallow_dir(&self.paths)
        }
        fn names(&self) -> Vec<String> {
            entry_names(&self.shallow()).unwrap().into_iter().collect()
        }
    }

    #[test]
    fn the_per_home_list_is_exact() {
        for n in ["auth.json", "auth.lock", "daemon.lock", "ipc", "app-server-control", "app-server-control.sock", "mcp-oauth-locks", "x.sqlite-wal", "x.sqlite-shm", "x-journal"] {
            assert!(is_per_home(n), "{n}");
        }
        for n in ["sessions", "history.jsonl", "state_5.sqlite", "config.toml", ".tmp", "thread-writer-locks", "journal"] {
            assert!(!is_per_home(n), "{n}");
        }
    }

    #[test]
    fn creation_links_shared_entries_and_never_the_per_home_ones() {
        let f = Fixture::new();
        let linked = ensure(&f.paths).unwrap();
        assert!(linked.contains(&"sessions".to_string()) && linked.contains(&"state_5.sqlite".to_string()));
        assert_eq!(fs::read_link(f.shallow().join("sessions")).unwrap(), f.primary().join("sessions"));
        for n in ["auth.json", "auth.lock", "ipc"] {
            assert!(!f.names().contains(&n.to_string()), "{n} must stay per home: {:?}", f.names());
        }
        assert!(ensure(&f.paths).unwrap().is_empty(), "a second run changes nothing");
        assert!(scan(&f.paths).unwrap().is_empty());
    }

    #[test]
    fn a_cli_in_the_shallow_home_cannot_reach_the_app_socket() {
        let f = Fixture::new();
        fs::write(f.primary().join("ipc/app.sock"), "").unwrap();
        ensure(&f.paths).unwrap();
        assert!(!f.shallow().join("ipc").exists());
        assert!(!f.shallow().join("ipc/app.sock").exists());
    }

    #[test]
    fn new_wal_and_control_entries_in_the_primary_are_not_linked() {
        let f = Fixture::new();
        ensure(&f.paths).unwrap();
        fs::write(f.primary().join("state_5.sqlite-wal"), "").unwrap();
        fs::write(f.primary().join("state_5.sqlite-shm"), "").unwrap();
        fs::create_dir(f.primary().join("app-server-control")).unwrap();
        assert!(scan(&f.paths).unwrap().is_empty());
        assert!(ensure(&f.paths).unwrap().is_empty());
        assert!(!f.names().iter().any(|n| n.ends_with("-wal") || n.contains("control")));
    }

    #[test]
    fn existing_links_to_per_home_entries_are_drift_and_repair_removes_only_the_link() {
        let f = Fixture::new();
        ensure(&f.paths).unwrap();
        fs::write(f.primary().join("state_5.sqlite-wal"), "wal").unwrap();
        symlink(f.primary().join("state_5.sqlite-wal"), f.shallow().join("state_5.sqlite-wal")).unwrap();
        symlink(f.primary().join("ipc"), f.shallow().join("ipc")).unwrap();
        let drift = scan(&f.paths).unwrap();
        assert_eq!(drift.len(), 2);
        assert!(drift.iter().all(|d| d.kind == DriftKind::ForbiddenLink));

        let dry = repair(&f.paths, true).unwrap();
        assert_eq!(dry.fixed.len(), 2);
        assert!(f.shallow().join("ipc").symlink_metadata().is_ok(), "dry run changes nothing");

        let r = repair(&f.paths, false).unwrap();
        assert_eq!(r.fixed.len(), 2);
        assert!(f.shallow().join("ipc").symlink_metadata().is_err());
        assert_eq!(fs::read_to_string(f.primary().join("state_5.sqlite-wal")).unwrap(), "wal");
        assert!(f.primary().join("ipc").is_dir());
        assert!(scan(&f.paths).unwrap().is_empty());
    }

    #[test]
    fn a_link_replaced_by_an_identical_file_is_relinked() {
        let f = Fixture::new();
        ensure(&f.paths).unwrap();
        fs::remove_file(f.shallow().join("config.toml")).unwrap();
        fs::write(f.shallow().join("config.toml"), "model = \"x\"\n").unwrap();
        assert_eq!(scan(&f.paths).unwrap(), vec![Drift { name: "config.toml".into(), kind: DriftKind::ReplacedByFile }]);
        let r = repair(&f.paths, false).unwrap();
        assert_eq!(r.fixed, vec![Fixed::Relinked("config.toml".into())]);
        assert_eq!(fs::read_link(f.shallow().join("config.toml")).unwrap(), f.primary().join("config.toml"));
    }

    #[test]
    fn a_differing_file_is_reported_and_left_alone() {
        let f = Fixture::new();
        ensure(&f.paths).unwrap();
        fs::remove_file(f.shallow().join("config.toml")).unwrap();
        fs::write(f.shallow().join("config.toml"), "model = \"newer\"\n").unwrap();
        let r = repair(&f.paths, false).unwrap();
        assert!(r.fixed.is_empty());
        assert_eq!(r.attention.len(), 1);
        assert_eq!(fs::read_to_string(f.shallow().join("config.toml")).unwrap(), "model = \"newer\"\n");
        assert_eq!(fs::read_to_string(f.primary().join("config.toml")).unwrap(), "model = \"x\"\n");
    }

    #[test]
    fn an_append_only_file_is_merged_before_it_is_relinked() {
        let f = Fixture::new();
        ensure(&f.paths).unwrap();
        fs::remove_file(f.shallow().join("history.jsonl")).unwrap();
        // The CLI copied the old content and appended two new lines; meanwhile the app appended one too.
        fs::write(f.shallow().join("history.jsonl"), "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n{\"a\":4}\n").unwrap();
        fs::write(f.primary().join("history.jsonl"), "{\"a\":1}\n{\"a\":2}\n{\"a\":9}").unwrap(); // no final newline
        let r = repair(&f.paths, false).unwrap();
        assert_eq!(r.fixed, vec![Fixed::Merged { name: "history.jsonl".into(), lines: 2 }]);
        assert_eq!(fs::read_to_string(f.primary().join("history.jsonl")).unwrap(), "{\"a\":1}\n{\"a\":2}\n{\"a\":9}\n{\"a\":3}\n{\"a\":4}\n");
        assert_eq!(fs::read_link(f.shallow().join("history.jsonl")).unwrap(), f.primary().join("history.jsonl"));
    }

    #[test]
    fn a_folder_and_an_unknown_entry_are_reported_not_moved() {
        let f = Fixture::new();
        ensure(&f.paths).unwrap();
        fs::remove_file(f.shallow().join("sessions")).unwrap();
        fs::create_dir_all(f.shallow().join("sessions/new")).unwrap();
        fs::write(f.shallow().join("brand-new.json"), "{}").unwrap();
        let r = repair(&f.paths, false).unwrap();
        let kinds: Vec<_> = r.attention.iter().map(|d| (d.name.as_str(), d.kind.clone())).collect();
        assert!(kinds.contains(&("sessions", DriftKind::ReplacedByDir)) && kinds.contains(&("brand-new.json", DriftKind::Unknown)), "{kinds:?}");
        assert!(f.shallow().join("sessions/new").is_dir() && f.shallow().join("brand-new.json").is_file());
        assert!(f.primary().join("sessions/2026").is_dir());
    }

    #[test]
    fn a_missing_link_is_added_by_repair_and_a_wrong_one_is_reported() {
        let f = Fixture::new();
        ensure(&f.paths).unwrap();
        fs::remove_file(f.shallow().join("sessions")).unwrap();
        fs::remove_file(f.shallow().join("config.toml")).unwrap();
        symlink("/somewhere/else", f.shallow().join("config.toml")).unwrap();
        let r = repair(&f.paths, false).unwrap();
        assert_eq!(r.fixed, vec![Fixed::Linked("sessions".into())]);
        assert_eq!(r.attention, vec![Drift { name: "config.toml".into(), kind: DriftKind::WrongTarget }]);
    }

    #[test]
    fn join_auth_replaces_the_copy_with_a_link_to_the_shared_login() {
        let f = Fixture::new();
        ensure(&f.paths).unwrap();
        fs::write(f.shallow().join("auth.json"), "old copy").unwrap();
        join_auth(&f.paths).unwrap();
        assert_eq!(fs::read_link(f.shallow().join("auth.json")).unwrap(), f.primary().join("auth.json"));
        assert_eq!(fs::read_to_string(f.shallow().join("auth.json")).unwrap(), "{}");
        join_auth(&f.paths).unwrap(); // idempotent
    }

    #[test]
    fn ensure_needs_the_primary_home() {
        let d = tempfile::tempdir().unwrap();
        assert!(ensure(&Paths::from_home(d.path())).is_err());
    }
}
