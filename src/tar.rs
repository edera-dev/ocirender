//! Tar file output sink for the OCI layer merge pipeline.
//!
//! Provides [`write_tar_with_progress`], which writes the merged tar stream
//! directly to a file. This is the simplest output sink:
//! unlike the squashfs sink there is no subprocess, and unlike the directory
//! sink there is no concurrent consumer — the merge engine writes directly to
//! the output file handle.

use std::{path::Path, sync::mpsc};

use crate::{
    LayerItem, PackerProgress,
    error::{Error, Result},
    overlay::merge_layers_into_streaming,
};

/// Stream the merged OCI layers into a plain tar file at `output`, emitting
/// progress events on `progress_tx` as each layer is processed.
///
/// On error the partially written output file is removed before returning, so
/// callers never observe a truncated tar.
pub fn write_tar_with_progress(
    receiver: mpsc::Receiver<LayerItem>,
    total_layers: usize,
    output: &Path,
    progress_tx: Option<std::sync::mpsc::SyncSender<PackerProgress>>,
) -> Result<()> {
    let file = std::fs::File::create(output).map_err(|e| Error::output(output, e))?;

    let result = merge_layers_into_streaming(receiver, total_layers, file, progress_tx.as_ref())
        .map_err(|e| e.into_error(output));

    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }

    result
}
