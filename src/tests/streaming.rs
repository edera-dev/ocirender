//! Tests for the streaming merge paths in lib.rs.
//!
//! These tests exercise `merge_layers_into_streaming` directly (via
//! `convert_tar_streaming`) and the `StreamingPacker` async API.  No
//! mksquashfs binary is required — all StreamingPacker tests use
//! `ImageSpec::Tar` or `ImageSpec::Dir` so correctness can be verified
//! by inspecting the output directly.
//!
//! Two categories:
//!
//!   1. Resequencing correctness — the streaming merge must apply layers
//!      newest-first regardless of the order blobs arrive on the channel.
//!      Tested by feeding a fixed set of layer blobs in every permutation
//!      (and a few named orders) and asserting the overlay result is the same.
//!
//!   2. Error handling — channel errors, premature close, and download failures
//!      must abort the merge cleanly: no partial output file on disk, no
//!      hang, no panic.

use super::helpers;
use helpers::{LayerBuilder, blob, file_contents_in_tar, hardlink_target_in_tar, paths_in_tar};

use std::sync::mpsc;

use crate::{
    ImageSpec, LayerMeta, PackerProgress, StreamingPacker, image::LayerBlob,
    overlay::merge_layers_into_streaming,
};
use tempfile::NamedTempFile;

// ── shared helpers ────────────────────────────────────────────────────────────

/// Send `blobs` over a std mpsc channel in the given order and run
/// `merge_layers_into_streaming`, returning the merged tar bytes.
fn streaming_merge(blobs_in_order: Vec<LayerBlob>, total_layers: usize) -> anyhow::Result<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    for blob in blobs_in_order {
        tx.send(Ok(blob)).unwrap();
    }
    drop(tx);

    let mut out = Vec::new();
    merge_layers_into_streaming(rx, total_layers, &mut out, None)?;
    Ok(out)
}

/// Build the three-layer fixture used by most resequencing tests:
///
///   layer 0: base.txt ("old"), keep.txt ("keep")
///   layer 1: base.txt ("new")   — overwrites layer 0
///   layer 2: .wh.keep.txt       — whiteouts keep.txt from layer 0
///
/// After merging: base.txt == "new", keep.txt absent.
fn three_layer_fixture() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let layer0 = LayerBuilder::new()
        .add_file("base.txt", b"old", 0o644)
        .add_file("keep.txt", b"keep", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_file("base.txt", b"new", 0o644)
        .finish();
    let layer2 = LayerBuilder::new().add_whiteout("", "keep.txt").finish();
    (layer0, layer1, layer2)
}

/// Assert the invariant that `three_layer_fixture` produces after correct merging.
fn assert_three_layer_invariant(merged: &[u8], label: &str) {
    let paths = paths_in_tar(merged);
    assert!(
        paths.iter().any(|p| p == "base.txt"),
        "{label}: base.txt must be present"
    );
    assert!(
        !paths.iter().any(|p| p == "keep.txt"),
        "{label}: keep.txt must be absent (whited out by layer 2)"
    );
    assert_eq!(
        file_contents_in_tar(merged, "base.txt"),
        Some(b"new".to_vec()),
        "{label}: layer 1 must win the overwrite"
    );
}

/// Write raw tar bytes to a NamedTempFile kept on disk and return its path.
/// `keep()` prevents auto-deletion so the packer can open the file later.
fn layer_to_tempfile(data: &[u8]) -> std::path::PathBuf {
    let mut f = NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, data).unwrap();
    let (_, path) = f.keep().unwrap();
    path
}

/// Build a `LayerMeta` vec for `count` uncompressed tar layers.
fn layer_metas(count: usize) -> Vec<LayerMeta> {
    (0..count)
        .map(|i| LayerMeta {
            index: i,
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
        })
        .collect()
}

// ─── Resequencing: named orders ───────────────────────────────────────────────

#[test]
fn streaming_descending_order_matches_batch() {
    let (l0, l1, l2) = three_layer_fixture();
    let blobs = vec![blob(l2, 2), blob(l1, 1), blob(l0, 0)];
    let merged = streaming_merge(blobs, 3).expect("merge must succeed");
    assert_three_layer_invariant(&merged, "descending");
}

#[test]
fn streaming_ascending_order_matches_batch() {
    let (l0, l1, l2) = three_layer_fixture();
    let blobs = vec![blob(l0, 0), blob(l1, 1), blob(l2, 2)];
    let merged = streaming_merge(blobs, 3).expect("merge must succeed");
    assert_three_layer_invariant(&merged, "ascending");
}

#[test]
fn streaming_middle_first_order_matches_batch() {
    let (l0, l1, l2) = three_layer_fixture();
    let blobs = vec![blob(l1, 1), blob(l2, 2), blob(l0, 0)];
    let merged = streaming_merge(blobs, 3).expect("merge must succeed");
    assert_three_layer_invariant(&merged, "middle-first");
}

#[test]
fn streaming_oldest_last_matches_batch() {
    let (l0, l1, l2) = three_layer_fixture();
    let blobs = vec![blob(l2, 2), blob(l0, 0), blob(l1, 1)];
    let merged = streaming_merge(blobs, 3).expect("merge must succeed");
    assert_three_layer_invariant(&merged, "oldest-last");
}

#[test]
fn streaming_all_permutations_agree() {
    // Exhaustively test all 6 permutations of 3 layers.  This is the primary
    // correctness guarantee: arrival order must never affect the output.
    let (l0, l1, l2) = three_layer_fixture();
    let raw = [l0, l1, l2];

    let perms: Vec<[usize; 3]> = vec![
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];

    for perm in &perms {
        let blobs = perm.iter().map(|&i| blob(raw[i].clone(), i)).collect();
        let merged = streaming_merge(blobs, 3)
            .unwrap_or_else(|e| panic!("merge failed for perm {perm:?}: {e}"));
        assert_three_layer_invariant(&merged, &format!("perm {perm:?}"));
    }
}

// ─── Resequencing: whiteout correctness under out-of-order delivery ───────────

#[test]
fn streaming_whiteout_layer_arrives_after_suppressed_layer() {
    // The critical ordering case: the layer containing the whiteout (layer 2)
    // arrives on the channel *after* the layer it suppresses (layer 0).
    // The resequencer must not process layer 0 until layer 2 has been
    // consumed, otherwise keep.txt will be emitted before the whiteout is
    // recorded.
    let (l0, l1, l2) = three_layer_fixture();

    let blobs = vec![blob(l0, 0), blob(l1, 1), blob(l2, 2)];
    let merged = streaming_merge(blobs, 3).expect("merge must succeed");

    assert!(
        !paths_in_tar(&merged).iter().any(|p| p == "keep.txt"),
        "keep.txt must be absent: whiteout layer arrived after suppressed layer"
    );
}

#[test]
fn streaming_hardlink_resolves_correctly_out_of_order() {
    // Hardlink in layer 1 pointing at a target in layer 0.  If layers arrive
    // in ascending order the hardlink is received before its target has been
    // processed — the deferred-emit logic must still resolve it correctly.
    let layer0 = LayerBuilder::new()
        .add_file("target.txt", b"content", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_hardlink("link.txt", "target.txt")
        .finish();

    let blobs = vec![blob(layer0, 0), blob(layer1, 1)];
    let merged = streaming_merge(blobs, 2).expect("merge must succeed");

    let paths = paths_in_tar(&merged);
    assert!(
        paths.iter().any(|p| p == "target.txt"),
        "target must be present"
    );
    assert!(
        paths.iter().any(|p| p == "link.txt"),
        "link must be present"
    );
    assert_eq!(
        hardlink_target_in_tar(&merged, "link.txt").as_deref(),
        Some("target.txt"),
        "hardlink target must resolve correctly"
    );
}

// ─── Error handling: channel errors ──────────────────────────────────────────

#[test]
fn streaming_channel_error_aborts_merge() {
    let (tx, rx) = mpsc::channel::<anyhow::Result<LayerBlob>>();
    tx.send(Err(anyhow::anyhow!("simulated download failure")))
        .unwrap();
    drop(tx);

    let mut out = Vec::new();
    let result = merge_layers_into_streaming(rx, 1, &mut out, None);
    assert!(
        result.is_err(),
        "merge must return an error on channel error"
    );
    assert!(
        result.unwrap_err().to_string().contains("download error"),
        "error message must mention download error"
    );
}

#[test]
fn streaming_channel_error_after_good_layers_aborts_merge() {
    let layer0 = LayerBuilder::new()
        .add_file("ok.txt", b"fine", 0o644)
        .finish();

    let (tx, rx) = mpsc::channel();
    tx.send(Ok(blob(layer0, 1))).unwrap();
    tx.send(Err(anyhow::anyhow!("download of layer 2 failed")))
        .unwrap();
    drop(tx);

    let mut out = Vec::new();
    let result = merge_layers_into_streaming(rx, 3, &mut out, None);
    assert!(result.is_err(), "merge must return an error");
}

// ─── Error handling: premature channel close ──────────────────────────────────

#[test]
fn streaming_premature_channel_close_returns_error() {
    let layer0 = LayerBuilder::new()
        .add_file("partial.txt", b"data", 0o644)
        .finish();

    let (tx, rx) = mpsc::channel();
    tx.send(Ok(blob(layer0, 0))).unwrap();
    drop(tx); // Close after 1 of 3 declared layers.

    let mut out = Vec::new();
    let result = merge_layers_into_streaming(rx, 3, &mut out, None);
    assert!(
        result.is_err(),
        "premature close must return an error, not silently truncate"
    );
}

#[test]
fn streaming_empty_channel_close_returns_error() {
    let (_tx, rx) = mpsc::channel::<anyhow::Result<LayerBlob>>();
    drop(_tx);

    let mut out = Vec::new();
    let result = merge_layers_into_streaming(rx, 2, &mut out, None);
    assert!(result.is_err(), "empty channel close must return an error");
}

// ─── Error handling: output file not left behind on error ────────────────────

#[test]
fn convert_tar_streaming_no_partial_file_on_channel_error() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let out = NamedTempFile::new().unwrap();
        let out_path = out.path().to_path_buf();
        drop(out);

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(Err(anyhow::anyhow!("injection error")))
            .await
            .unwrap();
        drop(tx);

        let result = crate::convert_tar_streaming(rx, 1, &out_path).await;
        assert!(
            result.is_err(),
            "convert_tar_streaming must return an error"
        );
        assert!(
            !out_path.exists(),
            "partial output file must be removed after error"
        );
    });
}

#[test]
fn convert_tar_streaming_no_partial_file_on_premature_close() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let out = NamedTempFile::new().unwrap();
        let out_path = out.path().to_path_buf();
        drop(out);

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let layer0 = LayerBuilder::new().add_file("a.txt", b"a", 0o644).finish();
        tx.send(Ok(blob(layer0, 0))).await.unwrap();
        drop(tx);

        let result = crate::convert_tar_streaming(rx, 2, &out_path).await;
        assert!(result.is_err(), "premature close must return an error");
        assert!(
            !out_path.exists(),
            "partial output file must be removed after premature close"
        );
    });
}

// ─── StreamingPacker: Tar output ─────────────────────────────────────────────

#[tokio::test]
async fn streaming_packer_tar_descending_order() {
    // Layers arrive newest-first: no resequencer buffering needed.
    let (l0, l1, l2) = three_layer_fixture();
    let raw = [l0, l1, l2];

    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    let packer = StreamingPacker::new(
        layer_metas(3),
        ImageSpec::Tar {
            path: out_path.clone(),
        },
        None,
    );

    for i in [2usize, 1, 0] {
        packer
            .notify_layer_ready(i, layer_to_tempfile(&raw[i]))
            .await
            .unwrap();
    }
    packer.finish().await.expect("packer must succeed");

    let merged = std::fs::read(&out_path).unwrap();
    assert_three_layer_invariant(&merged, "packer-tar-descending");
}

#[tokio::test]
async fn streaming_packer_tar_ascending_order() {
    // Layers arrive oldest-first: worst case for resequencer buffering.
    let (l0, l1, l2) = three_layer_fixture();
    let raw = [l0, l1, l2];

    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    let packer = StreamingPacker::new(
        layer_metas(3),
        ImageSpec::Tar {
            path: out_path.clone(),
        },
        None,
    );

    for i in [0usize, 1, 2] {
        packer
            .notify_layer_ready(i, layer_to_tempfile(&raw[i]))
            .await
            .unwrap();
    }
    packer.finish().await.expect("packer must succeed");

    let merged = std::fs::read(&out_path).unwrap();
    assert_three_layer_invariant(&merged, "packer-tar-ascending");
}

#[tokio::test]
async fn streaming_packer_tar_no_partial_file_on_error() {
    // An injected download error must cause finish() to return Err and leave
    // no partial output file behind.
    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    let packer = StreamingPacker::new(
        layer_metas(2),
        ImageSpec::Tar {
            path: out_path.clone(),
        },
        None,
    );

    packer.notify_error(anyhow::anyhow!("injected error")).await;
    let result = packer.finish().await;

    assert!(
        result.is_err(),
        "finish must return an error after notify_error"
    );
    assert!(
        !out_path.exists(),
        "partial output file must be removed after error"
    );
}

// ─── StreamingPacker: Dir output ─────────────────────────────────────────────

#[tokio::test]
async fn streaming_packer_dir_produces_correct_content() {
    // Verify that the Dir output path applies overlay semantics correctly.
    // We check the extracted filesystem directly rather than via a tar helper.
    let (l0, l1, l2) = three_layer_fixture();
    let raw = [l0, l1, l2];

    let out_dir = tempfile::TempDir::new().unwrap();
    let out_path = out_dir.path().to_path_buf();

    let packer = StreamingPacker::new(
        layer_metas(3),
        ImageSpec::Dir {
            path: out_path.clone(),
        },
        None,
    );

    // Deliver in an order that exercises resequencer buffering.
    for i in [1usize, 2, 0] {
        packer
            .notify_layer_ready(i, layer_to_tempfile(&raw[i]))
            .await
            .unwrap();
    }
    packer.finish().await.expect("packer must succeed");

    assert!(
        out_path.join("base.txt").exists(),
        "base.txt must be present"
    );
    assert!(
        !out_path.join("keep.txt").exists(),
        "keep.txt must be absent (whited out)"
    );
    assert_eq!(
        std::fs::read(out_path.join("base.txt")).unwrap(),
        b"new",
        "layer 1 must win the overwrite"
    );
}

// ─── StreamingPacker: progress events ────────────────────────────────────────

#[tokio::test]
async fn streaming_packer_progress_events_fired_for_all_layers() {
    // Uses Tar output — no mksquashfs required. Verifies that LayerStarted and
    // LayerFinished are emitted for every layer index regardless of arrival order.
    let (l0, l1, l2) = three_layer_fixture();
    let raw = [l0, l1, l2];

    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(16);
    let packer = StreamingPacker::new(
        layer_metas(3),
        ImageSpec::Tar { path: out_path },
        Some(progress_tx),
    );

    for i in [1usize, 0, 2] {
        packer
            .notify_layer_ready(i, layer_to_tempfile(&raw[i]))
            .await
            .unwrap();
    }
    packer.finish().await.expect("packer must succeed");

    let mut started = std::collections::HashSet::new();
    let mut finished = std::collections::HashSet::new();
    // Use recv().await rather than try_recv(): finish() returning only means
    // the spawn_blocking task is done, not that the relay_from_blocking task
    // has finished forwarding all buffered events to progress_rx. Blocking
    // until the channel closes ensures we see every event.
    while let Some(ev) = progress_rx.recv().await {
        match ev {
            PackerProgress::LayerStarted(i) => {
                started.insert(i);
            }
            PackerProgress::LayerFinished(i) => {
                finished.insert(i);
            }
        }
    }

    for i in 0..3 {
        assert!(started.contains(&i), "LayerStarted({i}) must be emitted");
        assert!(finished.contains(&i), "LayerFinished({i}) must be emitted");
    }
}

// ─── StreamingPacker: error / edge cases ─────────────────────────────────────

#[tokio::test]
async fn streaming_packer_notify_error_causes_finish_to_fail() {
    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    let packer = StreamingPacker::new(layer_metas(2), ImageSpec::Tar { path: out_path }, None);

    packer
        .notify_error(anyhow::anyhow!("injected download error"))
        .await;
    let result = packer.finish().await;
    assert!(
        result.is_err(),
        "finish must return an error after notify_error"
    );
}

#[tokio::test]
async fn streaming_packer_out_of_bounds_index_returns_error() {
    let out = NamedTempFile::new().unwrap();
    let out_path = out.path().to_path_buf();
    drop(out);

    let packer = StreamingPacker::new(
        layer_metas(2), // indices 0 and 1 only
        ImageSpec::Tar { path: out_path },
        None,
    );

    let result = packer
        .notify_layer_ready(99, std::path::PathBuf::from("/dev/null"))
        .await;
    assert!(result.is_err(), "out-of-bounds index must return an error");
}
