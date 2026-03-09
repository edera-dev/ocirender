//! Regression tests, one per production bug found during verify runs.
//! Each test is named after the bug it guards against and has a comment
//! explaining the root cause and how it was discovered.

use std::io::Cursor;
use tar::{Builder, EntryType, Header};

#[path = "helpers/mod.rs"]
mod helpers;
use helpers::{blob, merge, paths_in_tar, LayerBuilder};
// regression.rs also uses:
use helpers::hardlink_target_in_tar;

// ─── Regression tests ────────────────────────────────────────────────────────

/// Bug: whiteout suppression condition was inverted (`current_layer > layer_index` instead of
/// `current_layer < layer_index`), causing whiteouts to suppress entries from *newer* layers rather
/// than older ones. Files removed via `.wh.<name>` in a later layer were still appearing in the
/// output.
///
/// Discovered via: `usr/share/vulkan/icd.d/lvp_icd.json` appearing in squashfs output despite being
/// removed by a dpkg-divert whiteout in a later layer.
#[test]
fn regress_whiteout_suppression_direction() {
    let layer0 = LayerBuilder::new()
        .add_file("usr/share/vulkan/icd.d/lvp_icd.json", b"data", 0o644)
        .finish();
    // Layer 1 whiteouts the file from layer 0.
    let layer1 = LayerBuilder::new()
        .add_whiteout("usr/share/vulkan/icd.d", "lvp_icd.json")
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);
    assert!(
        !paths
            .iter()
            .any(|p| p == "usr/share/vulkan/icd.d/lvp_icd.json"),
        "whited-out file must not appear: got paths {paths:?}"
    );
}

/// Bug: hard link targets longer than 100 bytes were being silently truncated because we read them
/// via `entry.header().link_name()`, which only reads the raw 100-byte USTAR `linkname` field. The
/// PAX `linkpath` extension carrying the full target was ignored. The truncated target path was
/// then not found in the emitted-path tracker, so the link was silently dropped.
///
/// Discovered via: PostgreSQL/hammerdb timezone hard-link aliases all missing from squashfs output
/// (e.g. `timezone/Jamaica -> .../America/...` dropped because the full target path exceeded 100
/// bytes and was truncated to `America` or similar).
#[test]
fn regress_long_hardlink_target_truncated() {
    // Construct a target path that exceeds 100 bytes.
    let long_dir = "a".repeat(95);
    let target = format!("{long_dir}/canonical_file");
    assert!(
        target.len() > 100,
        "test setup: target must exceed 100 bytes"
    );

    let layer0 = LayerBuilder::new()
        .add_dir(&long_dir)
        .add_file(&target, b"content", 0o644)
        .add_hardlink("alias_file", &target)
        .finish();

    let merged = merge(vec![blob(layer0, 0)]);
    let paths = paths_in_tar(&merged);

    assert!(
        paths.iter().any(|p| p == &target),
        "canonical target must be present"
    );
    assert!(
        paths.iter().any(|p| p == "alias_file"),
        "hard link alias must be present; was it silently dropped due to truncated target?"
    );

    let resolved = hardlink_target_in_tar(&merged, "alias_file");
    assert_eq!(
        resolved.as_deref(),
        Some(target.as_str()),
        "hard link target must be the full untruncated path"
    );
}

/// Variant of the above: hard link and target in different layers, with a long target path. Guards
/// against the combination of cross-layer deferral and PAX target resolution both being required
/// simultaneously.
#[test]
fn regress_long_hardlink_target_cross_layer() {
    let long_dir = "b".repeat(60);
    let target = format!("{long_dir}/canonical_file");

    let layer0 = LayerBuilder::new()
        .add_dir(&long_dir)
        .add_file(&target, b"content", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_hardlink("alias_file", &target)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(
        paths.iter().any(|p| p == &target),
        "canonical target must be present"
    );
    assert!(
        paths.iter().any(|p| p == "alias_file"),
        "cross-layer hard link with long target must not be silently dropped"
    );
}

/// Bug: normalize_path only stripped leading `./` but not leading `/`. Hard link targets stored as
/// absolute paths in the tar (e.g. `/usr/share/foo`) would not match the normalized emitted path
/// (`usr/share/foo`), causing the link to be silently dropped.
#[test]
fn regress_absolute_hardlink_target_normalized() {
    // Manually construct a layer with an absolute-path hard link target,
    // which some tar producers emit.
    let mut builder = Builder::new(Vec::new());

    let mut file_hdr = Header::new_ustar();
    file_hdr.set_path("usr/share/foo").unwrap();
    file_hdr.set_size(4);
    file_hdr.set_mode(0o644);
    file_hdr.set_mtime(0);
    file_hdr.set_uid(0);
    file_hdr.set_gid(0);
    file_hdr.set_cksum();
    builder.append(&file_hdr, Cursor::new(b"data")).unwrap();

    // Use a PAX linkpath with a leading slash — the absolute form.
    builder
        .append_pax_extensions([("linkpath", b"/usr/share/foo" as &[u8])])
        .unwrap();
    let mut link_hdr = Header::new_ustar();
    link_hdr.set_path("usr/share/bar").unwrap();
    link_hdr.set_entry_type(EntryType::Link);
    link_hdr.set_link_name("usr/share/foo").ok(); // truncated USTAR field (no leading slash here)
    link_hdr.set_size(0);
    link_hdr.set_mode(0o644);
    link_hdr.set_mtime(0);
    link_hdr.set_uid(0);
    link_hdr.set_gid(0);
    link_hdr.set_cksum();
    builder
        .append(&link_hdr, Cursor::new(b"" as &[u8]))
        .unwrap();

    builder.finish().unwrap();
    let layer0 = builder.into_inner().unwrap();

    let merged = merge(vec![blob(layer0, 0)]);
    let paths = paths_in_tar(&merged);
    assert!(
        paths.iter().any(|p| p == "usr/share/bar"),
        "hard link with absolute PAX linkpath must not be dropped after normalization"
    );
}

/// Bug: a simple whiteout (`.wh.<name>`) on a directory was only suppressing the directory entry
/// itself, not its children. `is_suppressed` only checked for `Simple` state at the terminal node
/// of the trie walk, so child paths like `home/ubuntu/.bashrc` would not match and were emitted
/// anyway.  The fix treats `Simple` the same as `Opaque` in the ancestor check — once a path is
/// whited out, everything beneath it is suppressed regardless of whiteout type.
///
/// Discovered via: `plexinc/pms-docker:latest` producing a `/home/ubuntu` directory that umoci
/// correctly suppressed.
#[test]
fn regress_simple_whiteout_suppresses_directory_children() {
    let layer0 = LayerBuilder::new()
        .add_dir("home/ubuntu")
        .add_file("home/ubuntu/.bashrc", b"data", 0o644)
        .finish();
    let layer1 = LayerBuilder::new().add_whiteout("home", "ubuntu").finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);
    assert!(
        !paths.iter().any(|p| p == "home/ubuntu"),
        "whited-out directory must not appear"
    );
    assert!(
        !paths.iter().any(|p| p.starts_with("home/ubuntu/")),
        "children of whited-out directory must not appear"
    );
}
