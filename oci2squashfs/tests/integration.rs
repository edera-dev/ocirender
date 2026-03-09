//! Synthetic integration tests for the OCI → squashfs merge pipeline.
//! These tests exercise `overlay::merge_layers_into` directly on in-memory
//! blobs and inspect the resulting merged tar without invoking mksquashfs.

#[path = "helpers/mod.rs"]
mod helpers;
use helpers::{
    blob, hardlink_target_in_tar, merge, paths_in_tar, symlink_target_in_tar, LayerBuilder,
};

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn test_long_symlink_pax_preserved() {
    let long_target: String = "a".repeat(200);
    let layer0 = LayerBuilder::new()
        .add_symlink("link_to_long", &long_target)
        .finish();
    let merged = merge(vec![blob(layer0, 0)]);
    assert_eq!(
        symlink_target_in_tar(&merged, "link_to_long").as_deref(),
        Some(long_target.as_str()),
        "long symlink target must round-trip via PAX"
    );
}

#[test]
fn test_long_hardlink_pax_preserved() {
    let long_target: String = "a".repeat(200);
    let layer0 = LayerBuilder::new()
        .add_file(&long_target, b"hello", 0o644)
        .add_hardlink("link_to_long", &long_target)
        .finish();
    let merged = merge(vec![blob(layer0, 0)]);
    assert_eq!(
        hardlink_target_in_tar(&merged, "link_to_long").as_deref(),
        Some(long_target.as_str()),
        "long hardlink target must round-trip via PAX"
    );
}

#[test]
fn test_hardlink_across_layers() {
    let layer0 = LayerBuilder::new()
        .add_file("original.txt", b"hello", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_hardlink("link.txt", "original.txt")
        .finish();
    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);
    assert!(paths.contains(&"original.txt".to_string()));
    assert!(paths.contains(&"link.txt".to_string()));
}

#[test]
fn test_simple_whiteout() {
    let layer0 = LayerBuilder::new()
        .add_file("secret.txt", b"private", 0o644)
        .finish();
    let layer1 = LayerBuilder::new().add_whiteout("", "secret.txt").finish();
    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    assert!(!paths_in_tar(&merged).iter().any(|p| p == "secret.txt"));
}

#[test]
fn test_opaque_whiteout() {
    let layer0 = LayerBuilder::new()
        .add_dir("mydir")
        .add_file("mydir/old.txt", b"old", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_dir("mydir")
        .add_opaque_whiteout("mydir")
        .finish();
    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    assert!(!paths_in_tar(&merged).iter().any(|p| p == "mydir/old.txt"));
}

#[test]
fn test_opaque_whiteout_with_repopulation() {
    let layer0 = LayerBuilder::new()
        .add_dir("dir")
        .add_file("dir/old.txt", b"old", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_dir("dir")
        .add_opaque_whiteout("dir")
        .add_file("dir/new.txt", b"new", 0o644)
        .finish();
    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);
    assert!(paths.iter().any(|p| p == "dir/new.txt"));
    assert!(!paths.iter().any(|p| p == "dir/old.txt"));
}

#[test]
fn test_hardlink_to_whiteout_target_dropped() {
    let layer0 = LayerBuilder::new()
        .add_file("gone.txt", b"data", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_whiteout("", "gone.txt")
        .add_hardlink("link.txt", "gone.txt")
        .finish();
    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);
    assert!(!paths.iter().any(|p| p == "gone.txt"));
    assert!(!paths.iter().any(|p| p == "link.txt"));
}
