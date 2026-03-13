//! squashfs output sink for the OCI layer merge pipeline.
//!
//! Spawns `mksquashfs` as a subprocess and streams the merged tar directly
//! into its stdin via a pipe, producing a squashfs filesystem image without
//! writing any intermediate files to disk.
//!
//! `mksquashfs` ≥ 4.6 is required for correct `-tar` stdin support.

use anyhow::{Context, Result, bail};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
};

use crate::{PackerProgress, image::LayerBlob, overlay::merge_layers_into_streaming};

/// Stream the merged OCI layers into a squashfs image at `output`.
///
/// Convenience wrapper around [`write_squashfs_with_progress`] with no
/// progress channel.
pub fn write_squashfs(
    receiver: mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    output: &Path,
    squashfs_binpath: Option<&Path>,
) -> Result<()> {
    write_squashfs_with_progress(receiver, total_layers, output, squashfs_binpath, None)
}

/// Stream the merged OCI layers into a squashfs image at `output`, emitting
/// progress events on `progress_tx` as each layer is processed by the merge
/// engine.
///
/// `mksquashfs` is spawned immediately and begins consuming data as it
/// arrives. If the merge fails, the partial output file is removed and the
/// merge error is returned in preference to the `mksquashfs` exit status,
/// since the latter is typically just a consequence of the pipe closing
/// unexpectedly.
///
/// Any pre-existing file at `output` is removed before spawning to prevent
/// `mksquashfs` from appending to stale data (`-noappend` also prevents this,
/// but the explicit removal produces a cleaner error if the file is
/// unremovable).
///
/// `progress_tx` uses [`std::sync::mpsc::SyncSender`] so the blocking merge
/// thread never needs to interact with the tokio runtime. Send failures are
/// silently ignored.
pub fn write_squashfs_with_progress(
    receiver: mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    output: &Path,
    squashfs_binpath: Option<&Path>,
    progress_tx: Option<std::sync::mpsc::SyncSender<PackerProgress>>,
) -> Result<()> {
    if output.exists() {
        std::fs::remove_file(output)
            .with_context(|| format!("removing existing {}", output.display()))?;
    }

    let mut child = spawn_mksquashfs(output, squashfs_binpath)?;
    let stdin = child.stdin.take().context("child stdin")?;

    // stdin is moved into merge_layers_into_streaming and dropped when it
    // returns, closing the write end of the pipe. mksquashfs sees EOF and
    // exits cleanly regardless of whether the merge succeeded or failed.
    let merge_result =
        merge_layers_into_streaming(receiver, total_layers, stdin, progress_tx.as_ref());

    // Always wait for mksquashfs to exit — even on merge failure — so we
    // don't leave a zombie process or an open pipe handle behind.
    let exit = child.wait_with_output().context("waiting for mksquashfs")?;

    if merge_result.is_err() {
        let _ = std::fs::remove_file(output);
        return merge_result.context("merging layers into mksquashfs stdin");
    }

    if !exit.status.success() {
        let _ = std::fs::remove_file(output);
        let stderr = String::from_utf8_lossy(&exit.stderr);
        bail!("mksquashfs failed:\n{stderr}");
    }

    Ok(())
}

/// Spawn `mksquashfs` configured to read a tar archive from stdin and write
/// a squashfs image to `output`.
///
/// Compression is zstd at level 2, which provides a good balance of speed
/// and ratio for container image content. Fragments are disabled
/// (`-no-fragments`) to avoid the tail-packing pass that would require
/// buffering all data before writing.
///
/// The `-default-mode`, `-default-uid`, and `-default-gid` flags ensure that
/// implicit root directory entries created by `mksquashfs` for paths with no
/// explicit tar entry get mode `0755` and ownership `0:0`, rather than
/// inheriting the invoking user's identity.
fn spawn_mksquashfs(output: &Path, binpath: Option<&Path>) -> Result<Child> {
    let mut cmd = match binpath {
        Some(p) => Command::new(p),
        None => Command::new("mksquashfs"),
    };
    cmd.args([
        "-",
        output.to_str().context("output path is not UTF-8")?,
        "-tar",
        "-noappend",
        "-no-fragments",
        "-comp",
        "zstd",
        "-Xcompression-level",
        "2",
        "-quiet",
        "-default-mode",
        "0755",
        "-default-uid",
        "0",
        "-default-gid",
        "0",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .context("spawning mksquashfs — is it installed?")
}
