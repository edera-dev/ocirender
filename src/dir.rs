//! Directory output sink for the OCI layer merge pipeline.
//!
//! Provides [`write_dir_with_progress`], which unpacks the merged tar
//! stream directly into a destination directory using
//! [`tar::Archive::unpack`]. No intermediate tar file is written to disk.
//!
//! Internally, the merge thread and the unpack consumer run concurrently,
//! connected by a [`UnixStream`] pair that acts as an in-process pipe. This
//! allows the merge engine to stream entries into the directory as they are
//! produced rather than waiting for the full merged tar to be assembled first.

use std::{os::unix::net::UnixStream, path::Path, sync::mpsc, thread};

use crate::{
    LayerItem, PackerProgress,
    error::{Error, Result},
    overlay::{MergeError, merge_layers_into_streaming},
};

/// Unpack the merged OCI layers directly into `output_dir`, emitting progress
/// events on `progress_tx` as each layer is processed by the merge engine.
///
/// The merge and unpack steps run concurrently on separate threads, connected
/// by a [`UnixStream`] pair. A failure of the input (a corrupt layer, or the
/// layer source) is reported in preference to the unpack error it causes;
/// otherwise an unpack failure is reported in preference to the broken pipe
/// it causes on the merge side.
///
/// On error, any partially populated content in `output_dir` is left in place.
/// Callers are responsible for cleanup if an incomplete directory is not
/// acceptable.
pub fn write_dir_with_progress(
    receiver: mpsc::Receiver<LayerItem>,
    total_layers: usize,
    output_dir: &Path,
    progress_tx: Option<std::sync::mpsc::SyncSender<PackerProgress>>,
) -> Result<()> {
    std::fs::create_dir_all(output_dir).map_err(|e| Error::output(output_dir, e))?;

    let (reader, writer) = UnixStream::pair().map_err(|e| Error::output(output_dir, e))?;

    let merge_handle = thread::spawn(move || {
        merge_layers_into_streaming(receiver, total_layers, writer, progress_tx.as_ref())
    });

    // The IIFE captures the unpack result without an early return, ensuring
    // merge_handle.join() is always called regardless of whether unpack fails.
    let unpack_result = {
        let mut archive = tar::Archive::new(reader);
        archive.set_preserve_permissions(true);
        archive.set_preserve_mtime(true);
        archive.unpack(output_dir)
    };

    let merge_result = merge_handle.join().expect("merge thread panicked");

    match (merge_result, unpack_result) {
        (Err(MergeError::Input(e)), _) => Err(e),
        (_, Err(e)) | (Err(MergeError::Output(e)), Ok(())) => Err(Error::output(output_dir, e)),
        (Ok(()), Ok(())) => Ok(()),
    }
}
