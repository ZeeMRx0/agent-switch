//! Keep Claude Desktop's Code-tab session list across account switches.
//!
//! Desktop keeps one list per profile, in `claude-code-sessions/<accountUuid>/<orgUuid>/local_<id>.json`, so
//! a switch leaves the new account's Code tab empty even though the transcripts are shared. On every switch
//! this module copies those records between the profiles with a three-way sync: a manifest stores the hash
//! of each copy's last-synced bytes, so "changed since the last sync" is told apart from "only viewed".
//!
//! Rules: records are matched by `sessionId`; bytes are copied unchanged and never rewritten; a record that
//! does not parse or lacks a required key is never written over; a copy that is overwritten after a
//! conflict, or removed because its session was deleted elsewhere, is first moved or copied into
//! `backups/sessions/<stamp>/<profile>/` (nothing is unlinked). `plan` is a pure function, `execute` does
//! the I/O, and both are safe to repeat after a crash because the manifest is saved last.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::manage::stamp;
use super::{backups_dir, canon, claude_code_pids, default_profile, known_profile_dirs, profile_uuid, profiles_root, Registry};
use crate::error::{Error, Result};
use crate::fsutil::{ensure_private_dir, safe_write, WriteOpts};
use crate::locks::FlockGuard;
use crate::macos_apps::{bundle_pids, System, CLAUDE};
use crate::paths::Paths;
use crate::state::now_secs;

#[cfg(test)]
mod tests;

const MANIFEST_VERSION: u32 = 1;
const SESSIONS: &str = "claude-code-sessions";

// ---------------------------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------------------------

/// `claude-desktop/session-mirror.json`: ids and hashes only, never record contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// `false` after `mirror_off`: switches leave the session lists alone until `mirror_on`.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// The `<account>/<org>` folder each profile was last synced against. A profile that now resolves to
    /// another folder (a different account signed in) is left out of the sync instead of looking wiped.
    #[serde(default)]
    pub profiles: BTreeMap<String, String>,
    pub sessions: BTreeMap<String, Entry>,
}

fn enabled_by_default() -> bool {
    true
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The profile the session was first seen in; `mirror_off` restores the session there.
    pub origin: String,
    pub copies: BTreeMap<String, CopyState>,
    /// The session was deleted; `copies` lists the profiles whose copy still has to be moved to the backup
    /// folder because they were not readable when the deletion was planned.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyState {
    /// SHA-256 hex of the bytes as last synced.
    pub hash: String,
    pub present: bool,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest { version: MANIFEST_VERSION, enabled: true, profiles: BTreeMap::new(), sessions: BTreeMap::new() }
    }
}

impl Manifest {
    pub fn file(paths: &Paths) -> PathBuf {
        profiles_root(paths).join("session-mirror.json")
    }

    /// A missing file is an empty manifest; an unreadable, corrupt or newer one is an error (fail closed).
    pub fn load(file: &Path) -> Result<Manifest> {
        let text = match fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Manifest::default()),
            Err(e) => return Err(Error::io(format!("read {}", file.display()), e)),
        };
        let manifest: Manifest = serde_json::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", file.display())))?;
        if manifest.version != MANIFEST_VERSION {
            return Err(Error::Config(format!("{} has version {}, expected {MANIFEST_VERSION}", file.display(), manifest.version)));
        }
        Ok(manifest)
    }

    fn save(&self, file: &Path) -> Result<()> {
        if let Some(parent) = file.parent() {
            ensure_private_dir(parent)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        safe_write(file, text.as_bytes(), &WriteOpts::secret())
    }
}

// ---------------------------------------------------------------------------------------------
// Profiles and records
// ---------------------------------------------------------------------------------------------

/// A Desktop profile folder. In link mode the default folder is a symlink to a registered profile and
/// counts once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRef {
    pub id: String,
    pub dir: PathBuf,
    pub is_default: bool,
}

/// Registry profiles plus the app's default folder, de-duplicated by canonical path.
pub fn profiles(paths: &Paths) -> Result<Vec<ProfileRef>> {
    let registry = Registry::load(paths)?;
    let default = default_profile(paths);
    let default_canon = canon(&default);
    let mut out: Vec<ProfileRef> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for (id, entry) in &registry.profiles {
        let real = canon(&entry.dir);
        if entry.dir.is_dir() && !seen.contains(&real) {
            out.push(ProfileRef { id: id.clone(), dir: entry.dir.clone(), is_default: real == default_canon });
            seen.push(real);
        }
    }
    if default.is_dir() && !seen.contains(&default_canon) {
        out.push(ProfileRef { id: "default".into(), dir: default, is_default: true });
    }
    Ok(out)
}

/// The single `claude-code-sessions/<account>/<org>/` folder of a profile, or the reason there is none.
pub fn session_dir(profile_dir: &Path, account_uuid: &str) -> std::result::Result<PathBuf, String> {
    let account_dir = profile_dir.join(SESSIONS).join(account_uuid);
    let entries = fs::read_dir(&account_dir).map_err(|e| format!("no session folder at {} ({e})", account_dir.display()))?;
    let mut orgs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.') && e.path().is_dir())
        .map(|e| e.path())
        .collect();
    match orgs.len() {
        1 => Ok(orgs.remove(0)),
        0 => Err(format!("no organization folder under {}", account_dir.display())),
        n => Err(format!("{n} organization folders under {}; cannot tell which one the app uses", account_dir.display())),
    }
}

#[derive(Debug, Clone)]
pub struct Record {
    /// The session id, equal to the file stem (`local_<uuid>`).
    pub id: String,
    pub bytes: Vec<u8>,
    pub hash: String,
    pub value: Value,
    /// File birth time in nanoseconds since the epoch, when the filesystem reports one.
    pub born: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub records: Vec<Record>,
    pub skipped: Vec<Skipped>,
}

/// Turn edge time, turn count and activity time in comparison order.
pub type Key = (i64, i64, i64);

fn num(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0) as i64
}

impl Record {
    fn key(&self) -> Key {
        (num(&self.value, "lastTurnEdgeAt"), num(&self.value, "completedTurns"), num(&self.value, "lastActivityAt"))
    }

    fn title(&self) -> String {
        match self.value.get("title").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => self.id.clone(),
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// A session whose first turn never finished stores `completedTurns: null`, or has no such key at all
/// (a forced quit); `num` reads both as 0.
fn is_number_or_null(value: &Value) -> bool {
    value.is_number() || value.is_null()
}

type TypeCheck = (&'static str, fn(&Value) -> bool);

/// A key that may be absent but, when present, must pass `ok`.
fn absent_or(value: &Value, key: &str, ok: fn(&Value) -> bool) -> bool {
    value.get(key).is_none_or(ok)
}

/// The first required key that is missing or has the wrong type.
fn invalid_key(value: &Value, stem: &str) -> Option<&'static str> {
    if value.get("sessionId").and_then(Value::as_str) != Some(stem) {
        return Some("sessionId");
    }
    if !absent_or(value, "completedTurns", is_number_or_null) {
        return Some("completedTurns");
    }
    let required: [TypeCheck; 4] = [
        ("cliSessionId", Value::is_string),
        ("lastTurnEdgeAt", Value::is_number),
        ("isArchived", Value::is_boolean),
        ("createdAt", Value::is_number),
    ];
    required.iter().find(|(key, ok)| !value.get(*key).is_some_and(ok)).map(|(key, _)| *key)
}

fn load_one(path: &Path, stem: &str) -> std::result::Result<Record, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("cannot stat ({e})"))?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let bytes = fs::read(path).map_err(|e| format!("cannot read ({e})"))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| format!("not valid JSON ({e})"))?;
    if let Some(key) = invalid_key(&value, stem) {
        return Err(format!("`{key}` is missing or has an unexpected type"));
    }
    let born = meta.created().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos());
    Ok(Record { id: stem.to_string(), hash: sha256_hex(&bytes), bytes, value, born })
}

/// Every valid `local_*.json` in `dir`. Files that fail are listed in `skipped` and never written.
pub fn load(dir: &Path) -> Result<Loaded> {
    let mut loaded = Loaded::default();
    let entries = fs::read_dir(dir).map_err(|e| Error::io(format!("read {}", dir.display()), e))?;
    let mut names: Vec<String> = entries.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    for name in names {
        let Some(stem) = name.strip_suffix(".json").filter(|s| s.starts_with("local_")) else { continue };
        match load_one(&dir.join(&name), stem) {
            Ok(record) => loaded.records.push(record),
            Err(reason) => loaded.skipped.push(Skipped { id: stem.to_string(), reason }),
        }
    }
    Ok(loaded)
}

// ---------------------------------------------------------------------------------------------
// Busy guard
// ---------------------------------------------------------------------------------------------

fn is_busy(value: &Value) -> bool {
    let edge = num(value, "lastTurnEdgeAt");
    let reply_pending = value.get("latestUserFrameAt").and_then(Value::as_f64).is_some_and(|frame| frame as i64 > edge);
    let awaiting = value.get("interruptedAwaitingPerson").is_some_and(|v| !v.is_null());
    reply_pending || awaiting
}

/// Titles of the sessions of this profile that are mid-reply or waiting for a person. A profile that never
/// had a session folder has none; one whose folder exists but cannot be read or told apart (several
/// organization folders, an unreadable record folder) is an `Err` with the reason, so the caller can say so.
pub fn busy_sessions(profile_dir: &Path, account_uuid: &str) -> std::result::Result<Vec<String>, String> {
    if !profile_dir.join(SESSIONS).join(account_uuid).exists() {
        return Ok(Vec::new());
    }
    let dir = session_dir(profile_dir, account_uuid)?;
    let loaded = load(&dir).map_err(|e| e.to_string())?;
    let mut titles: Vec<String> = loaded.records.iter().filter(|r| is_busy(&r.value)).map(Record::title).collect();
    let waiting = fs::read_dir(dir.join("waiting-input")).map(|rd| rd.filter_map(|e| e.ok()).filter(|e| !e.file_name().to_string_lossy().starts_with('.')).count()).unwrap_or(0);
    if waiting > 0 {
        titles.push(format!("{waiting} request(s) waiting for your input"));
    }
    Ok(titles)
}

/// Whether every title in `current` appears in `offered`, a refusal's title list joined with ", ".
pub fn titles_within(current: &[String], offered: &str) -> bool {
    let padded = format!(", {offered}, ");
    current.iter().all(|t| padded.contains(&format!(", {t}, ")))
}

// ---------------------------------------------------------------------------------------------
// Plan (pure)
// ---------------------------------------------------------------------------------------------

/// What one profile holds for one session, as far as the plan needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub hash: String,
    pub key: Key,
    pub born: Option<u128>,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileState {
    pub id: String,
    /// `<account>/<org>`: the folder these records were read from.
    pub folder: String,
    pub seen: BTreeMap<String, Seen>,
    /// Files that exist but failed validation: never written, moved or counted as deleted.
    pub blocked: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWrite {
    pub session: String,
    pub from: String,
    pub to: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub session: String,
    pub profile: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub writes: Vec<PlannedWrite>,
    /// Copies saved to the backup folder because a write overwrites them.
    pub backups: Vec<Action>,
    /// Copies moved into the backup folder (a session deleted elsewhere, or `mirror_off`).
    pub moves: Vec<Action>,
    /// Saved last; `None` removes the manifest file.
    pub manifest: Option<Manifest>,
    /// Things the user should hear about: profiles left out of this run, deletions not propagated.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Changed,
    Unchanged,
    Deleted,
    Absent,
    Blocked,
}

fn classify(profile: &ProfileState, session: &str, entry: Option<&Entry>) -> Class {
    if profile.blocked.contains(session) {
        return Class::Blocked;
    }
    let known = entry.and_then(|e| e.copies.get(&profile.id));
    match profile.seen.get(session) {
        Some(seen) if known.is_some_and(|k| k.present && k.hash == seen.hash) => Class::Unchanged,
        Some(_) => Class::Changed,
        None if known.is_some_and(|k| k.present) => Class::Deleted,
        None => Class::Absent,
    }
}

/// The profile where the session first appeared: the earliest file birth time, else the default profile.
fn pick_origin(existing: &[(&ProfileState, &Seen, Class)], default_id: Option<&str>) -> String {
    if let [(profile, _, _)] = existing {
        return profile.id.clone();
    }
    let earliest = existing.iter().filter_map(|(p, s, _)| s.born.map(|b| (b, &p.id))).min();
    if let Some((_, id)) = earliest {
        return id.clone();
    }
    let default = default_id.and_then(|d| existing.iter().find(|(p, _, _)| p.id == d));
    default.or(existing.first()).map(|(p, _, _)| p.id.clone()).unwrap_or_default()
}

/// The profiles this run may classify. One whose folder differs from the folder it was synced against has
/// a different account signed in; its records say nothing about deletions here, so it sits the run out.
fn usable(manifest: &Manifest, profiles: &[ProfileState]) -> (Vec<ProfileState>, Vec<String>) {
    let mut notes = Vec::new();
    let kept = profiles
        .iter()
        .filter(|p| match manifest.profiles.get(&p.id) {
            Some(synced) if *synced != p.folder => {
                notes.push(format!("profile `{}` now uses another account folder; left out of this sync", p.id));
                false
            }
            _ => true,
        })
        .cloned()
        .collect();
    (kept, notes)
}

/// Profiles whose deletions must not propagate: one that lost every record it had, or that lost more than
/// two and over a quarter of them in a single run. That looks like a wipe or a restore, not a user.
fn mass_loss(manifest: &Manifest, profiles: &[ProfileState], notes: &mut Vec<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for p in profiles {
        let listed: Vec<&String> = manifest
            .sessions
            .iter()
            .filter(|(_, e)| !e.deleted && e.copies.get(&p.id).is_some_and(|c| c.present))
            .map(|(id, _)| id)
            .collect();
        let lost = listed.iter().filter(|id| !p.seen.contains_key(**id) && !p.blocked.contains(**id)).count();
        let wiped = !listed.is_empty() && p.seen.is_empty();
        if wiped || (lost > 2 && lost * 4 > listed.len()) {
            notes.push(format!("profile {} lost {lost} sessions at once; deletion not propagated", p.id));
            out.insert(p.id.clone());
        }
    }
    out
}

struct Ctx<'a> {
    profiles: &'a [ProfileState],
    default_id: Option<&'a str>,
    suppressed: &'a BTreeSet<String>,
}

/// Decide, for every session, what to write, back up and move. Does no I/O.
pub fn plan(manifest: &Manifest, profiles: &[ProfileState], default_id: Option<&str>) -> SyncPlan {
    let (profiles, mut notes) = usable(manifest, profiles);
    let suppressed = mass_loss(manifest, &profiles, &mut notes);
    let ctx = Ctx { profiles: &profiles, default_id, suppressed: &suppressed };
    let mut out = SyncPlan::default();
    let mut next = manifest.clone();
    for p in profiles.iter().filter(|p| !p.folder.is_empty()) {
        next.profiles.insert(p.id.clone(), p.folder.clone());
    }
    let ids: BTreeSet<&String> = manifest.sessions.keys().chain(profiles.iter().flat_map(|p| p.seen.keys())).collect();
    for id in ids {
        plan_session(id, manifest.sessions.get(id), &ctx, &mut out, &mut next);
    }
    out.manifest = Some(next);
    out.notes = notes;
    out
}

/// Copies the manifest lists in profiles that are not part of this run.
fn unread_copies(entry: &Entry, profiles: &[ProfileState]) -> BTreeMap<String, CopyState> {
    entry.copies.iter().filter(|(p, c)| c.present && !profiles.iter().any(|x| &x.id == *p)).map(|(p, c)| (p.clone(), c.clone())).collect()
}

/// A deleted session whose copies sat in profiles that could not be read: finish moving them now.
fn plan_tombstone(id: &str, entry: &Entry, ctx: &Ctx, out: &mut SyncPlan, next: &mut Manifest) {
    for p in ctx.profiles.iter().filter(|p| entry.copies.contains_key(&p.id) && p.seen.contains_key(id)) {
        out.moves.push(Action { session: id.to_string(), profile: p.id.clone() });
    }
    let remaining = unread_copies(entry, ctx.profiles);
    if remaining.is_empty() {
        next.sessions.remove(id);
    } else {
        next.sessions.insert(id.to_string(), Entry { origin: entry.origin.clone(), copies: remaining, deleted: true });
    }
}

fn plan_session(id: &str, entry: Option<&Entry>, ctx: &Ctx, out: &mut SyncPlan, next: &mut Manifest) {
    if let Some(entry) = entry.filter(|e| e.deleted) {
        return plan_tombstone(id, entry, ctx, out, next);
    }
    let classes: Vec<(&ProfileState, Class)> = ctx.profiles.iter().map(|p| (p, classify(p, id, entry))).collect();
    let existing: Vec<(&ProfileState, &Seen, Class)> = classes.iter().filter_map(|(p, c)| p.seen.get(id).map(|s| (*p, s, *c))).collect();
    let any_blocked = classes.iter().any(|(_, c)| *c == Class::Blocked);
    let deleted_in = |suppressed: bool| classes.iter().any(|(p, c)| *c == Class::Deleted && ctx.suppressed.contains(&p.id) == suppressed);

    if deleted_in(true) {
        return; // a wipe, not a deletion: keep the entry and the files as they are
    }
    if deleted_in(false) {
        out.moves.extend(existing.iter().map(|(p, _, _)| Action { session: id.to_string(), profile: p.id.clone() }));
        match entry.map(|e| (e, unread_copies(e, ctx.profiles))) {
            Some((e, pending)) if !pending.is_empty() => {
                next.sessions.insert(id.to_string(), Entry { origin: e.origin.clone(), copies: pending, deleted: true });
            }
            _ => {
                next.sessions.remove(id);
            }
        }
        return;
    }
    if existing.is_empty() {
        if !any_blocked {
            next.sessions.remove(id);
        }
        return;
    }

    let changed: Vec<_> = existing.iter().filter(|(_, _, c)| *c == Class::Changed).collect();
    let pool: Vec<_> = if changed.is_empty() { existing.iter().collect() } else { changed };
    let (winner_profile, winner, _) = *pool
        .iter()
        .max_by(|a, b| a.1.key.cmp(&b.1.key).then_with(|| a.1.hash.cmp(&b.1.hash)).then_with(|| b.0.id.cmp(&a.0.id)))
        .expect("the pool is not empty");

    for (profile, class) in &classes {
        if *class == Class::Blocked || profile.seen.get(id).is_some_and(|s| s.hash == winner.hash) {
            continue;
        }
        if *class == Class::Changed {
            out.backups.push(Action { session: id.to_string(), profile: profile.id.clone() });
        }
        out.writes.push(PlannedWrite { session: id.to_string(), from: winner_profile.id.clone(), to: profile.id.clone(), hash: winner.hash.clone() });
    }

    let origin = entry.map(|e| e.origin.clone()).unwrap_or_else(|| pick_origin(&existing, ctx.default_id));
    let mut copies = entry.map(|e| e.copies.clone()).unwrap_or_default();
    for (profile, class) in &classes {
        if *class != Class::Blocked {
            copies.insert(profile.id.clone(), CopyState { hash: winner.hash.clone(), present: true });
        }
    }
    next.sessions.insert(id.to_string(), Entry { origin, copies, deleted: false });
}

/// Turn a sync plan into the plan for switching mirroring off: each session's winner goes to its origin
/// profile and every other copy the manifest lists moves to the backup folder. The manifest stays, marked disabled.
/// Only sessions the manifest already knew are touched; copies that were never mirrored stay.
fn off_plan(manifest: &Manifest, synced: SyncPlan, profiles: &[ProfileState]) -> SyncPlan {
    let mut out = SyncPlan {
        moves: synced.moves.clone(),
        notes: synced.notes,
        manifest: Some(Manifest { enabled: false, ..Manifest::default() }),
        ..SyncPlan::default()
    };
    let mut moved: BTreeSet<(String, String)> = out.moves.iter().map(|m| (m.session.clone(), m.profile.clone())).collect();
    for (session, entry) in manifest.sessions.iter().filter(|(_, e)| !e.deleted) {
        if !profiles.iter().any(|p| p.id == entry.origin) {
            continue; // the origin folder is not readable now; leave this session where it is
        }
        if synced.moves.iter().any(|m| &m.session == session) {
            continue; // the synced plan treats it as deleted elsewhere: its copies are already moving out
        }
        out.writes.extend(synced.writes.iter().filter(|w| &w.session == session && w.to == entry.origin).cloned());
        out.backups.extend(synced.backups.iter().filter(|b| &b.session == session && b.profile == entry.origin).cloned());
        for profile in profiles.iter().filter(|p| p.id != entry.origin && entry.copies.contains_key(&p.id)) {
            if profile.seen.contains_key(session) && moved.insert((session.clone(), profile.id.clone())) {
                out.moves.push(Action { session: session.clone(), profile: profile.id.clone() });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Execute
// ---------------------------------------------------------------------------------------------

/// Where the plan's files live.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Profile id to its session folder.
    pub dirs: BTreeMap<String, PathBuf>,
    /// `backups/sessions/<stamp>`; each profile gets a subfolder.
    pub backup_root: PathBuf,
    pub manifest_file: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub mirrored: usize,
    pub backed_up: usize,
}

impl Stats {
    pub fn line(&self) -> String {
        if *self == Stats::default() {
            "sessions: already in sync".to_string()
        } else {
            format!("sessions: {} mirrored, {} backed up", self.mirrored, self.backed_up)
        }
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temporary name that `local_*.json` matching never picks up.
pub fn temp_name() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!(".agent-switch-{:x}-{nanos:x}-{}.tmp", std::process::id(), TEMP_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn file_name(session: &str) -> String {
    format!("{session}.json")
}

fn dir_of<'a>(layout: &'a Layout, profile: &str) -> Result<&'a PathBuf> {
    layout.dirs.get(profile).ok_or_else(|| Error::Invalid(format!("no session folder known for profile `{profile}`")))
}

/// Create a new file (never follows or replaces anything), mode 0600, flushed to disk.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::io(format!("create {}", path.display()), e))?;
    file.write_all(bytes).map_err(|e| Error::io(format!("write {}", path.display()), e))?;
    file.sync_all().map_err(|e| Error::io(format!("fsync {}", path.display()), e))
}

/// Temp file in the same folder, fsync, then `rename` over `name`.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let tmp = dir.join(temp_name());
    if let Err(e) = write_new(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dir.join(name)).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::io(format!("rename into {}", dir.join(name).display()), e)
    })?;
    fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| Error::io(format!("fsync folder {}", dir.display()), e))
}

/// A free destination in `<backup_root>/<profile>/`; an existing name gets a numeric suffix.
fn backup_dest(layout: &Layout, profile: &str, name: &str) -> Result<PathBuf> {
    let dir = layout.backup_root.join(profile);
    ensure_private_dir(&dir)?;
    let mut dest = dir.join(name);
    let mut n = 1;
    while dest.exists() {
        dest = dir.join(format!("{name}.{n}"));
        n += 1;
    }
    Ok(dest)
}

fn backup_copy(layout: &Layout, action: &Action) -> Result<()> {
    let name = file_name(&action.session);
    let src = dir_of(layout, &action.profile)?.join(&name);
    let bytes = fs::read(&src).map_err(|e| Error::io(format!("read {}", src.display()), e))?;
    write_new(&backup_dest(layout, &action.profile, &name)?, &bytes)
}

/// Rename into the backup folder; copy then remove when the folders are on different filesystems.
fn backup_move(layout: &Layout, action: &Action) -> Result<()> {
    let name = file_name(&action.session);
    let src = dir_of(layout, &action.profile)?.join(&name);
    let dest = backup_dest(layout, &action.profile, &name)?;
    if fs::rename(&src, &dest).is_ok() {
        return Ok(());
    }
    let bytes = fs::read(&src).map_err(|e| Error::io(format!("read {}", src.display()), e))?;
    write_new(&dest, &bytes)?;
    fs::remove_file(&src).map_err(|e| Error::io(format!("remove {}", src.display()), e))
}

/// Read every source record and check it still has the planned hash, before anything is written.
fn read_sources(plan: &SyncPlan, layout: &Layout) -> Result<Vec<Vec<u8>>> {
    plan.writes
        .iter()
        .map(|w| {
            let path = dir_of(layout, &w.from)?.join(file_name(&w.session));
            let bytes = fs::read(&path).map_err(|e| Error::io(format!("read {}", path.display()), e))?;
            if sha256_hex(&bytes) != w.hash {
                return Err(Error::Invalid(format!("{} changed while the sync was planned; nothing was written", path.display())));
            }
            Ok(bytes)
        })
        .collect()
}

/// Apply the plan: backups, moves, winner writes, manifest last. `is_running` is asked once, right before
/// the first write; when it is true nothing is changed.
pub fn execute(plan: &SyncPlan, layout: &Layout, is_running: impl Fn() -> bool) -> Result<Stats> {
    let payloads = read_sources(plan, layout)?;
    if is_running() {
        return Err(Error::Invalid("Claude is running; session records were left untouched".into()));
    }
    for action in &plan.backups {
        backup_copy(layout, action)?;
    }
    for action in &plan.moves {
        backup_move(layout, action)?;
    }
    for (write, bytes) in plan.writes.iter().zip(&payloads) {
        write_atomic(dir_of(layout, &write.to)?, &file_name(&write.session), bytes)?;
    }
    match &plan.manifest {
        Some(manifest) if !manifest.enabled || !manifest.sessions.is_empty() || layout.manifest_file.exists() => manifest.save(&layout.manifest_file)?,
        Some(_) => {}
        None => match fs::remove_file(&layout.manifest_file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(format!("remove {}", layout.manifest_file.display()), e)),
        },
    }
    Ok(Stats { mirrored: plan.writes.len(), backed_up: plan.backups.len() + plan.moves.len() })
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub stats: Stats,
    /// Profiles or records that were skipped, with the reason.
    pub notes: Vec<String>,
    /// Mirroring is switched off (`mirror_off`); nothing was read or written.
    pub disabled: bool,
}

impl Outcome {
    /// One line for the switch output.
    pub fn line(&self) -> String {
        if self.disabled {
            "session mirroring is off".to_string()
        } else if self.notes.is_empty() {
            self.stats.line()
        } else {
            format!("{}; notes: {}", self.stats.line(), self.notes.join("; "))
        }
    }
}

/// `<account>/<org>`, the last two components of a session folder.
fn folder_label(dir: &Path) -> String {
    let parts: Vec<_> = dir.components().rev().take(2).map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    format!("{}/{}", parts.get(1).map_or("", String::as_str), parts.first().map_or("", String::as_str))
}

struct Scanned {
    profile: ProfileRef,
    dir: PathBuf,
    loaded: Loaded,
}

impl Scanned {
    fn state(&self) -> ProfileState {
        let seen = self
            .loaded
            .records
            .iter()
            .map(|r| (r.id.clone(), Seen { hash: r.hash.clone(), key: r.key(), born: r.born }))
            .collect();
        ProfileState { id: self.profile.id.clone(), folder: folder_label(&self.dir), seen, blocked: self.loaded.skipped.iter().map(|s| s.id.clone()).collect() }
    }
}

fn scan_one(profile: &ProfileRef) -> std::result::Result<Scanned, String> {
    let uuid = profile_uuid(&profile.dir)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no signed-in account yet".to_string())?;
    let dir = session_dir(&profile.dir, &uuid)?;
    let loaded = load(&dir).map_err(|e| e.to_string())?;
    Ok(Scanned { profile: profile.clone(), dir, loaded })
}

/// Every profile with a usable session folder, plus a note for each profile or record that was skipped.
fn scan(paths: &Paths) -> Result<(Vec<Scanned>, Vec<String>)> {
    let mut scanned = Vec::new();
    let mut notes = Vec::new();
    for profile in profiles(paths)? {
        match scan_one(&profile) {
            Ok(s) => {
                notes.extend(s.loaded.skipped.iter().map(|k| format!("`{}` in `{}` ({})", k.id, profile.id, k.reason)));
                scanned.push(s);
            }
            Err(reason) => notes.push(format!("profile `{}` ({reason})", profile.id)),
        }
    }
    Ok((scanned, notes))
}

/// Read-only: why a profile or record would be left out of the next sync (no sign-in, no session folder,
/// an invalid record). Empty when everything is usable.
pub fn skipped_notes(paths: &Paths) -> Result<Vec<String>> {
    Ok(scan(paths)?.1)
}

/// Start of the refusal `ClaudeDesktop::apply` returns when a session is mid-reply.
pub const BUSY_PREFIX: &str = "Claude has work in progress: ";

/// The session titles in a busy refusal's text (as `Error` displays it), or `None` for any other error.
pub fn busy_titles_in(message: &str) -> Option<String> {
    let rest = &message[message.find(BUSY_PREFIX)? + BUSY_PREFIX.len()..];
    let end = rest.find(". Nothing was quit")?;
    Some(rest[..end].to_string())
}

fn layout_of(paths: &Paths, scanned: &[Scanned]) -> Layout {
    Layout {
        dirs: scanned.iter().map(|s| (s.profile.id.clone(), s.dir.clone())).collect(),
        backup_root: backups_dir(paths).join("sessions").join(stamp(now_secs())),
        manifest_file: Manifest::file(paths),
    }
}

fn default_id(scanned: &[Scanned]) -> Option<&str> {
    scanned.iter().find(|s| s.profile.is_default).map(|s| s.profile.id.as_str())
}

const MANIFEST_LOCK_TIMEOUT: Duration = Duration::from_millis(if cfg!(test) { 300 } else { 5_000 });

/// Serializes manifest read-plan-write cycles across processes.
pub fn lock(paths: &Paths) -> Result<FlockGuard> {
    FlockGuard::lock(&paths.locks_dir().join("claude-desktop-sessions.lock"), MANIFEST_LOCK_TIMEOUT)
}

fn outcome(stats: Stats, notes: Vec<String>) -> Outcome {
    Outcome { stats, notes, disabled: false }
}

/// Mirror the session records of every profile. Call it only while Claude is not running.
pub fn sync(paths: &Paths, is_running: impl Fn() -> bool) -> Result<Outcome> {
    let _lock = lock(paths)?;
    let manifest = Manifest::load(&Manifest::file(paths))?;
    if !manifest.enabled {
        return Ok(Outcome { stats: Stats::default(), notes: Vec::new(), disabled: true });
    }
    let (scanned, mut notes) = scan(paths)?;
    if scanned.len() < 2 {
        return Ok(outcome(Stats::default(), notes));
    }
    let states: Vec<ProfileState> = scanned.iter().map(Scanned::state).collect();
    let plan = plan(&manifest, &states, default_id(&scanned));
    let stats = execute(&plan, &layout_of(paths, &scanned), is_running)?;
    notes.extend(plan.notes);
    Ok(outcome(stats, notes))
}

/// Any process of the app bundle, or a Claude Code agent it started from a profile folder, is alive; an
/// unreadable process list counts as alive (fail closed).
pub fn claude_alive(sys: &dyn System, paths: &Paths) -> bool {
    sys.processes()
        .map(|p| !bundle_pids(&p, &CLAUDE).is_empty() || !claude_code_pids(&p, &known_profile_dirs(paths)).is_empty())
        .unwrap_or(true)
}

/// Switch mirroring off and keep it off: every session's newest content goes to its origin profile, the
/// other manifest-listed copies move to the backup folder, and the manifest is marked disabled so the next
/// switch does not mirror again. Sessions that were never mirrored are not touched. Refuses while Claude runs.
pub fn mirror_off(paths: &Paths, sys: &dyn System) -> Result<Outcome> {
    if claude_alive(sys, paths) {
        return Err(Error::Invalid("Claude is running; quit it before turning session mirroring off".into()));
    }
    let _lock = lock(paths)?;
    let manifest = Manifest::load(&Manifest::file(paths))?;
    if !manifest.enabled {
        return Ok(Outcome { stats: Stats::default(), notes: Vec::new(), disabled: true });
    }
    let (scanned, notes) = scan(paths)?;
    let (states, _) = usable(&manifest, &scanned.iter().map(Scanned::state).collect::<Vec<_>>());
    let plan = off_plan(&manifest, plan(&manifest, &states, default_id(&scanned)), &states);
    let stats = execute(&plan, &layout_of(paths, &scanned), || claude_alive(sys, paths))?;
    let mut notes = notes;
    notes.extend(plan.notes);
    Ok(outcome(stats, notes))
}

/// Switch mirroring on again after `mirror_off`. The next switch starts from the sessions each profile has.
pub fn mirror_on(paths: &Paths) -> Result<()> {
    let _lock = lock(paths)?;
    let file = Manifest::file(paths);
    let mut manifest = Manifest::load(&file)?;
    if manifest.enabled {
        return Ok(());
    }
    manifest.enabled = true;
    manifest.save(&file)
}
