//! Regression tests, one per production bug found during verify runs.
//! Each test is named after the bug it guards against and has a comment
//! explaining the root cause and how it was discovered.

use std::fs;
use std::io::Cursor;
use tar::{Builder, EntryType, Header};

#[path = "helpers/mod.rs"]
mod helpers;
use helpers::{LayerBuilder, blob, merge, paths_in_tar};
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

/// resolve_layers` constructed layer blob paths as `<image_dir>/blobs/sha256/<hash>` and
/// expected a plain file there. The some OCI downloaders instead store each blob as a directory
/// `<image_dir>/blobs/sha256/<hash>/` containing a file named after the layer's manifest order
/// index (e.g. `0` for the first layer). This caused `File::open` to fail with EISDIR.
///
/// The manifest-order index is used as the filename to handle the edge case of a manifest that
/// lists the same digest more than once.
#[test]
fn regress_layer_blob_stored_as_directory_with_order_index() {
    let dir = tempfile::tempdir().unwrap();
    let blobs_sha256 = dir.path().join("blobs").join("sha256");
    fs::create_dir_all(&blobs_sha256).unwrap();

    // Two layers, each stored as <hash>/<manifest-order>.
    let digest0 = "c".repeat(64);
    let digest1 = "d".repeat(64);
    let dir0 = blobs_sha256.join(&digest0);
    let dir1 = blobs_sha256.join(&digest1);
    fs::create_dir_all(&dir0).unwrap();
    fs::create_dir_all(&dir1).unwrap();
    fs::write(dir0.join("0"), b"layer0-content").unwrap();
    fs::write(dir1.join("1"), b"layer1-content").unwrap();

    // Write the manifest blob that the index points to.
    let manifest_digest = "e".repeat(64);
    let manifest_json = serde_json::json!({
        "schemaVersion": 2,
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": format!("sha256:{digest0}"),
                "size": 14
            },
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": format!("sha256:{digest1}"),
                "size": 14
            }
        ]
    });
    fs::write(
        blobs_sha256.join(&manifest_digest),
        manifest_json.to_string(),
    )
    .unwrap();

    // Write index.json as a proper OCI image index pointing at the manifest blob.
    let index_json = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{manifest_digest}"),
                "size": 0
            }
        ]
    });
    fs::write(dir.path().join("index.json"), index_json.to_string()).unwrap();

    let manifest = ocirender::image::load_manifest(dir.path()).unwrap();
    let layers = ocirender::image::resolve_layers(dir.path(), &manifest)
        .expect("resolve_layers must find blobs stored as <hash>/<manifest-order> directories");

    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].path, dir0.join("0"));
    assert_eq!(layers[1].path, dir1.join("1"));
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

/// Bug: `load_manifest_blob` only recognised OCI media type strings
/// (`application/vnd.oci.image.index.v1+json` and `...manifest.v1+json`).
/// `crane pull --format=oci` produces `index.json` entries typed as Docker
/// distribution media types (`manifest.list.v2+json` / `manifest.v2+json`).
/// The unrecognised type fell through to "direct single-image manifest" and
/// tried to deserialise a manifest list as an `OciManifest`, failing with
/// "missing field `layers`".
#[test]
fn regress_docker_manifest_list_media_types() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = dir.path().join("blobs").join("sha256");
    fs::create_dir_all(&blobs).unwrap();

    // A minimal layer blob.
    let layer_digest = "a".repeat(64);
    let layer_bytes = LayerBuilder::new()
        .add_file("etc/os-release", b"data", 0o644)
        .finish();
    fs::write(blobs.join(&layer_digest), &layer_bytes).unwrap();

    // Per-platform manifest using the Docker v2 manifest media type.
    let inner_digest = "b".repeat(64);
    let inner_manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": format!("sha256:{layer_digest}"),
            "size": layer_bytes.len()
        }]
    });
    fs::write(blobs.join(&inner_digest), inner_manifest.to_string()).unwrap();

    // Manifest list using the Docker manifest list media type.
    let list_digest = "c".repeat(64);
    let manifest_list = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
        "manifests": [{
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "digest": format!("sha256:{inner_digest}"),
            "size": inner_manifest.to_string().len()
        }]
    });
    fs::write(blobs.join(&list_digest), manifest_list.to_string()).unwrap();

    // index.json points at the manifest list with the Docker media type.
    let index_json = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
            "digest": format!("sha256:{list_digest}"),
            "size": manifest_list.to_string().len()
        }]
    });
    fs::write(dir.path().join("index.json"), index_json.to_string()).unwrap();

    let manifest = ocirender::image::load_manifest(dir.path())
        .expect("must handle Docker distribution manifest list media types");
    let layers = ocirender::image::resolve_layers(dir.path(), &manifest)
        .expect("must resolve layer from Docker-typed manifest");
    assert_eq!(layers.len(), 1, "exactly one layer must be resolved");
}
