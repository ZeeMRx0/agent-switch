//! Atomic, symlink-safe file writes.
//!
//! Ported from caam `internal/authfile/authfile.go` (`:607-612` forced 0600, `:2069-2090` atomic
//! temp+rename, `:2125` fsync), tightened with `O_NOFOLLOW` on the parent folder and `openat`/`renameat`
//! against that one folder handle, so a symlink swapped in after the check cannot redirect the write.

use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{fchmod, fsync, openat, renameat, unlinkat, AtFlags, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct WriteOpts {
    /// Final permission bits. Forced with `fchmod`, so the umask and any previous file mode are ignored.
    pub mode: u32,
    /// Parent folders that may be symlinks (for example a shallow home). Everything else is refused.
    pub allow_symlinked_parents: Vec<PathBuf>,
    /// Replace a symlink sitting at the file path. Off by default: a symlinked dotfile belongs to a dotfile
    /// manager, and a write that turned it into a regular file would silently break that setup.
    pub replace_symlink: bool,
}

impl WriteOpts {
    /// Mode 0600 for anything that holds a secret.
    pub fn secret() -> Self {
        WriteOpts { mode: 0o600, allow_symlinked_parents: Vec::new(), replace_symlink: false }
    }

    /// Keep the given mode, for a file whose permissions are the user's.
    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }

    pub fn replace_symlink(mut self) -> Self {
        self.replace_symlink = true;
        self
    }

    pub fn allow_symlinked_parent(mut self, parent: impl Into<PathBuf>) -> Self {
        self.allow_symlinked_parents.push(parent.into());
        self
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn open_parent(parent: &Path, opts: &WriteOpts) -> Result<OwnedFd> {
    let base = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    match rustix::fs::open(parent, base | OFlags::NOFOLLOW, Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(Errno::LOOP) | Err(Errno::NOTDIR) => {
            let is_link = std::fs::symlink_metadata(parent).map(|m| m.file_type().is_symlink()).unwrap_or(false);
            if !is_link {
                return Err(Error::io(format!("open folder {}", parent.display()), Errno::NOTDIR.into()));
            }
            if opts.allow_symlinked_parents.iter().any(|p| p == parent) {
                rustix::fs::open(parent, base, Mode::empty())
                    .map_err(|e| Error::io(format!("open folder {}", parent.display()), e.into()))
            } else {
                Err(Error::SymlinkedParent(parent.to_path_buf()))
            }
        }
        Err(e) => Err(Error::io(format!("open folder {}", parent.display()), e.into())),
    }
}

/// Create a new file next to `name` that must not exist yet (`O_EXCL`), never following a symlink.
fn create_exclusive(dir: &OwnedFd, name: &str) -> std::result::Result<OwnedFd, Errno> {
    openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
}

fn create_temp(dir: &OwnedFd, file_name: &str) -> Result<(String, OwnedFd)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    for _ in 0..32 {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = format!(".{file_name}.tmp-{}-{nanos:x}-{n}", std::process::id());
        match create_exclusive(dir, &tmp) {
            Ok(fd) => return Ok((tmp, fd)),
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(Error::io(format!("create temp file for {file_name}"), e.into())),
        }
    }
    Err(Error::Invalid(format!("could not find a free temp name for {file_name}")))
}

/// Write `bytes` to `path` atomically: temp file with `O_EXCL`, fsync, `renameat`, fsync the folder.
/// A symlink at `path` is never followed. It is refused with `Error::SymlinkedFile`, unless
/// `opts.replace_symlink` says the caller owns the link, in which case it is replaced by a regular file.
pub fn safe_write(path: &Path, bytes: &[u8], opts: &WriteOpts) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Invalid(format!("not a file path: {}", path.display())))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let dir = open_parent(parent, opts)?;
    if !opts.replace_symlink && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::SymlinkedFile(path.to_path_buf()));
    }
    let (tmp, fd) = create_temp(&dir, file_name)?;

    let result = (|| -> Result<()> {
        fchmod(&fd, Mode::from_raw_mode(opts.mode as _)).map_err(|e| Error::io("chmod temp file", e.into()))?;
        let mut file = File::from(fd);
        file.write_all(bytes).map_err(|e| Error::io("write temp file", e))?;
        file.sync_all().map_err(|e| Error::io("fsync temp file", e))?;
        drop(file);
        renameat(&dir, tmp.as_str(), &dir, file_name)
            .map_err(|e| Error::io(format!("rename into {}", path.display()), e.into()))?;
        fsync(&dir).map_err(|e| Error::io("fsync folder", e.into()))
    })();

    if result.is_err() {
        let _ = unlinkat(&dir, tmp.as_str(), AtFlags::empty());
    }
    result
}

/// Create `dir` (and parents) with mode 0700 when missing.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| Error::io(format!("create folder {}", dir.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn writes_new_file_with_mode_0600() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        safe_write(&p, b"one", &WriteOpts::secret()).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        assert_eq!(mode_of(&p), 0o600);
    }

    #[test]
    fn forces_0600_over_a_looser_existing_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        std::fs::write(&p, b"old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        safe_write(&p, b"new", &WriteOpts::secret()).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(mode_of(&p), 0o600);
    }

    #[test]
    fn leaves_no_temp_files_behind() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a");
        safe_write(&p, b"x", &WriteOpts::secret()).unwrap();
        safe_write(&p, b"y", &WriteOpts::secret()).unwrap();
        let names: Vec<_> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("a")]);
    }

    #[test]
    fn rejects_symlinked_parent() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = d.path().join("link");
        symlink(&real, &link).unwrap();
        let err = safe_write(&link.join("f"), b"x", &WriteOpts::secret()).unwrap_err();
        assert!(matches!(err, Error::SymlinkedParent(_)), "{err:?}");
        assert!(!real.join("f").exists());
    }

    #[test]
    fn allows_symlinked_parent_on_the_allowlist_only() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = d.path().join("link");
        symlink(&real, &link).unwrap();
        let opts = WriteOpts::secret().allow_symlinked_parent(&link);
        safe_write(&link.join("f"), b"x", &opts).unwrap();
        assert_eq!(std::fs::read(real.join("f")).unwrap(), b"x");
        let other = d.path().join("other");
        symlink(&real, &other).unwrap();
        assert!(matches!(safe_write(&other.join("g"), b"x", &opts), Err(Error::SymlinkedParent(_))));
    }

    #[test]
    fn a_symlinked_file_is_refused_and_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let managed = d.path().join("managed");
        std::fs::write(&managed, b"dotfiles").unwrap();
        let p = d.path().join(".claude.json");
        symlink(&managed, &p).unwrap();
        let err = safe_write(&p, b"new", &WriteOpts::secret()).unwrap_err();
        assert!(matches!(err, Error::SymlinkedFile(_)), "{err:?}");
        assert!(err.to_string().contains("dotfile manager"), "{err}");
        assert_eq!(std::fs::read(&managed).unwrap(), b"dotfiles");
        assert!(std::fs::symlink_metadata(&p).unwrap().file_type().is_symlink(), "the link is still there");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2, "no temp file is left behind");
    }

    #[test]
    fn replaces_a_symlink_target_instead_of_writing_through_it() {
        let d = tempfile::tempdir().unwrap();
        let victim = d.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let p = d.path().join("auth.json");
        symlink(&victim, &p).unwrap();
        safe_write(&p, b"new", &WriteOpts::secret().replace_symlink()).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert!(!std::fs::symlink_metadata(&p).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
    }

    #[test]
    fn temp_file_creation_is_exclusive_and_refuses_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let dir = rustix::fs::open(d.path(), OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
        create_exclusive(&dir, "t").unwrap();
        assert_eq!(create_exclusive(&dir, "t").unwrap_err(), Errno::EXIST);
        symlink(d.path().join("elsewhere"), d.path().join("s")).unwrap();
        assert!(create_exclusive(&dir, "s").is_err());
        assert!(!d.path().join("elsewhere").exists());
    }

    #[test]
    fn missing_parent_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(safe_write(&d.path().join("nope/f"), b"x", &WriteOpts::secret()).is_err());
    }
}
