//! Copy-on-write clones for Claude Desktop profiles.
//!
//! `cp -c` asks for `clonefile(2)` but silently falls back to a real copy when it cannot clone, so
//! callers refuse a cross-filesystem clone first and read the real cost from the change in free disk
//! space (`du` reports logical size and cannot show a clone's saving).
//!
//! The runtime allowlist is ported from Nemo-Illusionist/claude-code-account-switcher
//! `src/desktop_runtime.rs:20-36` (MIT): the sandbox images and the versioned download folders travel;
//! every per-VM identity file, mutable VM state and the Chromium caches do not.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::proc;

const CP: &str = "/bin/cp";
const DF: &str = "/bin/df";
const TOOL_TIMEOUT: Duration = Duration::from_secs(600);

/// The sandbox bundle, relative to a profile folder.
pub const BUNDLE: &str = "vm_bundles/claudevm.bundle";

/// The images and the `.origin` markers naming the image set they belong to.
const BUNDLE_FILES: &[&str] = &[
    "rootfs.img",
    "vmlinuz",
    "initrd",
    "initrd-micro",
    "initrd-micro.zst",
    ".rootfs.img.origin",
    ".vmlinuz.origin",
    ".initrd.origin",
    ".initrd-micro.origin",
    ".initrd-micro.zst.origin",
];

/// Folders that hold one subfolder per downloaded version. Everything inside travels whole.
const VERSIONED_ROOTS: &[&str] = &["claude-code", "claude-code-vm"];

/// Never cloned, even if a future layout puts them beside the images: this VM's identity (two live VMs
/// sharing a MAC address collide) and its mutable state. Kept as data so a test pins the list.
pub const NEVER_CLONE: &[&str] = &[
    "machineIdentifier",
    "macAddress",
    "gvisorMacAddress",
    "vmIP",
    "sessiondata.img",
    "efivars.fd",
    ".cowork-adopted",
    "warm",
    "Cache",
    "Code Cache",
];

/// Count of non-folder entries and their total logical size. Symlinks count as themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeStats {
    pub files: u64,
    pub bytes: u64,
}

pub fn tree_stats(dir: &Path) -> Result<TreeStats> {
    let mut stats = TreeStats { files: 0, bytes: 0 };
    walk(dir, &mut stats)?;
    Ok(stats)
}

fn walk(dir: &Path, stats: &mut TreeStats) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|e| Error::io(format!("read folder {}", dir.display()), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(format!("read folder {}", dir.display()), e))?;
        let meta = fs::symlink_metadata(entry.path()).map_err(|e| Error::io(format!("stat {}", entry.path().display()), e))?;
        if meta.is_dir() {
            walk(&entry.path(), stats)?;
        } else {
            stats.files += 1;
            stats.bytes += meta.len();
        }
    }
    Ok(())
}

/// Whether `a` and `b` are on one filesystem. The nearest existing ancestor stands in for a path that
/// does not exist yet.
pub fn same_filesystem(a: &Path, b: &Path) -> Result<bool> {
    Ok(device_of(a)? == device_of(b)?)
}

fn device_of(path: &Path) -> Result<u64> {
    let mut current = Some(path);
    while let Some(p) = current {
        if let Ok(meta) = fs::metadata(p) {
            return Ok(meta.dev());
        }
        current = p.parent();
    }
    Err(Error::Invalid(format!("cannot find the disk that holds {}", path.display())))
}

/// Free bytes on the filesystem holding `dir`, from `df -k`. `None` when it cannot be read.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let probe = nearest_existing(dir)?;
    let out = proc::run(Path::new(DF), &["-k", &probe.to_string_lossy()], None, Duration::from_secs(10)).ok()?;
    if out.code != 0 {
        return None;
    }
    parse_df_available(&String::from_utf8_lossy(&out.stdout))
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|p| p.exists()).map(Path::to_path_buf)
}

/// The `Avail` column of `df -k`, in bytes. The device name can hold spaces, so columns are counted from
/// the first numeric field.
pub fn parse_df_available(output: &str) -> Option<u64> {
    let line = output.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let first_number = fields.iter().position(|f| f.parse::<u64>().is_ok())?;
    fields.get(first_number + 2)?.parse::<u64>().ok().map(|kb| kb * 1024)
}

/// Clone `src` (a file or a folder tree) to `dst`, which must not exist. On failure nothing is left at
/// `dst`. Refuses a cross-filesystem clone, which `cp -c` would turn into a full copy.
pub fn clone_path(src: &Path, dst: &Path) -> Result<()> {
    clone_path_with(Path::new(CP), TOOL_TIMEOUT, src, dst)
}

fn clone_path_with(cp: &Path, timeout: Duration, src: &Path, dst: &Path) -> Result<()> {
    if fs::symlink_metadata(dst).is_ok() {
        return Err(Error::Invalid(format!("{} already exists; refusing to clone over it", dst.display())));
    }
    if !same_filesystem(src, dst)? {
        return Err(Error::Invalid(format!(
            "{} and {} are on different disks; a clone would be a full copy. Put them on the same disk",
            src.display(),
            dst.display()
        )));
    }
    let meta = fs::symlink_metadata(src).map_err(|e| Error::io(format!("stat {}", src.display()), e))?;
    let (s, d) = (src.to_string_lossy(), dst.to_string_lossy());
    let args: Vec<&str> = if meta.is_dir() { vec!["-c", "-R", &s, &d] } else { vec!["-c", &s, &d] };
    // A partial clone is worse than none, so `dst` goes on a timeout or a failure to start as well.
    let remove_partial = || {
        let _ = fs::remove_dir_all(dst).or_else(|_| fs::remove_file(dst));
    };
    let out = match proc::run(cp, &args, None, timeout) {
        Ok(out) => out,
        Err(e) => {
            remove_partial();
            return Err(e);
        }
    };
    if out.code != 0 {
        remove_partial();
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Invalid(format!("cloning {} failed: {}", src.display(), why.lines().next().unwrap_or("cp exited with an error").trim())));
    }
    Ok(())
}

/// Clone a whole folder and prove the copy has the same file count and logical size.
pub fn clone_tree_verified(src: &Path, dst: &Path) -> Result<TreeStats> {
    let before = tree_stats(src)?;
    clone_path(src, dst)?;
    let after = match tree_stats(dst) {
        Ok(a) => a,
        Err(e) => {
            let _ = fs::remove_dir_all(dst);
            return Err(e);
        }
    };
    if after != before {
        let _ = fs::remove_dir_all(dst);
        return Err(Error::Invalid(format!(
            "the clone of {} does not match the original ({} files / {} bytes, expected {} files / {} bytes); it was removed and nothing else was changed",
            src.display(),
            after.files,
            after.bytes,
            before.files,
            before.bytes
        )));
    }
    Ok(after)
}

/// One shareable item, relative to the profile folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub rel: PathBuf,
    pub is_dir: bool,
}

/// What may be cloned out of `profile`: the bundle images and every entry under the versioned roots.
/// Nothing in `NEVER_CLONE` is ever returned, whatever sits in the folder.
pub fn runtime_items(profile: &Path) -> Vec<Item> {
    let mut items: Vec<Item> = BUNDLE_FILES
        .iter()
        .filter(|name| !NEVER_CLONE.contains(name))
        .filter_map(|name| {
            let rel = Path::new(BUNDLE).join(name);
            let meta = fs::symlink_metadata(profile.join(&rel)).ok()?;
            meta.is_file().then_some(Item { rel, is_dir: false })
        })
        .collect();
    for root in VERSIONED_ROOTS {
        let Ok(entries) = fs::read_dir(profile.join(root)) else { continue };
        let mut found: Vec<Item> = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                if name == ".DS_Store" || NEVER_CLONE.contains(&name.as_str()) {
                    return None;
                }
                let meta = fs::symlink_metadata(e.path()).ok()?;
                (meta.is_dir() || meta.is_file()).then(|| Item { rel: Path::new(root).join(&name), is_dir: meta.is_dir() })
            })
            .collect();
        found.sort_by(|a, b| a.rel.cmp(&b.rel));
        items.append(&mut found);
    }
    items
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeReport {
    pub cloned: usize,
    pub skipped_existing: usize,
    /// Logical bytes cloned.
    pub logical: u64,
    /// Bytes the disk actually lost, when it could be measured. Close to zero on APFS.
    pub on_disk: Option<u64>,
}

/// Clone the shareable runtime of `src` into `dst`. Items `dst` already has are left alone. Fails when
/// the two folders are on different disks, before anything is copied.
pub fn clone_runtime(src: &Path, dst: &Path) -> Result<RuntimeReport> {
    let items = runtime_items(src);
    let mut report = RuntimeReport { cloned: 0, skipped_existing: 0, logical: 0, on_disk: None };
    if items.is_empty() {
        return Ok(report);
    }
    if !same_filesystem(src, dst)? {
        return Err(Error::Invalid(format!(
            "{} and {} are on different disks; cloning the runtime would copy every byte. Skipping it (the app downloads its own)",
            src.display(),
            dst.display()
        )));
    }
    let before = free_bytes(dst);
    for item in &items {
        let to = dst.join(&item.rel);
        if fs::symlink_metadata(&to).is_ok() {
            report.skipped_existing += 1;
            continue;
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(format!("create folder {}", parent.display()), e))?;
        }
        let from = src.join(&item.rel);
        report.logical += if item.is_dir { tree_stats(&from)?.bytes } else { fs::symlink_metadata(&from).map(|m| m.len()).unwrap_or(0) };
        clone_path(&from, &to)?;
        report.cloned += 1;
    }
    report.on_disk = before.zip(free_bytes(dst)).map(|(b, a)| b.saturating_sub(a));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, bytes: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    /// A profile with one of everything shareable and one of everything that must stay behind.
    fn profile(base: &Path, name: &str) -> PathBuf {
        let p = base.join(name);
        let bundle = p.join(BUNDLE);
        write(&bundle.join("rootfs.img"), &[1; 4096]);
        write(&bundle.join("vmlinuz"), &[2; 64]);
        write(&bundle.join("initrd.zst"), &[9; 8]); // not on the allowlist
        write(&bundle.join(".rootfs.img.origin"), b"set-1");
        for never in ["machineIdentifier", "macAddress", "gvisorMacAddress", "vmIP", "sessiondata.img", "efivars.fd", ".cowork-adopted"] {
            write(&bundle.join(never), b"identity");
        }
        write(&bundle.join("warm/state"), b"warm");
        write(&p.join("claude-code/2.1.246/binary"), &[3; 1024]);
        write(&p.join("claude-code/.DS_Store"), b"junk");
        write(&p.join("claude-code-vm/2.1.215/claude"), &[4; 2048]);
        write(&p.join("claude-code-vm/.sdk-version"), b"2.1.215");
        write(&p.join("Cache/blob"), &[5; 8192]);
        write(&p.join("Code Cache/blob"), &[6; 8192]);
        write(&p.join("Cookies"), b"cookies");
        write(&p.join("config.json"), b"{}");
        p
    }

    fn rels(items: &[Item]) -> Vec<String> {
        items.iter().map(|i| i.rel.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_allowlist_takes_images_and_versioned_folders_only() {
        let d = tempfile::tempdir().unwrap();
        let p = profile(d.path(), "p");
        let got = rels(&runtime_items(&p));
        assert_eq!(
            got,
            vec![
                "vm_bundles/claudevm.bundle/rootfs.img",
                "vm_bundles/claudevm.bundle/vmlinuz",
                "vm_bundles/claudevm.bundle/.rootfs.img.origin",
                "claude-code/2.1.246",
                "claude-code-vm/.sdk-version",
                "claude-code-vm/2.1.215",
            ]
        );
    }

    #[test]
    fn every_never_clone_name_is_excluded_wherever_it_sits() {
        let d = tempfile::tempdir().unwrap();
        let p = profile(d.path(), "p");
        // Even inside a versioned root, a never-clone name is not picked up.
        for never in NEVER_CLONE {
            write(&p.join("claude-code").join(never).join("x"), b"x");
        }
        let got = rels(&runtime_items(&p));
        for never in NEVER_CLONE {
            assert!(!got.iter().any(|r| r.rsplit('/').next() == Some(*never)), "{never} leaked into {got:?}");
        }
    }

    #[test]
    fn cloning_the_runtime_leaves_identity_files_and_caches_behind() {
        let d = tempfile::tempdir().unwrap();
        let src = profile(d.path(), "src");
        let dst = d.path().join("dst");
        fs::create_dir_all(&dst).unwrap();
        let report = clone_runtime(&src, &dst).unwrap();
        assert_eq!((report.cloned, report.skipped_existing), (6, 0));
        assert_eq!(fs::read(dst.join(BUNDLE).join("rootfs.img")).unwrap(), vec![1; 4096]);
        assert_eq!(fs::read(dst.join("claude-code/2.1.246/binary")).unwrap(), vec![3; 1024]);
        for gone in ["machineIdentifier", "macAddress", "gvisorMacAddress", "vmIP", "sessiondata.img", "efivars.fd", ".cowork-adopted", "warm"] {
            assert!(!dst.join(BUNDLE).join(gone).exists(), "{gone} was cloned");
        }
        assert!(!dst.join("Cache").exists() && !dst.join("Code Cache").exists() && !dst.join("Cookies").exists());
        // The source is untouched.
        assert!(src.join(BUNDLE).join("machineIdentifier").exists());
    }

    #[test]
    fn a_second_run_skips_what_is_already_there() {
        let d = tempfile::tempdir().unwrap();
        let src = profile(d.path(), "src");
        let dst = d.path().join("dst");
        fs::create_dir_all(&dst).unwrap();
        clone_runtime(&src, &dst).unwrap();
        let again = clone_runtime(&src, &dst).unwrap();
        assert_eq!((again.cloned, again.skipped_existing), (0, 6));
    }

    #[test]
    fn a_profile_without_runtime_clones_nothing() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        write(&src.join("config.json"), b"{}");
        let dst = d.path().join("dst");
        fs::create_dir_all(&dst).unwrap();
        let r = clone_runtime(&src, &dst).unwrap();
        assert_eq!((r.cloned, r.logical), (0, 0));
    }

    #[test]
    fn clone_path_refuses_an_existing_destination_and_leaves_it_alone() {
        let d = tempfile::tempdir().unwrap();
        let src = profile(d.path(), "src");
        let dst = d.path().join("dst");
        fs::create_dir_all(&dst).unwrap();
        write(&dst.join("keep"), b"mine");
        assert!(clone_path(&src, &dst).is_err());
        assert_eq!(fs::read(dst.join("keep")).unwrap(), b"mine");
    }

    #[test]
    fn a_clone_that_times_out_leaves_nothing_at_the_destination() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let src = profile(d.path(), "src");
        let dst = d.path().join("dst");
        // A stand-in for `cp` that writes part of the destination and then hangs.
        let slow = d.path().join("slow-cp");
        fs::write(&slow, "#!/bin/sh\nmkdir -p \"$4\" && touch \"$4/partial\"\nsleep 30\n").unwrap();
        fs::set_permissions(&slow, fs::Permissions::from_mode(0o755)).unwrap();
        let err = clone_path_with(&slow, Duration::from_millis(300), &src, &dst).unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err:?}");
        assert!(fs::symlink_metadata(&dst).is_err(), "the partial clone was removed");
    }

    #[test]
    fn a_failed_clone_leaves_nothing_at_the_destination() {
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("dst");
        assert!(clone_path(&d.path().join("missing"), &dst).is_err());
        assert!(!dst.exists());
    }

    #[test]
    fn a_verified_tree_clone_matches_in_files_and_size() {
        let d = tempfile::tempdir().unwrap();
        let src = profile(d.path(), "src");
        std::os::unix::fs::symlink("config.json", src.join("link")).unwrap();
        let dst = d.path().join("backup");
        let stats = clone_tree_verified(&src, &dst).unwrap();
        assert_eq!(stats, tree_stats(&src).unwrap());
        assert_eq!(stats.files, tree_stats(&dst).unwrap().files);
        // A symlink stays a symlink.
        assert!(fs::symlink_metadata(dst.join("link")).unwrap().file_type().is_symlink());
    }

    #[test]
    fn the_different_disk_rule_compares_devices() {
        let d = tempfile::tempdir().unwrap();
        assert!(same_filesystem(d.path(), &d.path().join("not/yet/created")).unwrap());
        // `/dev` is its own filesystem on macOS and Linux.
        assert!(!same_filesystem(d.path(), Path::new("/dev")).unwrap());
    }

    #[test]
    fn cross_filesystem_clones_are_refused_before_copying() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        write(&src.join("f"), b"x");
        let err = clone_path(&src, Path::new("/dev/agent-switch-clone-test")).unwrap_err().to_string();
        assert!(err.contains("different disks"), "{err}");
    }

    #[test]
    fn parses_the_free_space_column() {
        let out = "Filesystem 1024-blocks Used Available Capacity iused ifree %iused Mounted on\n/dev/disk3s1s1   971298980  13339060 324941388     4%  484019 3249413880    0%   /\n";
        assert_eq!(parse_df_available(out), Some(324941388 * 1024));
        assert_eq!(parse_df_available("nothing"), None);
        assert!(free_bytes(Path::new("/")).is_some());
    }
}
