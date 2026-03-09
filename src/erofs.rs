//! erofs output sink for the OCI layer merge pipeline.
//!
//! Spawns `mkfs.erofs` as a subprocess and streams the merged tar directly
//! into its stdin via a pipe, producing an erofs filesystem image without
//! writing any intermediate files to disk.

use anyhow::{Context, Result, bail};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
};

use crate::{PackerProgress, image::LayerBlob, overlay::merge_layers_into_streaming};

/// Stream the merged OCI layers into a erofs image at `output`.
///
/// Convenience wrapper around [`write_erofs_with_progress`] with no
/// progress channel.
pub fn write_erofs(
    receiver: mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    output: &Path,
    mkfs_erofs_binpath: Option<&Path>,
) -> Result<()> {
    write_erofs_with_progress(receiver, total_layers, output, mkfs_erofs_binpath, None)
}

/// Stream the merged OCI layers into a erofs image at `output`, emitting
/// progress events on `progress_tx` as each layer is processed by the merge
/// engine.
///
/// `mkfs.erofs` is spawned immediately and begins consuming data as it
/// arrives. If the merge fails, the partial output file is removed and the
/// merge error is returned in preference to the `mkfs.erofs` exit status,
/// since the latter is typically just a consequence of the pipe closing
/// unexpectedly.
///
/// Any pre-existing file at `output` is removed before spawning so that
/// `mkfs.erofs` writes a fresh image rather than potentially operating on
/// stale data. This also produces a cleaner error if the file is unremovable.
///
/// `progress_tx` uses [`std::sync::mpsc::SyncSender`] so the blocking merge
/// thread never needs to interact with the tokio runtime. Send failures are
/// silently ignored.
pub fn write_erofs_with_progress(
    receiver: mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    output: &Path,
    mkfs_erofs_binpath: Option<&Path>,
    progress_tx: Option<std::sync::mpsc::SyncSender<PackerProgress>>,
) -> Result<()> {
    if output.exists() {
        std::fs::remove_file(output)
            .with_context(|| format!("removing existing {}", output.display()))?;
    }

    let mut child = spawn_mkfs_erofs(output, mkfs_erofs_binpath)?;
    let stdin = child.stdin.take().context("child stdin")?;

    // stdin is moved into merge_layers_into_streaming and dropped when it
    // returns, closing the write end of the pipe. mkfs.erofs sees EOF and
    // exits cleanly regardless of whether the merge succeeded or failed.
    let merge_result =
        merge_layers_into_streaming(receiver, total_layers, stdin, progress_tx.as_ref());

    // Always wait for mkfs.erofs to exit — even on merge failure — so we
    // don't leave a zombie process or an open pipe handle behind.
    let exit = child.wait_with_output().context("waiting for mkfs.erofs")?;

    if merge_result.is_err() {
        let _ = std::fs::remove_file(output);
        let stderr = String::from_utf8_lossy(&exit.stderr);
        if !exit.status.success() && !stderr.is_empty() {
            return merge_result
                .context(format!(
                    "mkfs.erofs failed (status={}):\n{stderr}",
                    exit.status
                ))
                .context("merging layers into mkfs.erofs stdin");
        }
        return merge_result.context("merging layers into mkfs.erofs stdin");
    }

    if !exit.status.success() {
        let _ = std::fs::remove_file(output);
        let stderr = String::from_utf8_lossy(&exit.stderr);
        bail!("mkfs.erofs failed (status={}):\n{stderr}", exit.status);
    }

    Ok(())
}

fn spawn_mkfs_erofs(output: &Path, binpath: Option<&Path>) -> Result<Child> {
    let mut cmd = match binpath {
        Some(p) => Command::new(p),
        None => Command::new("mkfs.erofs"),
    };
    cmd.args([
        "-L",
        "root",
        "--tar=f",
        output.to_str().context("output path is not UTF-8")?,
        "/dev/stdin",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .context("spawning mkfs.erofs — is it installed?")
}
