//! Locks.
//!
//! * `DirLock`: a directory lock compatible with Node's `proper-lockfile`, which Claude Code uses
//!   (`mkdir` is the mutex; a lock whose mtime is older than `stale` is dead). Ported from claude-swap
//!   `claude_locks.py` (staleness numbers, serialized takeover through a sibling `.takeover` directory).
//! * `AccountLock`: one cross-process `flock` per account, held across every vault read, refresh,
//!   write, save-back, activate and undo for that account.
//! * `WatcherLock`: singleton `flock` so only one watcher runs (claude-swap `switcher.py:2378-2395`).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use rustix::fs::{flock, FlockOperation};
use rustix::io::Errno;

use crate::config::validate_account_id;
use crate::error::{Error, Result};
use crate::paths::Paths;

const TAKEOVER_SUFFIX: &str = ".takeover";
/// A takeover directory is held for one stat + rmdir; older than this, its holder died mid-takeover.
const TAKEOVER_STALE: Duration = Duration::from_secs(5);

/// Small jittered pause without pulling in a random-number crate.
fn jitter(base_ms: u64) -> Duration {
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0);
    Duration::from_millis(base_ms + nanos % base_ms.max(1))
}

// ---------------------------------------------------------------------------------------------
// Directory lock (proper-lockfile compatible)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct DirLockOpts {
    /// A lock whose mtime is older than this belongs to a dead holder.
    pub stale: Duration,
    /// How often the holder refreshes the mtime. Faster than Claude Code's 5 s, for margin.
    pub touch_every: Duration,
    /// How long to wait for a live holder.
    pub timeout: Duration,
}

impl DirLockOpts {
    /// `~/.claude/.oauth_refresh.lock` and `~/.claude.lock`: stale after 60 s.
    pub const fn credentials() -> Self {
        DirLockOpts { stale: Duration::from_secs(60), touch_every: Duration::from_secs(3), timeout: Duration::from_secs(9) }
    }

    /// `~/.claude.json.lock`: stale after 10 s.
    pub const fn config() -> Self {
        DirLockOpts { stale: Duration::from_secs(10), touch_every: Duration::from_secs(3), timeout: Duration::from_secs(9) }
    }
}

pub struct DirLock {
    path: PathBuf,
    stop: Option<Sender<()>>,
    toucher: Option<JoinHandle<()>>,
}

fn age(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(SystemTime::now().duration_since(modified).unwrap_or_default())
}

/// Remove `lock` if it is still stale, serialized through the takeover directory so two waiters that saw
/// the same dead holder cannot delete each other's fresh lock. Returns true if this call removed it.
fn remove_if_still_stale(lock: &Path, stale: Duration) -> bool {
    let mut name = lock.as_os_str().to_owned();
    name.push(TAKEOVER_SUFFIX);
    let takeover = PathBuf::from(name);
    match std::fs::create_dir(&takeover) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if age(&takeover).is_some_and(|a| a > TAKEOVER_STALE) {
                let _ = std::fs::remove_dir(&takeover);
            }
            return false;
        }
        Err(_) => return false,
    }
    // Re-check inside: the waiter before us may already have replaced the dead lock with a fresh one.
    let removed = age(lock).is_some_and(|a| a > stale) && std::fs::remove_dir(lock).is_ok();
    let _ = std::fs::remove_dir(&takeover);
    removed
}

impl DirLock {
    pub fn acquire(path: &Path, opts: DirLockOpts) -> Result<DirLock> {
        let deadline = Instant::now() + opts.timeout;
        loop {
            match std::fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => return Ok(Self::start_toucher(path.to_path_buf(), opts.touch_every)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if age(path).is_some_and(|a| a > opts.stale) && remove_if_still_stale(path, opts.stale) {
                        continue;
                    }
                }
                Err(e) => return Err(Error::io(format!("create lock {}", path.display()), e)),
            }
            if Instant::now() >= deadline {
                return Err(Error::LockBusy(format!("{} is held by another process", path.display())));
            }
            std::thread::sleep(jitter(50));
        }
    }

    fn start_toucher(path: PathBuf, every: Duration) -> DirLock {
        let (tx, rx) = mpsc::channel::<()>();
        let touch_path = path.clone();
        let handle = std::thread::spawn(move || loop {
            match rx.recv_timeout(every) {
                Err(RecvTimeoutError::Timeout) => {
                    if let Ok(dir) = File::open(&touch_path) {
                        let _ = dir.set_modified(SystemTime::now());
                    }
                }
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            }
        });
        DirLock { path, stop: Some(tx), toucher: Some(handle) }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(h) = self.toucher.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

// ---------------------------------------------------------------------------------------------
// flock-based locks
// ---------------------------------------------------------------------------------------------

/// An exclusive `flock` that lasts as long as the value lives (and is dropped by the OS if we crash).
pub struct FlockGuard {
    file: File,
    path: PathBuf,
}

fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| Error::io(format!("create folder {}", dir.display()), e))?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(|e| Error::io(format!("open lock file {}", path.display()), e))
}

impl FlockGuard {
    /// `Ok(None)` when another holder has it.
    pub fn try_lock(path: &Path) -> Result<Option<FlockGuard>> {
        let file = open_lock_file(path)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(FlockGuard { file, path: path.to_path_buf() })),
            Err(Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(Error::io(format!("lock {}", path.display()), e.into())),
        }
    }

    pub fn lock(path: &Path, timeout: Duration) -> Result<FlockGuard> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(g) = Self::try_lock(path)? {
                return Ok(g);
            }
            if Instant::now() >= deadline {
                return Err(Error::LockBusy(format!("{} is held by another process", path.display())));
            }
            std::thread::sleep(jitter(10));
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Held across every vault read, refresh, write, save-back, activate and undo for one account.
pub struct AccountLock(#[allow(dead_code)] FlockGuard);

impl AccountLock {
    pub fn acquire(paths: &Paths, account_id: &str, timeout: Duration) -> Result<AccountLock> {
        validate_account_id(account_id)?; // keeps `../` out of the lock file name
        FlockGuard::lock(&paths.account_lock_file(account_id), timeout).map(AccountLock)
    }
}

/// Lock several accounts in sorted order, so two switches touching the same pair cannot deadlock.
pub fn lock_accounts(paths: &Paths, ids: &[&str], timeout: Duration) -> Result<Vec<AccountLock>> {
    let mut sorted: Vec<&str> = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.into_iter().map(|id| AccountLock::acquire(paths, id, timeout)).collect()
}

/// Only one watcher may run. The file also records the owner's pid for diagnostics.
pub struct WatcherLock(FlockGuard);

impl WatcherLock {
    pub fn try_acquire(paths: &Paths) -> Result<WatcherLock> {
        let path = paths.watcher_lock_file();
        match FlockGuard::try_lock(&path)? {
            Some(mut guard) => {
                let _ = guard.file.set_len(0);
                let _ = guard.file.write_all(std::process::id().to_string().as_bytes());
                Ok(WatcherLock(guard))
            }
            None => {
                let mut pid = String::new();
                if let Ok(mut f) = File::open(&path) {
                    let _ = f.read_to_string(&mut pid);
                }
                Err(Error::LockBusy(format!("another watcher is already running (pid {})", pid.trim())))
            }
        }
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn quick(stale_ms: u64, timeout_ms: u64) -> DirLockOpts {
        DirLockOpts {
            stale: Duration::from_millis(stale_ms),
            touch_every: Duration::from_millis(40),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    fn backdate(dir: &Path, by: Duration) {
        File::open(dir).unwrap().set_modified(SystemTime::now() - by).unwrap();
    }

    #[test]
    fn dir_lock_excludes_and_releases() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(".oauth_refresh.lock");
        let first = DirLock::acquire(&p, DirLockOpts::credentials()).unwrap();
        assert!(p.is_dir());
        assert!(matches!(DirLock::acquire(&p, quick(60_000, 150)), Err(Error::LockBusy(_))));
        drop(first);
        assert!(!p.exists());
        DirLock::acquire(&p, quick(60_000, 150)).unwrap();
    }

    #[test]
    fn dir_lock_takes_over_a_stale_lock_but_not_a_fresh_one() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("l.lock");
        std::fs::create_dir(&p).unwrap();
        assert!(DirLock::acquire(&p, quick(10_000, 150)).is_err(), "fresh lock must not be stolen");
        backdate(&p, Duration::from_secs(3600));
        let g = DirLock::acquire(&p, DirLockOpts::credentials()).unwrap();
        assert!(age(&p).unwrap() < Duration::from_secs(5), "takeover creates a fresh lock");
        drop(g);
    }

    #[test]
    fn dir_lock_staleness_thresholds_match_claude_code() {
        assert_eq!(DirLockOpts::credentials().stale, Duration::from_secs(60));
        assert_eq!(DirLockOpts::config().stale, Duration::from_secs(10));
        // 30 s old: dead for the config lock, alive for the credentials lock.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.lock");
        std::fs::create_dir(&p).unwrap();
        backdate(&p, Duration::from_secs(30));
        let short = DirLockOpts { timeout: Duration::from_millis(100), ..DirLockOpts::credentials() };
        assert!(DirLock::acquire(&p, short).is_err());
        DirLock::acquire(&p, DirLockOpts::config()).unwrap();
    }

    #[test]
    fn live_holder_keeps_its_lock_by_touching_it() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.lock");
        let _held = DirLock::acquire(&p, quick(300, 100)).unwrap();
        std::thread::sleep(Duration::from_millis(700)); // well past `stale`
        assert!(matches!(DirLock::acquire(&p, quick(300, 100)), Err(Error::LockBusy(_))));
    }

    #[test]
    fn racing_takeovers_never_overlap() {
        let d = tempfile::tempdir().unwrap();
        let p = Arc::new(d.path().join("r.lock"));
        std::fs::create_dir(&*p).unwrap();
        backdate(&p, Duration::from_secs(3600));
        let inside = Arc::new(AtomicUsize::new(0));
        let overlaps = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (p, inside, overlaps) = (p.clone(), inside.clone(), overlaps.clone());
                std::thread::spawn(move || {
                    let g = DirLock::acquire(&p, quick(10_000, 10_000)).unwrap();
                    if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                        overlaps.fetch_add(1, Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(15));
                    inside.fetch_sub(1, Ordering::SeqCst);
                    drop(g);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(overlaps.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn account_lock_contention_and_release() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        let held = AccountLock::acquire(&paths, "personal", Duration::from_secs(1)).unwrap();
        assert!(matches!(AccountLock::acquire(&paths, "personal", Duration::from_millis(100)), Err(Error::LockBusy(_))));
        AccountLock::acquire(&paths, "team", Duration::from_millis(100)).unwrap(); // other accounts unaffected
        drop(held);
        AccountLock::acquire(&paths, "personal", Duration::from_millis(100)).unwrap();
    }

    #[test]
    fn account_lock_rejects_path_traversal_ids() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        assert!(AccountLock::acquire(&paths, "../evil", Duration::from_millis(10)).is_err());
    }

    #[test]
    fn lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        let _l = AccountLock::acquire(&paths, "a", Duration::from_millis(50)).unwrap();
        let mode = std::fs::metadata(paths.account_lock_file("a")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn lock_accounts_dedups_and_orders() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        let locks = lock_accounts(&paths, &["b", "a", "b"], Duration::from_millis(100)).unwrap();
        assert_eq!(locks.len(), 2);
    }

    #[test]
    fn watcher_is_a_singleton_and_records_its_pid() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(d.path());
        let first = WatcherLock::try_acquire(&paths).unwrap();
        let err = WatcherLock::try_acquire(&paths).err().expect("second watcher must be refused");
        assert!(err.to_string().contains(&std::process::id().to_string()), "{err}");
        drop(first);
        // Tests that spawn processes run in parallel with this one. Between fork and exec a child still holds
        // a copy of every open descriptor, and a flock lives as long as any copy does, so the release can
        // lag by a few milliseconds. Real code never depends on the release being immediate.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match WatcherLock::try_acquire(&paths) {
                Ok(_) => break,
                Err(Error::LockBusy(_)) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => panic!("the lock was not released after the first holder dropped it: {e}"),
            }
        }
    }
}
