//! Core algorithm: merge OCI layers into a single flat tar stream.
//!
//! File content is never buffered in full — entries are streamed directly
//! into the provided `Write` sink (typically mksquashfs's stdin pipe).
//! The only in-memory state is the tracker data structures, the small
//! hard-link metadata structs deferred to the end, and the content of any
//! regular files that were suppressed by a whiteout but have surviving
//! hardlinks that need to be promoted to real files.

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

/// Process a single layer blob, updating trackers and streaming non-deferred
/// entries into `output`.  Called by both [`merge_layers_into`] and
/// [`merge_layers_into_streaming`].
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

        // Skip the root directory entry (`./` or `/`), which normalises to an
        // empty path and is meaningless in a merged tar.
        if path.as_os_str().is_empty() {
            continue;
        }

        // Handle whiteout entries
        // These set suppression rules for older layers and are never emitted.
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

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

        // Path-level whiteout suppression
        // If this path is suppressed by a higher-layer whiteout we skip it,
        // but for regular files we first buffer the content.  A hardlink in
        // the same or an older layer may be alive and need the inode's bytes
        // to be promoted into a standalone file.
        if whiteout.is_suppressed(&path, blob.index) {
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
            // All other suppressed entry types (dir, symlink, hardlink) are
            // dropped without buffering.
            continue;
        }

        // Skip paths already written by a newer layer
        if emitted.contains(&path) {
            continue;
        }

        // Read the canonical header
        let canonical =
            CanonicalTarHeader::from_entry(&mut entry).context("capturing entry header")?;

        // Handle hard links
        if canonical.entry_type() == EntryType::Link {
            let link_target = canonical
                .link_name()
                .context("reading hard link target")?
                .context("hard link has no target")?;
            let target_path = normalize_path(&link_target);

            if whiteout.is_suppressed(&target_path, blob.index) {
                // The target was removed by a higher-layer whiteout, but the
                // link path itself is alive.  Record a promotion: we will emit
                // link_path as a standalone regular file (using the suppressed
                // target's buffered content) rather than as a hardlink to a
                // path that will not appear in the output.
                hardlinks.record_promotion(path, target_path, blob.index);
            } else {
                // Target is not suppressed.  Defer the hardlink normally; it
                // will be emitted after all layers are processed, once we can
                // confirm the target was written.
                hardlinks.record(path, target_path, blob.index, canonical);
            }
            continue;
        }

        // Stream entry directly into the output tar
        canonical
            .write_to_tar(&path, &mut entry, output)
            .with_context(|| format!("emitting {}", path.display()))?;
        emitted.insert(&path);
    }

    // Discard the speculative suppressed-file buffer now that this layer is
    // complete.  It exists only to serve same-layer hardlink promotions and
    // is never needed once process_layer returns.
    hardlinks.end_layer();

    Ok(())
}

/// Emit the deferred promotion and hardlink entries into `output`.
///
/// Promotions are emitted first so that any normal deferred hardlink whose
/// target happens to be a promoted link path finds it already in the
/// emitted-path set.
fn emit_deferred<W: Write>(
    hardlinks: HardLinkTracker,
    emitted: &mut EmittedPathTracker,
    output: &mut Builder<W>,
) -> Result<()> {
    let (deferred, promotions) = hardlinks.drain_sorted();

    // ── Promotions: surviving hardlinks whose targets were whited out ────────
    //
    // All promotions that share the same suppressed `target_path` reference
    // the same underlying inode and must be emitted as a hardlink group.
    // The group member with the lowest layer index becomes the primary: it is
    // emitted as a standalone regular file.  Every other group member is
    // emitted as a hardlink to that primary, preserving st_ino / st_nlink
    // semantics that an overlayfs upper layer (or tools like rsync / du) can
    // observe.
    //
    // Note: `drain_sorted` returns promotions sorted by ascending layer_index,
    // so the natural iteration order already gives us the oldest-first
    // ordering we want within each group.
    let mut promotion_groups: std::collections::HashMap<PathBuf, Vec<_>> =
        std::collections::HashMap::new();
    for promo in promotions {
        promotion_groups
            .entry(promo.target_path.clone())
            .or_default()
            .push(promo);
    }

    // Iterate groups in a deterministic order so test output is stable.
    let mut group_keys: Vec<PathBuf> = promotion_groups.keys().cloned().collect();
    group_keys.sort();

    for key in group_keys {
        let group = promotion_groups.remove(&key).unwrap();
        // group is already sorted by layer_index (drain_sorted guarantees this).

        // Find the first group member that has buffered file content and whose
        // link path has not already been emitted by a newer layer.
        let primary_idx = group
            .iter()
            .position(|e| e.file_data.is_some() && !emitted.contains(&e.link_path));

        let Some(primary_idx) = primary_idx else {
            // No usable primary (malformed image, or all paths emitted already).
            continue;
        };

        // Emit the primary as a regular file.
        let primary_link_path = group[primary_idx].link_path.clone();
        let (file_canonical, data) = group[primary_idx].file_data.as_ref().unwrap();
        let regular_canonical = file_canonical.clone_as_regular();
        regular_canonical
            .write_to_tar(&primary_link_path, data.as_slice(), output)
            .with_context(|| format!("emitting promoted entry {}", primary_link_path.display()))?;
        emitted.insert(&primary_link_path);

        // Emit every other group member as a hardlink to the primary.
        // We use the file's canonical header (same inode metadata) recast as
        // a Link entry pointing at the primary link path.
        for (i, promo) in group.iter().enumerate() {
            if i == primary_idx {
                continue;
            }
            if emitted.contains(&promo.link_path) {
                continue;
            }
            // Use this member's file_data canonical if available, otherwise
            // fall back to the primary's.  In practice all group members
            // reference the same inode so their metadata is identical.
            // Use the base canonical only for its inode metadata (mode, uid,
            // gid, mtime).  write_hardlink_to_tar rebuilds path and linkpath
            // from scratch using PAX extensions + builder.append so that a
            // long primary_link_path doesn't trigger a GNU LongName auxiliary
            // entry between the queued PAX extensions and the main entry.
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
            // Target was suppressed or never present — drop the link.
            continue;
        }
        hl.canonical
            .write_to_tar(&hl.link_path, &[] as &[u8], output)
            .with_context(|| format!("emitting hard link {}", hl.link_path.display()))?;
        emitted.insert(&hl.link_path);
    }

    Ok(())
}

/// Merge layers, streaming the resulting tar into `sink`.
///
/// `sink` is typically the stdin pipe of a `mksquashfs` subprocess.
/// File data flows directly from the layer blobs into `sink` without
/// being accumulated in memory.
pub fn merge_layers_into<W: Write>(mut layers: Vec<LayerBlob>, sink: W) -> Result<()> {
    // Process in reverse (newest first).
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
    // Flush and drop the Builder, closing the write end of the pipe so
    // mksquashfs sees EOF and knows the tar stream is complete.
    let mut sink = output.into_inner()?;
    sink.flush()?;
    Ok(())
}

/// Streaming variant of [`merge_layers_into`].
///
/// Layers arrive via `receiver` in arbitrary order as downloads complete.
/// `total_layers` must match the number of layers declared in the manifest.
///
/// After each layer is fully processed, its index is sent on `progress_tx`
/// if one was supplied.  Send failures are silently ignored.
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

    // Resequencing buffer: index → LayerBlob.
    // We process layers newest-first, so next_index counts down from
    // total_layers-1 to 0.
    let mut buffer: std::collections::HashMap<usize, LayerBlob> = std::collections::HashMap::new();
    let mut next_index = total_layers.saturating_sub(1);
    let mut received = 0usize;

    while received < total_layers {
        // Block until the next blob (or error) arrives.
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

        // Drain any contiguous descending run we can now process.
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

/// Strip leading `./` or `/` from paths.
pub fn normalize_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let s = s.trim_start_matches("./").trim_start_matches('/');
    PathBuf::from(s)
}
