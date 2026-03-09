//! Verification: diff a generated image against a reference directory.
//!
//! The public entry point is [`verify`], which accepts an [`ImageSpec`]
//! describing the generated image and a path to a reference directory. For
//! squashfs and erofs images the filesystem is mounted read-only (via
//! `squashfuse` or `erofsfuse` respectively) for the duration of the
//! comparison and unmounted on return. For directory images the comparison
//! is performed directly.
//!
//! Comparison is recursive and checks file type, permissions, ownership,
//! size, symlink target, and SHA-256 content hash. Results are collected into
//! a [`VerifyReport`] rather than failing on the first difference, so a
//! single run surfaces all discrepancies.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

use crate::ImageSpec;

// ─── RAII mount guard ─────────────────────────────────────────────────────────

/// RAII guard that mounts a squashfs image via `squashfuse` on construction
/// and unmounts it on drop.
///
/// The [`TempDir`] holding the mountpoint is kept alive for the lifetime of
/// this guard. Rust drops fields in declaration order, so `mountpoint` is
/// dropped after the `Drop` impl runs and the filesystem is unmounted —
/// ensuring the directory is not deleted while still in use.
struct SquashMount {
    mountpoint: TempDir,
}

impl SquashMount {
    /// Mount `squashfs` at a freshly created temporary directory.
    ///
    /// Returns an error if `squashfuse` is not installed or if the mount
    /// fails.
    fn new(squashfs: &Path) -> Result<Self> {
        let mountpoint = TempDir::new().context("creating temp mount dir")?;
        let status = Command::new("squashfuse")
            .arg(squashfs)
            .arg(mountpoint.path())
            .status()
            .context("spawning squashfuse — is it installed?")?;
        if !status.success() {
            anyhow::bail!("squashfuse failed with status {status}");
        }
        Ok(Self { mountpoint })
    }

    fn path(&self) -> &Path {
        self.mountpoint.path()
    }
}

impl Drop for SquashMount {
    fn drop(&mut self) {
        // Try fusermount first (Linux FUSE), fall back to umount (macOS/BSD).
        // Errors are ignored: if unmounting fails here there is nothing useful
        // to do, and panicking in Drop would abort the process.
        let ok = Command::new("fusermount")
            .args(["-u", self.mountpoint.path().to_str().unwrap_or("")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !ok {
            let _ = Command::new("umount").arg(self.mountpoint.path()).status();
        }
        // TempDir::drop runs after this, removing the now-unmounted directory.
    }
}

/// RAII guard that mounts a erofs image via `erofsfuse` on construction
/// and unmounts it on drop.
///
/// The [`TempDir`] holding the mountpoint is kept alive for the lifetime of
/// this guard. Rust drops fields in declaration order, so `mountpoint` is
/// dropped after the `Drop` impl runs and the filesystem is unmounted —
/// ensuring the directory is not deleted while still in use.
struct ErofsMount {
    mountpoint: TempDir,
}

impl ErofsMount {
    /// Mount `erofs` at a freshly created temporary directory.
    ///
    /// Returns an error if `erofsfuse` is not installed or if the mount
    /// fails.
    fn new(erofs: &Path) -> Result<Self> {
        let mountpoint = TempDir::new().context("creating temp mount dir")?;
        let status = Command::new("erofsfuse")
            .arg(erofs)
            .arg(mountpoint.path())
            .status()
            .context("spawning erofsfuse — is it installed?")?;
        if !status.success() {
            anyhow::bail!("erofsfuse failed with status {status}");
        }
        Ok(Self { mountpoint })
    }

    fn path(&self) -> &Path {
        self.mountpoint.path()
    }
}

impl Drop for ErofsMount {
    fn drop(&mut self) {
        // Try fusermount first (Linux FUSE), fall back to umount (macOS/BSD).
        // Errors are ignored: if unmounting fails here there is nothing useful
        // to do, and panicking in Drop would abort the process.
        let ok = Command::new("fusermount")
            .args(["-u", self.mountpoint.path().to_str().unwrap_or("")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !ok {
            let _ = Command::new("umount").arg(self.mountpoint.path()).status();
        }
        // TempDir::drop runs after this, removing the now-unmounted directory.
    }
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// The result of comparing a generated image against a reference directory.
#[derive(Debug)]
pub struct VerifyReport {
    /// Paths present in the generated image but absent from the reference.
    pub only_in_generated: Vec<PathBuf>,
    /// Paths present in the reference but absent from the generated image.
    pub only_in_reference: Vec<PathBuf>,
    /// Per-file attribute or content differences found in both trees.
    pub differences: Vec<FileDiff>,
}

impl VerifyReport {
    /// Return `true` if the generated image and reference are identical.
    pub fn is_clean(&self) -> bool {
        self.only_in_generated.is_empty()
            && self.only_in_reference.is_empty()
            && self.differences.is_empty()
    }
}

/// A single per-file difference between the generated image and the reference.
#[derive(Debug)]
pub struct FileDiff {
    /// Relative path of the differing entry.
    pub path: PathBuf,
    /// Human-readable description of the difference, e.g.
    /// `"mode: generated=0755 reference=0644"`.
    pub detail: String,
}

/// Compare a generated image against a reference directory and return a
/// [`VerifyReport`].
///
/// - [`ImageSpec::Squashfs`]: mounts the squashfs read-only via `squashfuse`,
///   diffs the mount against `reference`, then unmounts. Requires `squashfuse`
///   and `fusermount` (Linux) or `umount` (macOS/BSD) to be installed.
/// - [`ImageSpec::Erofs`]: mounts the erofs image read-only via `erofsfuse`,
///   diffs the mount against `reference`, then unmounts. Requires `erofsfuse`
///   and `fusermount` (Linux) or `umount` (macOS/BSD) to be installed.
/// - [`ImageSpec::Dir`]: diffs the directory directly against `reference`.
///   No external tools required.
/// - [`ImageSpec::Tar`]: returns `Err`. Tar archives cannot be compared
///   directly — extract to a directory with `convert-dir` first, then verify
///   with [`ImageSpec::Dir`].
///
/// If `ignore_ownership` is `true`, uid and gid differences are not recorded.
/// This is appropriate when comparing a squashfs image (which preserves
/// ownership from tar headers) against a directory extracted as a non-root
/// user (where `chown` silently fails, leaving all files owned by the
/// invoking user).
pub fn verify(spec: ImageSpec, reference: &Path, ignore_ownership: bool) -> Result<VerifyReport> {
    match spec {
        ImageSpec::Squashfs { path, .. } => {
            let mount = SquashMount::new(&path)?;
            verify_dirs(mount.path(), reference, ignore_ownership)
        }
        ImageSpec::Erofs { path, .. } => {
            let mount = ErofsMount::new(&path)?;
            verify_dirs(mount.path(), reference, ignore_ownership)
        }
        ImageSpec::Dir { path } => verify_dirs(&path, reference, ignore_ownership),
        ImageSpec::Tar { .. } => anyhow::bail!(
            "tar verification is not supported directly; \
             extract to a directory with convert-dir first, then use --dir"
        ),
    }
}

/// Compare two directory trees and return a [`VerifyReport`].
///
/// This is the core comparison primitive. [`verify`] calls this after
/// resolving the generated image to a directory (mounting it if necessary).
/// All differences are collected before returning; the function does not
/// short-circuit on the first mismatch.
///
/// If `ignore_ownership` is `true`, uid and gid differences are not recorded.
pub(crate) fn verify_dirs(
    generated: &Path,
    reference: &Path,
    ignore_ownership: bool,
) -> Result<VerifyReport> {
    let generated_tree = walk_tree(generated).context("walking generated directory")?;
    let reference_tree = walk_tree(reference).context("walking reference directory")?;

    let mut report = VerifyReport {
        only_in_generated: Vec::new(),
        only_in_reference: Vec::new(),
        differences: Vec::new(),
    };

    for (rel, gen_info) in &generated_tree {
        match reference_tree.get(rel) {
            None => report.only_in_generated.push(rel.clone()),
            Some(ref_info) => report.differences.extend(compare_entries(
                rel,
                gen_info,
                ref_info,
                ignore_ownership,
            )),
        }
    }
    for rel in reference_tree.keys() {
        if !generated_tree.contains_key(rel) {
            report.only_in_reference.push(rel.clone());
        }
    }

    Ok(report)
}

// ─── Tree walking ─────────────────────────────────────────────────────────────

/// Per-entry metadata collected during a directory walk.
#[derive(Debug)]
struct EntryInfo {
    kind: EntryKind,
    /// Permission bits only (masked to `0o7777`). File type bits are captured
    /// separately in `kind` to keep comparisons straightforward.
    mode: u32,
    uid: u32,
    gid: u32,
    /// Byte size of the file. Only meaningful for regular files; ignored for
    /// directories and symlinks.
    size: u64,
    /// Symlink target path, if this entry is a symlink.
    symlink_target: Option<PathBuf>,
    /// Hex-encoded SHA-256 of the file content, if this entry is a regular
    /// file.
    sha256: Option<String>,
}

#[derive(Debug, PartialEq)]
enum EntryKind {
    File,
    Dir,
    Symlink,
    /// Device nodes, named pipes, sockets, and other non-regular entry types.
    Other,
}

/// Recursively walk `root` and return a map of relative path → [`EntryInfo`].
fn walk_tree(root: &Path) -> Result<HashMap<PathBuf, EntryInfo>> {
    let mut map = HashMap::new();
    walk_dir(root, root, &mut map)?;
    Ok(map)
}

fn walk_dir(root: &Path, current: &Path, map: &mut HashMap<PathBuf, EntryInfo>) -> Result<()> {
    for entry in
        fs::read_dir(current).with_context(|| format!("reading dir {}", current.display()))?
    {
        let entry = entry?;
        let abs = entry.path();
        let rel = abs
            .strip_prefix(root)
            .context("strip prefix")?
            .to_path_buf();

        // Use symlink_metadata so that symlink entries are recorded as
        // symlinks rather than being followed to their targets.
        let meta = fs::symlink_metadata(&abs)
            .with_context(|| format!("metadata for {}", abs.display()))?;
        let ft = meta.file_type();

        let (kind, symlink_target, sha256) = if ft.is_symlink() {
            (EntryKind::Symlink, Some(fs::read_link(&abs)?), None)
        } else if ft.is_file() {
            (EntryKind::File, None, Some(hash_file(&abs)?))
        } else if ft.is_dir() {
            (EntryKind::Dir, None, None)
        } else {
            (EntryKind::Other, None, None)
        };

        // Mask to permission bits only; file type bits are in `kind`.
        const PERMISSION_BITS: u32 = 0o7777;

        map.insert(
            rel,
            EntryInfo {
                kind,
                mode: meta.permissions().mode() & PERMISSION_BITS,
                uid: meta.uid(),
                gid: meta.gid(),
                size: meta.len(),
                symlink_target,
                sha256,
            },
        );

        if ft.is_dir() {
            walk_dir(root, &abs, map)?;
        }
    }
    Ok(())
}

/// Compute the SHA-256 hash of a file's contents, returned as a lowercase hex
/// string.
fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ─── Comparison ───────────────────────────────────────────────────────────────

/// Compare two entries at `rel` and return any differences as [`FileDiff`]
/// values.
///
/// If the entry types differ, a single type-mismatch diff is returned and
/// further attribute comparison is skipped — mode, uid, etc. are meaningless
/// across a type boundary.
fn compare_entries(
    rel: &Path,
    generated: &EntryInfo,
    reference: &EntryInfo,
    ignore_ownership: bool,
) -> Vec<FileDiff> {
    let mut diffs = Vec::new();
    macro_rules! diff {
        ($msg:expr) => {
            diffs.push(FileDiff {
                path: rel.to_path_buf(),
                detail: $msg,
            })
        };
    }

    if generated.kind != reference.kind {
        diff!(format!(
            "type mismatch: generated={:?} reference={:?}",
            generated.kind, reference.kind
        ));
        return diffs;
    }
    if generated.mode != reference.mode {
        diff!(format!(
            "mode: generated={:04o} reference={:04o}",
            generated.mode, reference.mode
        ));
    }
    if !ignore_ownership {
        if generated.uid != reference.uid {
            diff!(format!(
                "uid: generated={} reference={}",
                generated.uid, reference.uid
            ));
        }
        if generated.gid != reference.gid {
            diff!(format!(
                "gid: generated={} reference={}",
                generated.gid, reference.gid
            ));
        }
    }
    if generated.symlink_target != reference.symlink_target {
        diff!(format!(
            "symlink target: generated={:?} reference={:?}",
            generated.symlink_target, reference.symlink_target
        ));
    }
    if generated.kind == EntryKind::File {
        if generated.size != reference.size {
            diff!(format!(
                "size: generated={} reference={}",
                generated.size, reference.size
            ));
        }
        if generated.sha256 != reference.sha256 {
            diff!("sha256 mismatch".into());
        }
    }
    diffs
}
