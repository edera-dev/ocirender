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

use anyhow::{Context, Result};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tar::{Builder, EntryType};

use crate::{
    PackerProgress,
    canonical::CanonicalTarHeader,
    image::LayerBlob,
    layers::open_layer,
    tracker::{EmittedPathTracker, HardLinkTracker, WhiteoutTracker},
};

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
    output: &mut Builder<W>,
) -> Result<()> {
    let mut archive = open_layer(&blob.path, &blob.media_type)
        .with_context(|| format!("opening layer {}", blob.path.display()))?;

    let entries = archive.entries().context("reading tar entries")?;
    for entry_result in entries {
        let mut entry = entry_result.context("reading tar entry")?;
        let raw_path = entry.path().context("entry path")?.into_owned();
        let path = normalize_path(&raw_path);

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
            // these bytes for promotion to a standalone file.
            let entry_type = entry.header().entry_type();
            if entry_type == EntryType::Regular || entry_type == EntryType::Continuous {
                let canonical = CanonicalTarHeader::from_entry(&mut entry)
                    .context("capturing suppressed entry header")?;
                let mut data = Vec::new();
                entry
                    .read_to_end(&mut data)
                    .context("buffering suppressed file content")?;
                hardlinks.note_suppressed_file(path, canonical, data);
            }
            // Directories, symlinks, and hardlinks pointing at suppressed
            // paths are dropped without buffering.
            continue;
        }

        // Newest version already emitted — skip older duplicate.
        if emitted.contains(&path) {
            continue;
        }

        let canonical =
            CanonicalTarHeader::from_entry(&mut entry).context("capturing entry header")?;

        if canonical.entry_type() == EntryType::Link {
            let link_target = canonical
                .link_name()
                .context("reading hard link target")?
                .context("hard link has no target")?;
            let target_path = normalize_path(&link_target);

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

        canonical
            .write_to_tar(&path, &mut entry, output)
            .with_context(|| format!("emitting {}", path.display()))?;
        emitted.insert(&path);
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
    output: &mut Builder<W>,
) -> Result<()> {
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
        regular_canonical
            .write_to_tar(&primary_link_path, data.as_slice(), output)
            .with_context(|| format!("emitting promoted entry {}", primary_link_path.display()))?;
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
            base_canonical
                .write_hardlink_to_tar(&promo.link_path, &primary_link_path, output)
                .with_context(|| {
                    format!(
                        "emitting within-group hardlink {} → {}",
                        promo.link_path.display(),
                        primary_link_path.display()
                    )
                })?;
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
        hl.canonical
            .write_to_tar(&hl.link_path, &[] as &[u8], output)
            .with_context(|| format!("emitting hard link {}", hl.link_path.display()))?;
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
/// Primarily used in unit tests. Production code goes through the streaming
/// path via `write_for_spec`.
pub fn merge_layers_into<W: Write>(mut layers: Vec<LayerBlob>, sink: W) -> Result<()> {
    layers.sort_by_key(|l| std::cmp::Reverse(l.index));

    let mut whiteout = WhiteoutTracker::default();
    let mut emitted = EmittedPathTracker::default();
    let mut hardlinks = HardLinkTracker::default();

    let mut output = Builder::new(sink);
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

    emit_deferred(hardlinks, &mut emitted, &mut output)?;

    output.finish()?;
    // Flush and drop the Builder to close the write end of any pipe, signalling
    // EOF to the consumer (e.g. mksquashfs).
    let mut sink = output.into_inner()?;
    sink.flush()?;
    Ok(())
}

/// Merge OCI layers into a single tar stream written to `sink`, accepting
/// layers in any arrival order.
///
/// `total_layers` must equal the number of layers declared in the manifest.
/// Layers are resequenced internally and processed newest-first; processing
/// of a given layer begins as soon as all newer layers have been processed,
/// regardless of when older layers arrive.
///
/// A download error delivered as `Err` on the channel aborts the merge
/// immediately and propagates the error to the caller. If the channel closes
/// before all `total_layers` items are received, an error is returned.
///
/// `progress_tx`, if supplied, receives [`PackerProgress::LayerStarted`] and
/// [`PackerProgress::LayerFinished`] events around each call to
/// `process_layer`. Send failures are silently ignored.
pub fn merge_layers_into_streaming<W: Write>(
    receiver: std::sync::mpsc::Receiver<anyhow::Result<LayerBlob>>,
    total_layers: usize,
    sink: W,
    progress_tx: Option<&std::sync::mpsc::SyncSender<PackerProgress>>,
) -> Result<()> {
    let mut whiteout = WhiteoutTracker::default();
    let mut emitted = EmittedPathTracker::default();
    let mut hardlinks = HardLinkTracker::default();

    let mut output = Builder::new(sink);
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
            Ok(Err(e)) => return Err(e).context("download error received on streaming channel"),
            Err(_) => {
                anyhow::bail!(
                    "layer channel closed after {received} of {total_layers} layers; \
                     sender dropped without completing all layers"
                );
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

    emit_deferred(hardlinks, &mut emitted, &mut output)?;

    output.finish()?;
    let mut sink = output.into_inner()?;
    sink.flush()?;
    Ok(())
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
