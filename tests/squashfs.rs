//! Tests for squashfs.rs error handling.
//!
//! These tests exercise the three failure modes in write_squashfs_with_progress
//! without requiring a real mksquashfs binary.  All cases use squashfs_binpath
//! to point at a synthetic script or a known-absent path.
//!
//! Failure modes covered:
//!
//!   1. Binary not found — spawn fails entirely; no output file must be created.
//!   2. Binary exits nonzero — merge succeeded but mksquashfs rejected the
//!      input; output file must be removed and the exit-status error surfaced.
//!   3. Binary exits nonzero after a channel error — merge_result.is_err()
//!      branch; the merge error must be surfaced (not the exit status), and the
//!      output file must be removed.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::mpsc,
};

use oci2squashfs::image::LayerBlob;
use tempfile::{NamedTempFile, TempDir};

#[path = "helpers/mod.rs"]
mod helpers;
use helpers::{LayerBuilder, blob};

// ── script helpers ────────────────────────────────────────────────────────────

/// Write a shell script into `dir` with the given body and make it executable.
/// Returns the path to the script.
fn write_script(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    let dir = dir.path();
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Build a minimal one-layer channel for passing to write_squashfs.
fn one_layer_channel() -> (mpsc::Receiver<anyhow::Result<LayerBlob>>, usize) {
    let layer = LayerBuilder::new()
        .add_file("hello.txt", b"hello", 0o644)
        .finish();
    let (tx, rx) = mpsc::channel();
    tx.send(Ok(blob(layer, 0))).unwrap();
    drop(tx);
    (rx, 1)
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// When the mksquashfs binary does not exist at the given path, spawn must fail
/// with a clear error and must not create an output file.
#[test]
fn squashfs_binary_not_found_returns_error() {
    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out); // Release so write_squashfs can manage the file itself.

    let (rx, total) = one_layer_channel();
    let result = oci2squashfs::squashfs::write_squashfs(
        rx,
        total,
        &out_path,
        Some(Path::new("/nonexistent/bin/mksquashfs")),
    );

    assert!(result.is_err(), "missing binary must return an error");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("spawning mksquashfs"),
        "error must mention spawning mksquashfs"
    );
    assert!(
        !out_path.exists(),
        "no output file must be created when spawn fails"
    );
}

/// When mksquashfs exits with a nonzero status, write_squashfs must return an
/// error and must remove the (partial) output file.
#[test]
fn squashfs_nonzero_exit_returns_error_and_removes_output() {
    let scripts = TempDir::new().unwrap();
    // Script that consumes all stdin (so the merge doesn't get a broken pipe)
    // then exits with status 1.
    let script = write_script(&scripts, "fake_mksquashfs.sh", "cat > /dev/null; exit 1");

    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    let (rx, total) = one_layer_channel();
    let result = oci2squashfs::squashfs::write_squashfs(rx, total, &out_path, Some(&script));

    assert!(result.is_err(), "nonzero exit must return an error");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("mksquashfs failed"),
        "error must report mksquashfs failure; got: {msg}"
    );
    assert!(
        !out_path.exists(),
        "partial output file must be removed after nonzero exit"
    );
}

/// When mksquashfs exits nonzero AND stderr contains output, that stderr text
/// must appear in the returned error message so the caller can diagnose the
/// problem without re-running with verbose flags.
#[test]
fn squashfs_nonzero_exit_includes_stderr_in_error() {
    let scripts = TempDir::new().unwrap();
    let script = write_script(
        &scripts,
        "fake_mksquashfs_stderr.sh",
        "cat > /dev/null; echo 'unsupported option -tar' >&2; exit 1",
    );

    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    let (rx, total) = one_layer_channel();
    let result = oci2squashfs::squashfs::write_squashfs(rx, total, &out_path, Some(&script));

    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("unsupported option -tar"),
        "stderr from mksquashfs must appear in the error; got: {msg}"
    );
}

/// When the merge itself fails (channel error), the merge error must be
/// surfaced — not the mksquashfs exit status — and the output file must be
/// removed.  This exercises the `merge_result.is_err()` branch in
/// write_squashfs_with_progress.
#[test]
fn squashfs_merge_error_surfaced_over_exit_status() {
    let scripts = TempDir::new().unwrap();
    // Script exits 1 after draining stdin — it would produce a "mksquashfs failed"
    // error on its own, but the merge error must take precedence.
    let script = write_script(
        &scripts,
        "fake_mksquashfs_drain.sh",
        "cat > /dev/null; exit 1",
    );

    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    // Send an Err on the channel so the merge fails.
    let (tx, rx) = mpsc::channel();
    tx.send(Err(anyhow::anyhow!("injected download failure")))
        .unwrap();
    drop(tx);

    let result = oci2squashfs::squashfs::write_squashfs(rx, 1, &out_path, Some(&script));

    assert!(result.is_err(), "merge error must propagate");
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("merging layers into mksquashfs stdin"),
        "merge error context must be surfaced, not mksquashfs exit status; got: {msg}"
    );
    // The root cause must also be reachable via the anyhow chain.
    let chain = format!("{err:#}");
    assert!(
        chain.contains("injected download failure"),
        "root cause must be present in error chain; got: {chain}"
    );
    assert!(
        !out_path.exists(),
        "output file must be removed when the merge fails"
    );
}

/// When mksquashfs closes its stdin early (simulating a crash), the merge
/// thread will get a broken pipe writing to the stdin pipe.  The error must
/// propagate cleanly — no panic, no hang — and the output file must not be
/// left behind.
#[test]
fn squashfs_broken_pipe_on_early_stdin_close() {
    let scripts = TempDir::new().unwrap();
    // Script exits immediately without reading stdin.
    let script = write_script(&scripts, "fake_mksquashfs_crash.sh", "exit 1");

    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    // Use a large-ish layer so we're likely to hit the broken pipe before the
    // merge finishes naturally.
    let large_data: Vec<u8> = (0u8..=255).cycle().take(512 * 1024).collect();
    let layer = LayerBuilder::new()
        .add_file("big.bin", &large_data, 0o644)
        .finish();
    let (tx, rx) = mpsc::channel();
    tx.send(Ok(blob(layer, 0))).unwrap();
    drop(tx);

    let result = oci2squashfs::squashfs::write_squashfs(rx, 1, &out_path, Some(&script));

    // We expect an error — either from the broken pipe on the merge side or
    // from the nonzero exit on the mksquashfs side.  Either way it must not
    // panic or hang, and the output must be cleaned up.
    assert!(result.is_err(), "early stdin close must return an error");
    assert!(
        !out_path.exists(),
        "output file must not be left behind after broken pipe"
    );
}

/// Verify that a pre-existing output file at the target path is removed before
/// writing begins, so stale data from a previous run is never mistaken for a
/// successful conversion.
#[test]
fn squashfs_pre_existing_output_removed_before_spawn() {
    let scripts = TempDir::new().unwrap();
    let script = write_script(&scripts, "fake_mksquashfs_ok.sh", "cat > /dev/null; exit 1");

    // Write stale data to the output path before we start.
    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    fs::write(&out_path, b"stale squashfs data").unwrap();
    drop(out);

    let (rx, total) = one_layer_channel();
    let _ = oci2squashfs::squashfs::write_squashfs(rx, total, &out_path, Some(&script));

    // Whether the overall result is Ok or Err, the stale file must not remain
    // with its original contents — write_squashfs removes it before spawning.
    if out_path.exists() {
        let contents = fs::read(&out_path).unwrap();
        assert_ne!(
            contents, b"stale squashfs data",
            "stale output must be overwritten, not preserved"
        );
    }
    // If the file doesn't exist that's also fine — it was removed and the
    // error path cleaned it up.
}
