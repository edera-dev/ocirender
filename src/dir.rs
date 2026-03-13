//! Directory output sink for the OCI layer merge pipeline.
//!
//! Provides [`write_dir`] and [`write_dir_with_progress`], which unpack the
//! merged tar stream directly into a destination directory using
//! [`tar::Archive::unpack`]. No intermediate tar file is written to disk.
//!
//! Internally, the merge thread and the unpack consumer run concurrently,
//! connected by a [`UnixStream`] pair that acts as an in-process pipe. This
//! allows the merge engine to stream entries into the directory as they are
//! produced rather than waiting for the full merged tar to be assembled first.

use anyhow::{Context, Result};
use std::{os::unix::net::UnixStream, path::Path, sync::mpsc, thread};

use crate::{PackerProgress, image::LayerBlob, overlay::merge_layers_into_streaming};

/// Unpack the merged OCI layers directly into `output_dir`, emitting progress
/// events on `progress_tx` as each layer is processed by the merge engine.
///
/// The merge and unpack steps run concurrently on separate threads, connected
/// by a [`UnixStream`] pair. If both threads fail, the merge error is returned
/// in preference to the unpack error, since a merge failure (e.g. a corrupt
/// layer blob) is more likely to be the root cause of a broken pipe on the
/// unpack side.
///
/// On error, any partially populated content in `output_dir` is left in place.
/// Callers are responsible for cleanup if an incomplete directory is not
/// acceptable.
pub fn write_dir_with_progress(
    receiver: mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    output_dir: &Path,
    progress_tx: Option<std::sync::mpsc::SyncSender<PackerProgress>>,
) -> Result<()> {
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("creating output directory {}", output_dir.display()))?;

    let (reader, writer) = UnixStream::pair().context("creating socket pair for tar pipe")?;

    let merge_handle = thread::spawn(move || {
        merge_layers_into_streaming(receiver, total_layers, writer, progress_tx.as_ref())
    });

    // The IIFE captures the unpack result without an early return, ensuring
    // merge_handle.join() is always called regardless of whether unpack fails.
    let unpack_result = {
        let mut archive = tar::Archive::new(reader);
        archive.set_preserve_permissions(true);
        archive.set_preserve_mtime(true);
        archive
            .unpack(output_dir)
            .context("unpacking merged tar into output directory")
    };

    let merge_result = merge_handle.join().expect("merge thread panicked");

    // Prefer the merge error if both fail: it's more likely to be the root
    // cause (e.g. a corrupt layer blob) rather than a downstream consequence
    // of the pipe closing unexpectedly.
    merge_result.and(unpack_result)
}

/// Unpack the merged OCI layers directly into `output_dir`.
///
/// Convenience wrapper around [`write_dir_with_progress`] with no progress
/// channel. On error, any partially populated content in `output_dir` is left
/// in place — callers are responsible for cleanup.
pub fn write_dir(
    receiver: mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    output_dir: &Path,
) -> Result<()> {
    write_dir_with_progress(receiver, total_layers, output_dir, None)
}
