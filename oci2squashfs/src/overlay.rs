//! Core algorithm: merge OCI layers into a single flat tar stream.
//!
//! File content is never buffered in full — entries are streamed directly
//! into the provided `Write` sink (typically mksquashfs's stdin pipe).
//! The only in-memory state is the tracker data structures and the small
//! hard-link metadata structs deferred to the end.

use anyhow::{Context, Result};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use tar::{Builder, EntryType};

use crate::{
    canonical::CanonicalTarHeader,
    image::LayerBlob,
    layers::open_layer,
    tracker::{EmittedPathTracker, HardLinkTracker, WhiteoutTracker},
};

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
        let mut archive = open_layer(&blob.path, &blob.media_type)
            .with_context(|| format!("opening layer {}", blob.path.display()))?;

        let entries = archive.entries().context("reading tar entries")?;
        for entry_result in entries {
            let mut entry = entry_result.context("reading tar entry")?;
            let raw_path = entry.path().context("entry path")?.into_owned();
            let path = normalize_path(&raw_path);

            // 1. Check whiteout suppression.
            if whiteout.is_suppressed(&path, blob.index) {
                continue;
            }

            // 2. Check already-emitted.
            if emitted.contains(&path) {
                continue;
            }

            // 3. Handle whiteout entries.
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

            // We capture the header + PAX extensions first (cheap), then
            // stream the entry body straight into the Builder without
            // buffering it in a Vec.
            let canonical =
                CanonicalTarHeader::from_entry(&mut entry).context("capturing entry header")?;

            // 4. Handle hard links — defer, emitting only metadata structs.
            if canonical.entry_type() == EntryType::Link {
                let link_target = canonical
                    .link_name()
                    .context("reading hard link target")?
                    .context("hard link has no target")?;
                let target_path = normalize_path(&link_target);
                if !whiteout.is_suppressed(&target_path, blob.index) {
                    hardlinks.record(path, target_path, blob.index, canonical);
                }
                continue;
            }

            // 5. Stream entry directly into the output tar.
            //
            canonical
                .write_to_tar(&path, &mut entry, &mut output)
                .with_context(|| format!("emitting {}", path.display()))?;
            emitted.insert(&path);
        }
    }

    // Emit deferred hard links (oldest-layer first).
    // These are pure metadata — no file content to stream.
    for hl in hardlinks.drain_sorted() {
        if !emitted.contains(&hl.target_path) {
            continue;
        }
        hl.canonical
            .write_to_tar(&hl.link_path, &[] as &[u8], &mut output)
            .with_context(|| format!("emitting hard link {}", hl.link_path.display()))?;
        emitted.insert(&hl.link_path);
    }

    output.finish()?;
    // Flush and drop the Builder, closing the write end of the pipe so
    // mksquashfs sees EOF and knows the tar stream is complete.
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
