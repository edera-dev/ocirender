//! Synthetic integration tests for the OCI → squashfs merge pipeline.
//! These tests exercise `overlay::merge_layers_into` directly on in-memory
//! blobs and inspect the resulting merged tar without invoking mksquashfs.

#[path = "helpers/mod.rs"]
mod helpers;
use helpers::{
    LayerBuilder, blob, entry_type_in_tar, file_contents_in_tar, file_mode_in_tar,
    hardlink_target_in_tar, merge, paths_in_tar, symlink_target_in_tar,
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

#[test]
fn test_hardlink_alias_survives_whiteout_of_original_name() {
    // layer0: original.txt (file) + alias.txt -> original.txt (hardlink)
    // layer1: .wh.original.txt
    // Expected: alias.txt survives, promoted to a real file with content "hello"
    let layer0 = LayerBuilder::new()
        .add_file("original.txt", b"hello", 0o644)
        .add_hardlink("alias.txt", "original.txt")
        .finish();

    let layer1 = LayerBuilder::new()
        .add_whiteout("", "original.txt")
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(!paths.iter().any(|p| p == "original.txt"));
    assert!(paths.iter().any(|p| p == "alias.txt"));

    assert_eq!(hardlink_target_in_tar(&merged, "alias.txt"), None);
    assert_eq!(
        file_contents_in_tar(&merged, "alias.txt"),
        Some(b"hello".to_vec())
    );
}

#[test]
fn test_hardlink_alias_survives_whiteout_of_original_name_across_layers() {
    let layer0 = LayerBuilder::new()
        .add_file("original.txt", b"hello", 0o644)
        .finish();

    let layer1 = LayerBuilder::new()
        .add_hardlink("alias.txt", "original.txt")
        .finish();

    let layer2 = LayerBuilder::new()
        .add_whiteout("", "original.txt")
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1), blob(layer2, 2)]);
    let paths = paths_in_tar(&merged);

    assert!(!paths.iter().any(|p| p == "original.txt"));
    assert!(paths.iter().any(|p| p == "alias.txt"));
}

#[test]
fn test_directory_replaced_by_file_across_layers() {
    let layer0 = LayerBuilder::new()
        .add_dir("path")
        .add_file("path/child.txt", b"old", 0o644)
        .finish();

    let layer1 = LayerBuilder::new()
        .add_whiteout("", "path")
        .add_file("path", b"replacement", 0o644)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "path"));
    assert!(
        !paths.iter().any(|p| p == "path/child.txt"),
        "children of the old directory must not survive replacement by a file"
    );
}

#[test]
fn test_symlink_replaced_by_directory_across_layers() {
    let layer0 = LayerBuilder::new()
        .add_symlink("path", "some/target")
        .finish();

    let layer1 = LayerBuilder::new()
        .add_whiteout("", "path")
        .add_dir("path")
        .add_file("path/child.txt", b"new", 0o644)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "path"));
    assert!(paths.iter().any(|p| p == "path/child.txt"));
    assert_eq!(symlink_target_in_tar(&merged, "path"), None);
}

#[test]
fn test_directory_implicitly_replaced_by_symlink_no_whiteout() {
    // Some real-world images (e.g. gitlab-toolbox's `srv/gitlab/log` symlink
    // covering a prior directory) replace a directory subtree with a
    // non-directory in a newer layer *without* an explicit whiteout. The OCI
    // layer spec is ambiguous here in practice, but Docker/overlayfs
    // semantics shadow the older subtree implicitly. Without that handling,
    // the merged tar contains both a symlink at P and a regular file at
    // P/child, which mksquashfs rejects with "non-directory" FATAL ERROR.
    let layer0 = LayerBuilder::new()
        .add_dir("path")
        .add_file("path/child.txt", b"old", 0o644)
        .finish();

    let layer1 = LayerBuilder::new()
        .add_symlink("path", "elsewhere")
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "path"));
    assert!(
        !paths.iter().any(|p| p == "path/child.txt"),
        "older-layer children of an implicitly-shadowed directory must not \
         survive; found paths: {paths:?}"
    );
    assert_eq!(
        symlink_target_in_tar(&merged, "path"),
        Some("elsewhere".to_string())
    );
}

#[test]
fn test_directory_implicitly_replaced_by_regular_file_no_whiteout() {
    let layer0 = LayerBuilder::new()
        .add_dir("path")
        .add_file("path/child.txt", b"old", 0o644)
        .finish();

    let layer1 = LayerBuilder::new().add_file("path", b"new", 0o644).finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "path"));
    assert!(
        !paths.iter().any(|p| p == "path/child.txt"),
        "older-layer children of an implicitly-shadowed file must not survive"
    );
    assert_eq!(
        file_contents_in_tar(&merged, "path").as_deref(),
        Some(&b"new"[..])
    );
}

#[test]
fn test_directory_replaced_by_symlink_across_layers() {
    let layer0 = LayerBuilder::new()
        .add_dir("path")
        .add_file("path/child.txt", b"old", 0o644)
        .finish();

    let layer1 = LayerBuilder::new()
        .add_whiteout("", "path")
        .add_symlink("path", "elsewhere")
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "path"));
    assert!(
        !paths.iter().any(|p| p == "path/child.txt"),
        "children of the removed directory must not survive replacement by a symlink"
    );
    assert_eq!(
        symlink_target_in_tar(&merged, "path"),
        Some("elsewhere".to_string())
    );
}

#[test]
fn test_file_replaced_by_directory_across_layers() {
    let layer0 = LayerBuilder::new().add_file("path", b"old", 0o644).finish();

    let layer1 = LayerBuilder::new()
        .add_whiteout("", "path")
        .add_dir("path")
        .add_file("path/child.txt", b"new", 0o644)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "path"));
    assert!(paths.iter().any(|p| p == "path/child.txt"));
}

#[test]
fn test_opaque_whiteout_with_nested_repopulation() {
    let layer0 = LayerBuilder::new()
        .add_dir("dir")
        .add_dir("dir/sub")
        .add_file("dir/sub/old.txt", b"old", 0o644)
        .add_file("dir/peer.txt", b"peer", 0o644)
        .finish();

    let layer1 = LayerBuilder::new()
        .add_dir("dir")
        .add_opaque_whiteout("dir")
        .add_dir("dir/sub")
        .add_file("dir/sub/new.txt", b"new", 0o644)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "dir/sub/new.txt"));
    assert!(!paths.iter().any(|p| p == "dir/sub/old.txt"));
    assert!(!paths.iter().any(|p| p == "dir/peer.txt"));
}

#[test]
fn test_simple_whiteout_then_recreate_same_path_in_later_layer() {
    let layer0 = LayerBuilder::new()
        .add_file("foo.txt", b"old", 0o644)
        .finish();

    let layer1 = LayerBuilder::new().add_whiteout("", "foo.txt").finish();

    let layer2 = LayerBuilder::new()
        .add_file("foo.txt", b"new", 0o644)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1), blob(layer2, 2)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "foo.txt"));
}

#[test]
fn test_whiteout_directory_then_recreate_descendant_in_later_layer() {
    let layer0 = LayerBuilder::new()
        .add_dir("dir")
        .add_file("dir/old.txt", b"old", 0o644)
        .finish();

    let layer1 = LayerBuilder::new().add_whiteout("", "dir").finish();

    let layer2 = LayerBuilder::new()
        .add_dir("dir")
        .add_file("dir/new.txt", b"new", 0o644)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1), blob(layer2, 2)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "dir/new.txt"));
    assert!(!paths.iter().any(|p| p == "dir/old.txt"));
}

#[test]
fn test_hardlink_target_replaced_with_new_file_same_path() {
    // layer0: original.txt (file, "old") + alias.txt -> original.txt (hardlink)
    // layer1: .wh.original.txt + original.txt (file, "new")
    // Expected:
    //   original.txt → "new" (from layer1)
    //   alias.txt    → "old" (promoted real file, content from layer0 inode)
    //   alias.txt is NOT a hardlink (target was suppressed then recreated as a different inode)
    let layer0 = LayerBuilder::new()
        .add_file("original.txt", b"old", 0o644)
        .add_hardlink("alias.txt", "original.txt")
        .finish();

    let layer1 = LayerBuilder::new()
        .add_whiteout("", "original.txt")
        .add_file("original.txt", b"new", 0o644)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(paths.iter().any(|p| p == "original.txt"));
    assert!(paths.iter().any(|p| p == "alias.txt"));

    assert_eq!(
        file_contents_in_tar(&merged, "original.txt"),
        Some(b"new".to_vec())
    );
    assert_eq!(
        file_contents_in_tar(&merged, "alias.txt"),
        Some(b"old".to_vec())
    );
    assert_eq!(hardlink_target_in_tar(&merged, "alias.txt"), None);
}

#[test]
fn test_multiple_hardlinks_same_layer_target_suppressed_share_inode() {
    // alias1 and alias2 both hardlink to original.txt in the same layer.
    // original.txt is whited out in a newer layer.  The surviving pair must
    // be emitted as a hardlink group — one regular file plus one hardlink to
    // it — so that squashfs assigns them the same inode.  This preserves
    // st_nlink / st_ino semantics visible through an overlayfs upper layer.
    let layer0 = LayerBuilder::new()
        .add_file("original.txt", b"hello", 0o644)
        .add_hardlink("alias1.txt", "original.txt")
        .add_hardlink("alias2.txt", "original.txt")
        .finish();
    let layer1 = LayerBuilder::new()
        .add_whiteout("", "original.txt")
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(
        !paths.iter().any(|p| p == "original.txt"),
        "whited-out path must not appear"
    );
    assert!(
        paths.iter().any(|p| p == "alias1.txt"),
        "alias1 must survive"
    );
    assert!(
        paths.iter().any(|p| p == "alias2.txt"),
        "alias2 must survive"
    );

    let alias1_target = hardlink_target_in_tar(&merged, "alias1.txt");
    let alias2_target = hardlink_target_in_tar(&merged, "alias2.txt");

    // Exactly one must be a regular file; the other must hardlink to it.
    match (alias1_target.as_deref(), alias2_target.as_deref()) {
        (None, Some("alias1.txt")) => {
            // alias1 is the primary regular file; alias2 hardlinks to it.
            assert_eq!(
                file_contents_in_tar(&merged, "alias1.txt"),
                Some(b"hello".to_vec()),
                "primary must carry the file content"
            );
        }
        (Some("alias2.txt"), None) => {
            // alias2 is the primary regular file; alias1 hardlinks to it.
            assert_eq!(
                file_contents_in_tar(&merged, "alias2.txt"),
                Some(b"hello".to_vec()),
                "primary must carry the file content"
            );
        }
        (a1, a2) => panic!(
            "expected one regular file and one hardlink to it; \
             got alias1 target={a1:?}, alias2 target={a2:?}"
        ),
    }
}

#[test]
fn test_multiple_hardlinks_cross_layer_target_suppressed_share_inode() {
    // Same inode semantics test as above, but the two surviving hardlinks
    // are in different layers.  The whiteout appears in a layer newer than
    // both, so both links must be promoted and emitted as a hardlink group.
    let layer0 = LayerBuilder::new()
        .add_file("original.txt", b"hello", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_hardlink("alias1.txt", "original.txt")
        .finish();
    let layer2 = LayerBuilder::new()
        .add_hardlink("alias2.txt", "original.txt")
        .finish();
    let layer3 = LayerBuilder::new()
        .add_whiteout("", "original.txt")
        .finish();

    let merged = merge(vec![
        blob(layer0, 0),
        blob(layer1, 1),
        blob(layer2, 2),
        blob(layer3, 3),
    ]);
    let paths = paths_in_tar(&merged);

    assert!(
        !paths.iter().any(|p| p == "original.txt"),
        "whited-out path must not appear"
    );
    assert!(
        paths.iter().any(|p| p == "alias1.txt"),
        "alias1 must survive"
    );
    assert!(
        paths.iter().any(|p| p == "alias2.txt"),
        "alias2 must survive"
    );

    let alias1_target = hardlink_target_in_tar(&merged, "alias1.txt");
    let alias2_target = hardlink_target_in_tar(&merged, "alias2.txt");

    match (alias1_target.as_deref(), alias2_target.as_deref()) {
        (None, Some("alias1.txt")) => {
            assert_eq!(
                file_contents_in_tar(&merged, "alias1.txt"),
                Some(b"hello".to_vec()),
                "primary must carry the file content"
            );
        }
        (Some("alias2.txt"), None) => {
            assert_eq!(
                file_contents_in_tar(&merged, "alias2.txt"),
                Some(b"hello".to_vec()),
                "primary must carry the file content"
            );
        }
        (a1, a2) => panic!(
            "expected one regular file and one hardlink to it; \
             got alias1 target={a1:?}, alias2 target={a2:?}"
        ),
    }
}

// ─── Whiteout edge cases ──────────────────────────────────────────────────────

#[test]
fn test_whiteout_of_nonexistent_path_is_silent() {
    // A whiteout entry for a path that never existed in any layer must not
    // cause a panic, an error, or a spurious entry in the output.
    let layer0 = LayerBuilder::new()
        .add_file("real.txt", b"hello", 0o644)
        .finish();
    let layer1 = LayerBuilder::new().add_whiteout("", "ghost.txt").finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(
        paths.iter().any(|p| p == "real.txt"),
        "real.txt must survive"
    );
    assert!(
        !paths
            .iter()
            .any(|p| p == "ghost.txt" || p == ".wh.ghost.txt"),
        "no ghost path or raw whiteout entry must appear"
    );
}

#[test]
fn test_whiteout_of_hardlink_name_leaves_group_intact() {
    // Whiting out a hardlink *name* (not the original file) must remove only
    // that name.  The regular file and any other surviving aliases must still
    // appear with their content intact.
    let layer0 = LayerBuilder::new()
        .add_file("original.txt", b"hello", 0o644)
        .add_hardlink("alias.txt", "original.txt")
        .finish();
    let layer1 = LayerBuilder::new().add_whiteout("", "alias.txt").finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(
        paths.iter().any(|p| p == "original.txt"),
        "original must survive"
    );
    assert!(
        !paths.iter().any(|p| p == "alias.txt"),
        "whited-out link must not appear"
    );
    assert_eq!(
        file_contents_in_tar(&merged, "original.txt"),
        Some(b"hello".to_vec()),
        "original content must be intact"
    );
}

#[test]
fn test_opaque_whiteout_preserves_directory_entry_itself() {
    // The opaque whiteout suppresses *children* from older layers, but the
    // directory entry that carries the opaque whiteout must itself survive.
    // The existing test_opaque_whiteout checks child suppression; this test
    // makes the directory-survival assertion explicit.
    let layer0 = LayerBuilder::new()
        .add_dir("mydir")
        .add_file("mydir/old.txt", b"old", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_dir("mydir")
        .add_opaque_whiteout("mydir")
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(
        paths.iter().any(|p| p == "mydir"),
        "directory entry itself must survive the opaque whiteout"
    );
    assert!(
        !paths.iter().any(|p| p == "mydir/old.txt"),
        "children from older layers must be suppressed"
    );
}

// ─── PAX header round-trip ────────────────────────────────────────────────────

#[test]
fn test_long_path_pax_preserved() {
    // A file whose path exceeds USTAR's 100-byte name field limit must
    // round-trip correctly through the merge pipeline.
    let long_path: String = "a".repeat(110);
    let layer0 = LayerBuilder::new()
        .add_file(&long_path, b"hello", 0o644)
        .finish();

    let merged = merge(vec![blob(layer0, 0)]);

    assert!(
        paths_in_tar(&merged).iter().any(|p| p == &long_path),
        "long file path must survive the merge"
    );
    assert_eq!(
        file_contents_in_tar(&merged, &long_path),
        Some(b"hello".to_vec()),
        "content must be intact for the long-path file"
    );
}

#[test]
fn test_long_promoted_hardlink_target_pax_preserved() {
    // When a promotion group's primary link path exceeds 100 bytes, the
    // non-primary members must reference it via a PAX `linkpath` extension
    // (exercising the branch added to clone_as_hardlink_to).
    let long_primary: String = "a".repeat(110);
    let long_secondary: String = "b".repeat(110);

    let layer0 = LayerBuilder::new()
        .add_file("original.txt", b"hello", 0o644)
        .add_hardlink(&long_primary, "original.txt")
        .add_hardlink(&long_secondary, "original.txt")
        .finish();
    let layer1 = LayerBuilder::new()
        .add_whiteout("", "original.txt")
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(
        !paths.iter().any(|p| p == "original.txt"),
        "whited-out path must not appear"
    );
    assert!(
        paths.iter().any(|p| p == &long_primary),
        "long primary must survive"
    );
    assert!(
        paths.iter().any(|p| p == &long_secondary),
        "long secondary must survive"
    );

    let primary_target = hardlink_target_in_tar(&merged, &long_primary);
    let secondary_target = hardlink_target_in_tar(&merged, &long_secondary);

    // Exactly one of the two must be a regular file; the other must be a
    // hardlink whose target is the full (>100 byte) path of the primary.
    match (primary_target.as_deref(), secondary_target.as_deref()) {
        (None, Some(t)) => {
            assert_eq!(
                t,
                long_primary.as_str(),
                "hardlink target must be the full long path, not truncated"
            );
            assert_eq!(
                file_contents_in_tar(&merged, &long_primary),
                Some(b"hello".to_vec())
            );
        }
        (Some(t), None) => {
            assert_eq!(
                t,
                long_secondary.as_str(),
                "hardlink target must be the full long path, not truncated"
            );
            assert_eq!(
                file_contents_in_tar(&merged, &long_secondary),
                Some(b"hello".to_vec())
            );
        }
        (a, b) => panic!(
            "expected one regular file and one hardlink to it; \
             got long_primary target={a:?}, long_secondary target={b:?}"
        ),
    }
}

// ─── Path normalisation ───────────────────────────────────────────────────────

#[test]
fn test_dot_slash_prefix_deduplication() {
    // Many tools (docker save, tar c .) emit ./-prefixed paths.  An entry at
    // `./foo.txt` and an entry at `foo.txt` refer to the same filesystem path
    // and must be deduplicated: only the newer layer's version should appear,
    // and it must appear exactly once.
    let layer0 = LayerBuilder::new()
        .add_file("foo.txt", b"old", 0o644)
        .finish();
    // layer1 uses the ./  prefix as docker save would produce.
    let layer1 = LayerBuilder::new()
        .add_file_dotslash("./foo.txt", b"new", 0o644)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    let count = paths.iter().filter(|p| p.as_str() == "foo.txt").count();
    assert_eq!(
        count, 1,
        "foo.txt must appear exactly once despite mixed ./ prefixing"
    );
    assert_eq!(
        file_contents_in_tar(&merged, "foo.txt"),
        Some(b"new".to_vec()),
        "newer layer's content must win"
    );
}

// ─── Content fidelity ────────────────────────────────────────────────────────

#[test]
fn test_zero_byte_file_promotion() {
    // A zero-byte regular file must survive promotion (i.e. note_suppressed_file
    // must buffer it correctly and write_to_tar must emit it with zero content).
    let layer0 = LayerBuilder::new()
        .add_file("original.txt", b"", 0o644)
        .add_hardlink("alias.txt", "original.txt")
        .finish();
    let layer1 = LayerBuilder::new()
        .add_whiteout("", "original.txt")
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);
    let paths = paths_in_tar(&merged);

    assert!(
        !paths.iter().any(|p| p == "original.txt"),
        "whited-out path must not appear"
    );
    assert!(
        paths.iter().any(|p| p == "alias.txt"),
        "alias must be promoted"
    );
    assert_eq!(
        file_contents_in_tar(&merged, "alias.txt"),
        Some(vec![]),
        "zero-byte content must round-trip correctly"
    );
    assert_eq!(
        hardlink_target_in_tar(&merged, "alias.txt"),
        None,
        "promoted entry must be a regular file, not a hardlink"
    );
}

// ─── Metadata preservation ───────────────────────────────────────────────────

#[test]
fn test_file_mode_bits_preserved() {
    // Permission bits set in a layer must survive the merge unchanged.
    let layer0 = LayerBuilder::new()
        .add_file("script.sh", b"#!/bin/sh", 0o755)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_file("secret.key", b"key_data", 0o600)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);

    assert_eq!(
        file_mode_in_tar(&merged, "script.sh"),
        Some(0o755),
        "executable mode must be preserved"
    );
    assert_eq!(
        file_mode_in_tar(&merged, "secret.key"),
        Some(0o600),
        "restrictive mode must be preserved"
    );
}

#[test]
fn test_file_mode_override_in_newer_layer() {
    // When a newer layer replaces a file, the newer layer's mode wins along
    // with its content.
    let layer0 = LayerBuilder::new()
        .add_file("script.sh", b"#!/bin/sh\nold", 0o644)
        .finish();
    let layer1 = LayerBuilder::new()
        .add_file("script.sh", b"#!/bin/sh\nnew", 0o755)
        .finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);

    assert_eq!(
        file_mode_in_tar(&merged, "script.sh"),
        Some(0o755),
        "newer layer's mode must win"
    );
    assert_eq!(
        file_contents_in_tar(&merged, "script.sh"),
        Some(b"#!/bin/sh\nnew".to_vec()),
        "newer layer's content must win"
    );
}

// ─── Special entry types ──────────────────────────────────────────────────────

#[test]
fn test_fifo_passes_through() {
    // The merge algorithm must be transparent to FIFO (named pipe) entries —
    // they carry no content and must be written to the output tar with their
    // entry type intact.
    let layer0 = LayerBuilder::new().add_fifo("my_pipe").finish();

    let merged = merge(vec![blob(layer0, 0)]);

    assert!(
        paths_in_tar(&merged).iter().any(|p| p == "my_pipe"),
        "FIFO must pass through"
    );
    assert_eq!(
        entry_type_in_tar(&merged, "my_pipe"),
        Some(tar::EntryType::Fifo),
        "entry type must remain Fifo after merge"
    );
}

#[test]
fn test_fifo_suppressed_by_whiteout() {
    // FIFOs must be subject to the same whiteout suppression as regular files.
    let layer0 = LayerBuilder::new().add_fifo("my_pipe").finish();
    let layer1 = LayerBuilder::new().add_whiteout("", "my_pipe").finish();

    let merged = merge(vec![blob(layer0, 0), blob(layer1, 1)]);

    assert!(
        !paths_in_tar(&merged).iter().any(|p| p == "my_pipe"),
        "whited-out FIFO must not appear in the output"
    );
}
