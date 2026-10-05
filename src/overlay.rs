//! Core OCI layer merge algorithm.
//!
//! The two public entry points are [`merge_layers_into`] (batch, takes a
//! pre-sorted `Vec<LayerBlob>`) and [`merge_layers_into_streaming`] (accepts
//! layers via a channel in any arrival order). Both produce an identical tar
//! stream; the batch variant is primarily useful in tests.
//!
//! ## Algorithm overview
//!
//! Layers are processed **newest-first**. On the first encounter of any path,
//! that version wins and is written to the output. Subsequent encounters of the
//! same path in older layers are skipped. This makes the "newest wins" rule
//! fall out naturally from iteration order rather than requiring explicit
//! overwrite logic.
//!
//! Three tracker data structures maintain the necessary state across layers;
//! see [`crate::tracker`] for details.
//!
//! Hard links are a special case: a hardlink's target may live in an older
//! layer that hasn't been processed yet, so they are deferred and replayed
//! after all layers are complete. If a target was suppressed by a whiteout,
//! surviving hardlinks to it are *promoted* to standalone regular files.
//! See `emit_deferred` for the full promotion logic.
//!
//! ## Streaming resequencing
//!
//! [`merge_layers_into_streaming`] accepts layers in any order but must
//! process them newest-first. It maintains a resequencing buffer (a
//! `HashMap<index, LayerBlob>`) and a `next_index` cursor that counts down
//! from `total_layers - 1` to `0`. Each time a blob arrives, it is inserted
//! into the buffer; then the cursor is used to drain any contiguous
//! descending run that is now ready to process. This means a single
//! out-of-order arrival can unblock multiple waiting layers at once.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use tar::{Builder, EntryType};

use crate::{
    LayerItem, PackerProgress,
    canonical::CanonicalTarHeader,
    error::Error,
    image::LayerBlob,
    layers::open_layer,
    sparse::PaxSparse,
    tracker::{EmittedPathTracker, HardLinkTracker, WhiteoutTracker},
};

/// Why a merge stopped.
#[derive(Debug)]
pub enum MergeError {
    /// The input failed: a layer could not be read, or the layer source
    /// failed or ran out. This is the error to report.
    Input(Error),
    /// Writing the merged stream to the sink failed. What that means is up to
    /// the sink: for mksquashfs, a broken pipe usually means it exited first.
    Output(io::Error),
}

impl From<Error> for MergeError {
    fn from(e: Error) -> Self {
        Self::Input(e)
    }
}

impl MergeError {
    /// The error to report, for a sink whose only failure mode is writing to
    /// `output`.
    pub fn into_error(self, output: &Path) -> Error {
        match self {
            Self::Input(e) => e,
            Self::Output(e) => Error::output(output, e),
        }
    }
}

fn layer_error(blob: &LayerBlob, message: impl Into<String>, e: io::Error) -> MergeError {
    MergeError::Input(Error::layer(blob, message, e))
}

/// A writer that records whether it has failed.
///
/// Writing an entry copies its data from the layer to the sink, and
/// `tar::Builder` reports a failure on either side, or an entry it cannot
/// encode at all, as the same kind of `io::Error`. Wrapping the sink tells
/// them apart: only an error the sink itself returned is an output failure,
/// and anything else is the layer's.
pub struct TrackedWriter<W> {
    inner: W,
    failed: bool,
}

impl<W> TrackedWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            failed: false,
        }
    }

    fn note<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &result
            && e.kind() != io::ErrorKind::Interrupted
        {
            self.failed = true;
        }
        result
    }
}

impl<W: Write> Write for TrackedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result = self.inner.write(buf);
        self.note(result)
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.inner.flush();
        self.note(result)
    }
}

/// Process a single layer blob: record whiteouts, defer hardlinks, and stream
/// all other non-suppressed, non-duplicate entries into `output`.
///
/// Suppressed regular files are buffered into the `HardLinkTracker` rather
/// than being dropped immediately, because a surviving hardlink in the same or
/// an older layer may need their content for promotion.
fn process_layer<W: Write>(
    blob: &LayerBlob,
    whiteout: &mut WhiteoutTracker,
    emitted: &mut EmittedPathTracker,
    hardlinks: &mut HardLinkTracker,
    output: &mut Builder<TrackedWriter<W>>,
) -> Result<(), MergeError> {
    let mut archive = open_layer(&blob.path, &blob.media_type)
        .map_err(|e| layer_error(blob, "cannot open layer", e))?;

    let entries = archive
        .entries()
        .map_err(|e| layer_error(blob, "reading tar entries", e))?;
    for entry_result in entries {
        let mut entry = entry_result.map_err(|e| layer_error(blob, "reading tar entry", e))?;
        let mut canonical = CanonicalTarHeader::from_entry(&mut entry)
            .map_err(|e| layer_error(blob, "reading entry header", e))?;

        // A PAX-format sparse file stands for a plain regular file: handle it
        // as one, under its real path and with its holes filled back in.
        let pax_sparse = PaxSparse::detect(&canonical.pax_extensions)
            .map_err(|e| layer_error(blob, "reading PAX sparse records", e))?;
        let raw_path = match pax_sparse.as_ref().and_then(PaxSparse::name) {
            Some(name) => name.to_path_buf(),
            None => entry
                .path()
                .map_err(|e| layer_error(blob, "reading entry path", e))?
                .into_owned(),
        };
        let path = normalize_path(&raw_path);
        let mut data: Box<dyn Read + '_> = match pax_sparse {
            Some(sparse) => {
                sparse.rewrite_header(&mut canonical);
                let data = sparse.expand(&mut entry).map_err(|e| {
                    layer_error(blob, format!("expanding sparse file {}", path.display()), e)
                })?;
                Box::new(data)
            }
            None => Box::new(&mut entry),
        };

        // Skip the root directory entry (`./`, `/`, or `.`), which normalises
        // to an empty path or `.` and is meaningless in a merged tar.
        if path.as_os_str().is_empty() || path == Path::new(".") {
            continue;
        }

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Whiteout entries set suppression rules for older layers and are
        // never emitted themselves.
        if file_name == ".wh..wh..opq" {
            let parent = path.parent().unwrap_or(Path::new(""));
            whiteout.insert_opaque(parent, blob.index);
            continue;
        }
        if let Some(real_name) = file_name.strip_prefix(".wh.") {
            let parent = path.parent().unwrap_or(Path::new(""));
            whiteout.insert_simple(&parent.join(real_name), blob.index);
            continue;
        }

        if whiteout.is_suppressed(&path, blob.index) {
            // Buffer regular file content even for suppressed entries: a
            // hardlink in the same or an older layer may be alive and need
            // these bytes for promotion to a standalone file. Sparse files are
            // captured as regular ones by now, so they are buffered too.
            if matches!(
                canonical.entry_type(),
                EntryType::Regular | EntryType::Continuous
            ) {
                let mut buffered = Vec::new();
                data.read_to_end(&mut buffered)
                    .map_err(|e| layer_error(blob, format!("reading {}", path.display()), e))?;
                hardlinks.note_suppressed_file(path, canonical, buffered);
            }
            // Directories, symlinks, and hardlinks pointing at suppressed
            // paths are dropped without buffering.
            continue;
        }

        // Newest version already emitted — skip older duplicate.
        if emitted.contains(&path) {
            continue;
        }

        if canonical.entry_type() == EntryType::Link {
            let link_target = canonical
                .link_name()
                .map_err(|e| layer_error(blob, "reading hard link target", e))?
                .ok_or_else(|| {
                    layer_error(
                        blob,
                        format!("hard link {} has no target", path.display()),
                        io::Error::from(io::ErrorKind::InvalidData),
                    )
                })?;
            let target_path = normalize_path(&link_target);

            // The link path will ultimately surface in the output as a
            // non-directory (either the hardlink itself or, via promotion, a
            // standalone regular file). Shadow it so older-layer entries
            // beneath this path are suppressed — see non-directory shadow
            // note below.
            whiteout.insert_opaque(&path, blob.index);

            if whiteout.is_suppressed(&target_path, blob.index) {
                // Target was whited out but this link path is alive. Record a
                // promotion: at emit time, link_path will be written as a
                // standalone regular file using the suppressed target's content.
                hardlinks.record_promotion(path, target_path, blob.index);
            } else {
                // Target is live. Defer the hardlink for emission after all
                // layers are processed, once we can confirm the target was
                // actually written.
                hardlinks.record(path, target_path, blob.index, canonical);
            }
            continue;
        }

        if let Err(e) = canonical.write_to_tar(&path, &mut data, output) {
            return Err(if output.get_ref().failed {
                MergeError::Output(e)
            } else {
                layer_error(blob, format!("cannot emit {}", path.display()), e)
            });
        }
        emitted.insert(&path);

        // When a non-directory wins at path P, older-layer entries under
        // P/... must be implicitly suppressed: OCI layers do not require an
        // explicit whiteout to replace a directory tree with a non-directory,
        // and real images (e.g. gitlab-toolbox's `srv/gitlab/log` symlink
        // covering the prior directory) rely on this.
        if canonical.entry_type() != EntryType::Directory {
            whiteout.insert_opaque(&path, blob.index);
        }
    }

    // Discard the per-layer suppressed-file buffer. It exists only to serve
    // same-layer hardlink promotions; once process_layer returns, no
    // subsequent layer can contain a hardlink to a file that only exists in
    // the layer just processed (tar requires regular files to precede their
    // hardlinks within an archive, and older layers cannot reference paths
    // that only exist in newer ones).
    hardlinks.end_layer();

    Ok(())
}

/// Emit all deferred promotions and hardlinks into `output`.
///
/// Promotions are emitted before normal hardlinks so that any deferred
/// hardlink whose target happens to be a promoted path finds it already
/// recorded in `emitted`.
///
/// See the [module-level documentation](self) for a description of the
/// promotion algorithm.
fn emit_deferred<W: Write>(
    hardlinks: HardLinkTracker,
    emitted: &mut EmittedPathTracker,
    output: &mut Builder<TrackedWriter<W>>,
) -> io::Result<()> {
    let (deferred, promotions) = hardlinks.drain_sorted();

    // ── Promotions ───────────────────────────────────────────────────────────
    //
    // Group promotions by their suppressed target path: all members share the
    // same underlying inode. The oldest member (lowest layer index) becomes
    // the primary and is emitted as a regular file; all others are emitted as
    // hardlinks to it, preserving inode-sharing semantics for tools like
    // rsync and du. drain_sorted guarantees ascending layer_index order within
    // each group.
    let mut promotion_groups: std::collections::HashMap<PathBuf, Vec<_>> =
        std::collections::HashMap::new();
    for promo in promotions {
        promotion_groups
            .entry(promo.target_path.clone())
            .or_default()
            .push(promo);
    }

    // Sort group keys for deterministic output order.
    let mut group_keys: Vec<PathBuf> = promotion_groups.keys().cloned().collect();
    group_keys.sort();

    for key in group_keys {
        let group = promotion_groups.remove(&key).unwrap();

        // Find the oldest group member that has buffered content and hasn't
        // already been emitted by a newer layer.
        let primary_idx = group
            .iter()
            .position(|e| e.file_data.is_some() && !emitted.contains(&e.link_path));

        let Some(primary_idx) = primary_idx else {
            // No usable primary: malformed image, or all paths already emitted.
            continue;
        };

        let primary_link_path = group[primary_idx].link_path.clone();
        let (file_canonical, data) = group[primary_idx].file_data.as_ref().unwrap();
        let regular_canonical = file_canonical.clone_as_regular();
        regular_canonical.write_to_tar(&primary_link_path, data.as_slice(), output)?;
        emitted.insert(&primary_link_path);

        for (i, promo) in group.iter().enumerate() {
            if i == primary_idx || emitted.contains(&promo.link_path) {
                continue;
            }
            // All group members reference the same inode, so any member's
            // canonical header has identical metadata. Prefer this member's
            // own header if it has one, otherwise fall back to the primary's.
            let base_canonical = promo
                .file_data
                .as_ref()
                .map(|(c, _)| c)
                .unwrap_or(file_canonical);
            base_canonical.write_hardlink_to_tar(&promo.link_path, &primary_link_path, output)?;
            emitted.insert(&promo.link_path);
        }
    }

    // ── Normal deferred hardlinks ────────────────────────────────────────────
    for hl in deferred {
        if !emitted.contains(&hl.target_path) {
            // Target was suppressed by a whiteout or never present in any
            // layer — drop the link silently.
            continue;
        }
        // Emit the target as resolved above rather than as the layer spelled
        // it: an absolute or `./`-prefixed target names the same archive
        // member, but extractors resolve a hardlink target against the
        // extraction root, and the `tar` crate (behind the dir output) refuses
        // an absolute one.
        hl.canonical
            .write_hardlink_to_tar(&hl.link_path, &hl.target_path, output)?;
        emitted.insert(&hl.link_path);
    }

    Ok(())
}

/// Merge `layers` into a single tar stream written to `sink`.
///
/// Layers are sorted by index (newest first) before processing. This is the
/// batch variant of the merge algorithm; the streaming variant is
/// [`merge_layers_into_streaming`].
///
/// Only the tests use it. Production code goes through the streaming path
/// via `write_for_spec`.
#[cfg(test)]
pub fn merge_layers_into<W: Write>(mut layers: Vec<LayerBlob>, sink: W) -> Result<(), MergeError> {
    layers.sort_by_key(|l| std::cmp::Reverse(l.index));

    let mut whiteout = WhiteoutTracker::default();
    let mut emitted = EmittedPathTracker::default();
    let mut hardlinks = HardLinkTracker::default();

    let mut output = Builder::new(TrackedWriter::new(sink));
    output.mode(tar::HeaderMode::Complete);

    for blob in &layers {
        process_layer(
            blob,
            &mut whiteout,
            &mut emitted,
            &mut hardlinks,
            &mut output,
        )?;
    }

    finish_output(hardlinks, &mut emitted, output).map_err(MergeError::Output)
}

/// Merge OCI layers into a single tar stream written to `sink`, accepting
/// layers in any arrival order.
///
/// `total_layers` must equal the number of layers declared in the manifest.
/// Layers are resequenced internally and processed newest-first; processing
/// of a given layer begins as soon as all newer layers have been processed,
/// regardless of when older layers arrive.
///
/// An `Err` delivered on the channel (the caller failing to supply a layer)
/// aborts the merge immediately with [`Error::LayerSource`]. If the channel
/// closes before all `total_layers` items are received, the merge fails with
/// [`Error::MissingLayers`].
///
/// `progress_tx`, if supplied, receives [`PackerProgress::LayerStarted`] and
/// [`PackerProgress::LayerFinished`] events around each call to
/// `process_layer`. Send failures are silently ignored.
pub fn merge_layers_into_streaming<W: Write>(
    receiver: std::sync::mpsc::Receiver<LayerItem>,
    total_layers: usize,
    sink: W,
    progress_tx: Option<&std::sync::mpsc::SyncSender<PackerProgress>>,
) -> Result<(), MergeError> {
    let mut whiteout = WhiteoutTracker::default();
    let mut emitted = EmittedPathTracker::default();
    let mut hardlinks = HardLinkTracker::default();

    let mut output = Builder::new(TrackedWriter::new(sink));
    output.mode(tar::HeaderMode::Complete);

    // next_index is the layer we want to process next (counting down from
    // total_layers-1 to 0). buffer holds layers that have arrived but whose
    // turn hasn't come yet.
    let mut buffer: std::collections::HashMap<usize, LayerBlob> = std::collections::HashMap::new();
    let mut next_index = total_layers.saturating_sub(1);
    let mut received = 0usize;

    while received < total_layers {
        let blob = match receiver.recv() {
            Ok(Ok(blob)) => blob,
            Ok(Err(e)) => return Err(Error::LayerSource(e).into()),
            Err(_) => {
                return Err(Error::MissingLayers {
                    received,
                    expected: total_layers,
                }
                .into());
            }
        };
        received += 1;
        buffer.insert(blob.index, blob);

        // Drain any contiguous descending run that is now unblocked. A single
        // arrival may unblock multiple layers if earlier arrivals were already
        // buffered and waiting for this one.
        while let Some(blob) = buffer.remove(&next_index) {
            let idx = blob.index;
            if let Some(tx) = progress_tx {
                let _ = tx.try_send(PackerProgress::LayerStarted(idx));
            }
            process_layer(
                &blob,
                &mut whiteout,
                &mut emitted,
                &mut hardlinks,
                &mut output,
            )?;
            if let Some(tx) = progress_tx {
                let _ = tx.try_send(PackerProgress::LayerFinished(idx));
            }
            if next_index == 0 {
                break;
            }
            next_index -= 1;
        }
    }

    finish_output(hardlinks, &mut emitted, output).map_err(MergeError::Output)
}

/// Emit the deferred hardlinks and promotions, then finish the archive and
/// flush the sink. Dropping the sink afterwards closes the write end of any
/// pipe, signalling EOF to its consumer (e.g. mksquashfs).
fn finish_output<W: Write>(
    hardlinks: HardLinkTracker,
    emitted: &mut EmittedPathTracker,
    mut output: Builder<TrackedWriter<W>>,
) -> io::Result<()> {
    emit_deferred(hardlinks, emitted, &mut output)?;
    output.finish()?;
    output.into_inner()?.flush()
}

/// Normalise a tar entry path by stripping any leading `./` or `/` prefix.
///
/// OCI layer tarballs commonly use `./`-prefixed paths (e.g. `./usr/bin/cat`).
/// Normalising to a plain relative path (`usr/bin/cat`) gives consistent keys
/// for the tracker data structures and the emitted tar entries.
pub fn normalize_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let s = s.trim_start_matches("./").trim_start_matches('/');
    PathBuf::from(s)
}
