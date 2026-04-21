//! Unit tests for image.rs: manifest loading, layer resolution, and media-type
//! detection.  No tar merging or mksquashfs involvement — these tests exercise
//! the parsing and path-resolution layer in isolation.

use std::fs;

use ocirender::image::{detect_media_type, load_manifest, resolve_layers};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Write a minimal OCI image layout (index.json → manifest blob → layer blobs)
/// into a temp directory.  Returns the temp dir (kept alive by the caller) and
/// the path to each written layer blob.
struct OciLayout {
    dir: TempDir,
}

impl OciLayout {
    /// `layers`: list of `(compressed_bytes, media_type)`.
    fn new(layers: &[(&[u8], &str)]) -> Self {
        let dir = TempDir::new().unwrap();
        let blobs = dir.path().join("blobs").join("sha256");
        fs::create_dir_all(&blobs).unwrap();

        let manifest_layers: Vec<serde_json::Value> = layers
            .iter()
            .map(|(data, media_type)| {
                let digest = sha256_hex(data);
                fs::write(blobs.join(&digest), data).unwrap();
                serde_json::json!({
                    "mediaType": media_type,
                    "digest": format!("sha256:{digest}"),
                    "size": data.len(),
                })
            })
            .collect();

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "layers": manifest_layers,
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest = sha256_hex(&manifest_bytes);
        fs::write(blobs.join(&manifest_digest), &manifest_bytes).unwrap();

        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{"digest": format!("sha256:{manifest_digest}"), "size": manifest_bytes.len()}],
        });
        fs::write(
            dir.path().join("index.json"),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();

        Self { dir }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_encode(&Sha256::digest(data))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

/// Write a minimal Docker save layout (manifest.json only) into a temp dir.
/// `layers`: list of `(data, media_type)`.  Returns `(TempDir, layer_paths)`.
fn docker_save_layout(layers: &[(&[u8], &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    let blobs = dir.path().join("blobs").join("sha256");
    fs::create_dir_all(&blobs).unwrap();

    let layer_paths: Vec<String> = layers
        .iter()
        .map(|(data, _)| {
            let digest = sha256_hex(data);
            fs::write(blobs.join(&digest), data).unwrap();
            format!("blobs/sha256/{digest}")
        })
        .collect();

    let layer_sources: serde_json::Map<String, serde_json::Value> = layers
        .iter()
        .map(|(data, media_type)| {
            let digest = sha256_hex(data);
            let key = format!("sha256:{digest}");
            (key, serde_json::json!({"mediaType": media_type}))
        })
        .collect();

    let manifest = serde_json::json!([{
        "Config": "blobs/sha256/fakeconfigdigest",
        "Layers": layer_paths,
        "LayerSources": layer_sources,
    }]);
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    dir
}

// ── detect_media_type ─────────────────────────────────────────────────────────

#[test]
fn detect_gzip() {
    let f = tempfile::NamedTempFile::new().unwrap();
    fs::write(f.path(), &[0x1f, 0x8b, 0x00, 0x00]).unwrap();
    assert_eq!(
        detect_media_type(f.path()).unwrap(),
        "application/vnd.oci.image.layer.v1.tar+gzip"
    );
}

#[test]
fn detect_zstd() {
    let f = tempfile::NamedTempFile::new().unwrap();
    fs::write(f.path(), &[0x28, 0xb5, 0x2f, 0xfd]).unwrap();
    assert_eq!(
        detect_media_type(f.path()).unwrap(),
        "application/vnd.oci.image.layer.v1.tar+zstd"
    );
}

#[test]
fn detect_bzip2() {
    let f = tempfile::NamedTempFile::new().unwrap();
    fs::write(f.path(), &[0x42, 0x5a, 0x68, 0x00]).unwrap();
    assert_eq!(
        detect_media_type(f.path()).unwrap(),
        "application/vnd.oci.image.layer.v1.tar+bzip2"
    );
}

#[test]
fn detect_xz() {
    let f = tempfile::NamedTempFile::new().unwrap();
    fs::write(f.path(), &[0xfd, 0x37, 0x7a, 0x58]).unwrap();
    assert_eq!(
        detect_media_type(f.path()).unwrap(),
        "application/vnd.oci.image.layer.v1.tar+xz"
    );
}

#[test]
fn detect_uncompressed_fallback() {
    let f = tempfile::NamedTempFile::new().unwrap();
    // Plain tar magic bytes — not any of the compressed formats.
    fs::write(f.path(), &[0x75, 0x73, 0x74, 0x61]).unwrap();
    assert_eq!(
        detect_media_type(f.path()).unwrap(),
        "application/vnd.oci.image.layer.v1.tar"
    );
}

#[test]
fn detect_media_type_file_not_found() {
    let result = detect_media_type(std::path::Path::new("/nonexistent/path/layer.tar"));
    assert!(result.is_err(), "missing file must return an error");
}

// ── load_manifest: OCI layout ─────────────────────────────────────────────────

#[test]
fn load_manifest_oci_layout_single_layer() {
    let layout = OciLayout::new(&[(&[0u8; 4], "application/vnd.oci.image.layer.v1.tar")]);
    let manifest = load_manifest(layout.path()).unwrap();
    assert_eq!(manifest.layers.len(), 1);
    assert_eq!(
        manifest.layers[0].media_type,
        "application/vnd.oci.image.layer.v1.tar"
    );
}

#[test]
fn load_manifest_oci_layout_multiple_layers_preserves_order() {
    let layout = OciLayout::new(&[
        (
            &[0x1f, 0x8b, 0, 0],
            "application/vnd.oci.image.layer.v1.tar+gzip",
        ),
        (
            &[0x28, 0xb5, 0x2f, 0xfd],
            "application/vnd.oci.image.layer.v1.tar+zstd",
        ),
        (&[0u8; 4], "application/vnd.oci.image.layer.v1.tar"),
    ]);
    let manifest = load_manifest(layout.path()).unwrap();
    assert_eq!(manifest.layers.len(), 3);
    assert!(manifest.layers[0].media_type.ends_with("+gzip"));
    assert!(manifest.layers[1].media_type.ends_with("+zstd"));
    assert_eq!(
        manifest.layers[2].media_type,
        "application/vnd.oci.image.layer.v1.tar"
    );
}

#[test]
fn load_manifest_index_json_preferred_over_manifest_json() {
    // When both files are present, index.json must win.  The index.json
    // points at a manifest with a gzip layer; the manifest.json declares a
    // zstd layer.  The parsed result must reflect the gzip layer.
    let layout = OciLayout::new(&[(
        &[0x1f, 0x8b, 0, 0],
        "application/vnd.oci.image.layer.v1.tar+gzip",
    )]);

    // Write a manifest.json alongside index.json with a different media_type.
    let decoy_layer_data = [0x28u8, 0xb5, 0x2f, 0xfd];
    let decoy_digest = sha256_hex(&decoy_layer_data);
    let blobs = layout.path().join("blobs").join("sha256");
    fs::write(blobs.join(&decoy_digest), &decoy_layer_data).unwrap();
    let decoy_manifest = serde_json::json!([{
        "Config": "irrelevant",
        "Layers": [format!("blobs/sha256/{decoy_digest}")],
        "LayerSources": {
            format!("sha256:{decoy_digest}"): {"mediaType": "application/vnd.oci.image.layer.v1.tar+zstd"}
        },
    }]);
    fs::write(
        layout.path().join("manifest.json"),
        serde_json::to_vec(&decoy_manifest).unwrap(),
    )
    .unwrap();

    let manifest = load_manifest(layout.path()).unwrap();
    assert_eq!(manifest.layers.len(), 1);
    assert!(
        manifest.layers[0].media_type.ends_with("+gzip"),
        "index.json must take precedence; got media_type = {}",
        manifest.layers[0].media_type
    );
}

#[test]
fn load_manifest_no_metadata_files_returns_error() {
    let dir = TempDir::new().unwrap();
    let result = load_manifest(dir.path());
    assert!(
        result.is_err(),
        "missing index.json and manifest.json must be an error"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("no index.json or manifest.json"),
        "error message must identify the missing files"
    );
}

#[test]
fn load_manifest_empty_manifests_array_returns_error() {
    let dir = TempDir::new().unwrap();
    let blobs = dir.path().join("blobs").join("sha256");
    fs::create_dir_all(&blobs).unwrap();

    let manifest_bytes =
        serde_json::to_vec(&serde_json::json!({"schemaVersion":2,"layers":[]})).unwrap();
    let manifest_digest = sha256_hex(&manifest_bytes);
    fs::write(blobs.join(&manifest_digest), &manifest_bytes).unwrap();

    // index.json with an empty manifests array.
    let index = serde_json::json!({"schemaVersion": 2, "manifests": []});
    fs::write(
        dir.path().join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();

    let result = load_manifest(dir.path());
    assert!(result.is_err(), "empty manifests array must be an error");
}

#[test]
fn load_manifest_nested_index_followed_to_single_image_manifest() {
    // Reproduces the containerd/Docker Desktop two-level layout:
    //   index.json → platform index (mediaType: ...image.index...)
    //              → single-image manifest (mediaType: ...image.manifest...)
    //              → layers
    let dir = TempDir::new().unwrap();
    let blobs = dir.path().join("blobs").join("sha256");
    fs::create_dir_all(&blobs).unwrap();

    // Write the layer blob.
    let layer_data = [0x1f_u8, 0x8b, 0x00, 0x00]; // gzip magic
    let layer_digest = sha256_hex(&layer_data);
    fs::write(blobs.join(&layer_digest), &layer_data).unwrap();

    // Write the inner single-image manifest.
    let inner_manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": format!("sha256:{layer_digest}"),
            "size": layer_data.len(),
        }],
    });
    let inner_bytes = serde_json::to_vec(&inner_manifest).unwrap();
    let inner_digest = sha256_hex(&inner_bytes);
    fs::write(blobs.join(&inner_digest), &inner_bytes).unwrap();

    // Write the platform index that points at the inner manifest.
    let platform_index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{inner_digest}"),
            "size": inner_bytes.len(),
            "platform": {"os": "linux", "architecture": "amd64"},
        }],
    });
    let platform_bytes = serde_json::to_vec(&platform_index).unwrap();
    let platform_digest = sha256_hex(&platform_bytes);
    fs::write(blobs.join(&platform_digest), &platform_bytes).unwrap();

    // Write index.json pointing at the platform index.
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "digest": format!("sha256:{platform_digest}"),
            "size": platform_bytes.len(),
        }],
    });
    fs::write(
        dir.path().join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();

    let manifest = load_manifest(dir.path()).unwrap();
    assert_eq!(
        manifest.layers.len(),
        1,
        "must find the layer through two levels of indirection"
    );
    assert!(
        manifest.layers[0].media_type.ends_with("+gzip"),
        "layer media type must be preserved through nested index traversal; got {:?}",
        manifest.layers[0].media_type
    );
}

#[test]
fn load_manifest_nested_index_with_no_image_manifest_returns_error() {
    // A nested index that contains only further index entries (no single-image
    // manifest) must return a clear error rather than panicking or producing
    // an empty layer list.
    let dir = TempDir::new().unwrap();
    let blobs = dir.path().join("blobs").join("sha256");
    fs::create_dir_all(&blobs).unwrap();

    // Platform index containing only another index entry, no manifest entry.
    let platform_index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size": 0,
        }],
    });
    let platform_bytes = serde_json::to_vec(&platform_index).unwrap();
    let platform_digest = sha256_hex(&platform_bytes);
    fs::write(blobs.join(&platform_digest), &platform_bytes).unwrap();

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "digest": format!("sha256:{platform_digest}"),
            "size": platform_bytes.len(),
        }],
    });
    fs::write(
        dir.path().join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();

    let result = load_manifest(dir.path());
    assert!(
        result.is_err(),
        "all-index nested structure must return an error"
    );
    let err = result.unwrap_err();
    let chain = format!("{err:#}");
    assert!(
        chain.contains("no single-image manifest entry"),
        "error chain must explain why traversal failed; got: {chain}"
    );
}

#[test]
fn load_manifest_unsupported_digest_algorithm_returns_error() {
    let dir = TempDir::new().unwrap();
    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{"digest": "blake3:abcdef", "size": 0}],
    });
    fs::write(
        dir.path().join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();

    let result = load_manifest(dir.path());
    assert!(
        result.is_err(),
        "unsupported digest algorithm must be an error"
    );
}

// ── load_manifest: Docker save ────────────────────────────────────────────────

#[test]
fn load_manifest_docker_save_with_layer_sources() {
    let dir = docker_save_layout(&[
        (
            &[0x1f, 0x8b, 0, 0],
            "application/vnd.oci.image.layer.v1.tar+gzip",
        ),
        (&[0u8; 4], "application/vnd.oci.image.layer.v1.tar"),
    ]);
    let manifest = load_manifest(dir.path()).unwrap();
    assert_eq!(manifest.layers.len(), 2);
    assert!(manifest.layers[0].media_type.ends_with("+gzip"));
    assert_eq!(
        manifest.layers[1].media_type,
        "application/vnd.oci.image.layer.v1.tar"
    );
}

#[test]
fn load_manifest_docker_save_without_layer_sources_falls_back_to_magic() {
    // A Docker save layout without LayerSources must fall back to magic-byte
    // detection in resolve_layers.  We verify the manifest parses successfully
    // and that the media_type field is empty (the signal for the fallback).
    let dir = TempDir::new().unwrap();
    let blobs = dir.path().join("blobs").join("sha256");
    fs::create_dir_all(&blobs).unwrap();

    let data = [0x1f_u8, 0x8b, 0x00, 0x00]; // gzip magic
    let digest = sha256_hex(&data);
    fs::write(blobs.join(&digest), &data).unwrap();

    let manifest = serde_json::json!([{
        "Config": "blobs/sha256/fakecfg",
        "Layers": [format!("blobs/sha256/{digest}")],
        // No LayerSources key.
    }]);
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let manifest = load_manifest(dir.path()).unwrap();
    assert_eq!(manifest.layers.len(), 1);
    // Empty media_type is the signal that resolve_layers must run detection.
    assert!(
        manifest.layers[0].media_type.is_empty(),
        "absent LayerSources must produce empty media_type; got {:?}",
        manifest.layers[0].media_type
    );
}

#[test]
fn load_manifest_docker_save_empty_array_returns_error() {
    let dir = TempDir::new().unwrap();
    let manifest = serde_json::json!([]);
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let result = load_manifest(dir.path());
    assert!(
        result.is_err(),
        "empty manifest.json array must be an error"
    );
}

// ── resolve_layers ────────────────────────────────────────────────────────────

#[test]
fn resolve_layers_assigns_correct_indices() {
    let layout = OciLayout::new(&[
        (&[0u8; 4], "application/vnd.oci.image.layer.v1.tar"),
        (&[1u8; 4], "application/vnd.oci.image.layer.v1.tar"),
        (&[2u8; 4], "application/vnd.oci.image.layer.v1.tar"),
    ]);
    let manifest = load_manifest(layout.path()).unwrap();
    let layers = resolve_layers(layout.path(), &manifest).unwrap();
    assert_eq!(layers.len(), 3);
    for (i, layer) in layers.iter().enumerate() {
        assert_eq!(layer.index, i, "layer at position {i} must have index {i}");
    }
}

#[test]
fn resolve_layers_missing_blob_returns_error() {
    let layout = OciLayout::new(&[(&[0u8; 4], "application/vnd.oci.image.layer.v1.tar")]);
    let mut manifest = load_manifest(layout.path()).unwrap();

    // Corrupt the digest so the blob path won't exist.
    manifest.layers[0].digest = "sha256:deadbeef".repeat(4);

    let result = resolve_layers(layout.path(), &manifest);
    assert!(result.is_err(), "missing blob file must be an error");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("layer blob not found"),
        "error must identify the missing blob"
    );
}

#[test]
fn resolve_layers_falls_back_to_magic_when_media_type_empty() {
    // Simulates the Docker-save-without-LayerSources path: media_type is empty
    // in the manifest, so resolve_layers must detect it from the blob's magic bytes.
    let dir = TempDir::new().unwrap();
    let blobs = dir.path().join("blobs").join("sha256");
    fs::create_dir_all(&blobs).unwrap();

    let data = [0x1f_u8, 0x8b, 0x00, 0x00]; // gzip magic
    let digest = sha256_hex(&data);
    fs::write(blobs.join(&digest), &data).unwrap();

    let manifest = serde_json::json!([{
        "Config": "blobs/sha256/fakecfg",
        "Layers": [format!("blobs/sha256/{digest}")],
    }]);
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let manifest = load_manifest(dir.path()).unwrap();
    let layers = resolve_layers(dir.path(), &manifest).unwrap();

    assert_eq!(layers.len(), 1);
    assert_eq!(
        layers[0].media_type, "application/vnd.oci.image.layer.v1.tar+gzip",
        "empty media_type must be resolved to gzip via magic byte detection"
    );
}

#[test]
fn resolve_layers_blob_stored_as_directory_with_order_index() {
    // Regression: some OCI downloaders store each blob as a directory
    // <hash>/<manifest-order> rather than a plain file named <hash>.
    // resolve_layers must detect this and append the index as a filename
    // component.  (This duplicates the regression test in regression.rs but
    // exercises it through the public image API directly.)
    let dir = TempDir::new().unwrap();
    let blobs = dir.path().join("blobs").join("sha256");
    fs::create_dir_all(&blobs).unwrap();

    let digest0 = "a".repeat(64);
    let blob_dir = blobs.join(&digest0);
    fs::create_dir_all(&blob_dir).unwrap();
    fs::write(blob_dir.join("0"), b"layer-content").unwrap();

    let manifest_bytes = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": format!("sha256:{digest0}"), "size": 13}],
    }))
    .unwrap();
    let manifest_digest = sha256_hex(&manifest_bytes);
    fs::write(blobs.join(&manifest_digest), &manifest_bytes).unwrap();

    let index = serde_json::json!({"schemaVersion":2,"manifests":[{"digest":format!("sha256:{manifest_digest}"),"size":manifest_bytes.len()}]});
    fs::write(
        dir.path().join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();

    let manifest = load_manifest(dir.path()).unwrap();
    let layers = resolve_layers(dir.path(), &manifest).unwrap();

    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].path, blob_dir.join("0"));
}

#[test]
fn resolve_layers_paths_point_to_existing_files() {
    let layout = OciLayout::new(&[
        (&[0u8; 4], "application/vnd.oci.image.layer.v1.tar"),
        (&[1u8; 4], "application/vnd.oci.image.layer.v1.tar+gzip"),
    ]);
    let manifest = load_manifest(layout.path()).unwrap();
    let layers = resolve_layers(layout.path(), &manifest).unwrap();

    for layer in &layers {
        assert!(
            layer.path.exists(),
            "resolved path {} must exist on disk",
            layer.path.display()
        );
        assert!(
            layer.path.is_file(),
            "resolved path {} must be a regular file",
            layer.path.display()
        );
    }
}

// ── oci-spec compatibility ────────────────────────────────────────────────────
//
// These tests verify that index.json files produced by the `oci-spec` crate
// (as used in production to assemble the download directory) are parsed
// correctly by load_manifest + resolve_layers.  A field name mismatch between
// oci-spec's serde output and our OciIndex/OciDescriptor structs would cause a
// silent parse failure or a missing-field error.

#[cfg(test)]
mod oci_spec_compat {
    use super::*;
    use oci_spec::image::{
        DescriptorBuilder, ImageIndexBuilder, ImageManifestBuilder, MediaType, Sha256Digest,
    };
    use std::str::FromStr;

    /// Reproduce the exact index.json assembly used in production and verify
    /// that load_manifest parses it and resolve_layers finds the layer blob.
    #[test]
    fn load_manifest_parses_oci_spec_generated_index() {
        let dir = TempDir::new().unwrap();
        let blobs = dir.path().join("blobs").join("sha256");
        fs::create_dir_all(&blobs).unwrap();

        // Write a minimal layer blob.
        let layer_data = [0x1f_u8, 0x8b, 0x00, 0x00]; // gzip magic
        let layer_digest_hex = sha256_hex(&layer_data);
        fs::write(blobs.join(&layer_digest_hex), &layer_data).unwrap();

        // Build a manifest blob using oci-spec, mirroring what the downloader does.
        let layer_desc = DescriptorBuilder::default()
            .media_type(MediaType::ImageLayerGzip)
            .size(layer_data.len() as u64)
            .digest(Sha256Digest::from_str(&layer_digest_hex).unwrap())
            .build()
            .unwrap();

        let manifest = ImageManifestBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageManifest)
            .layers(vec![layer_desc])
            .config(
                DescriptorBuilder::default()
                    .media_type(MediaType::ImageConfig)
                    .size(2u64)
                    .digest(Sha256Digest::from_str(&sha256_hex(b"{}")).unwrap())
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest_hex = sha256_hex(&manifest_bytes);
        fs::write(blobs.join(&manifest_digest_hex), &manifest_bytes).unwrap();

        // Write a config blob so resolve_layers doesn't trip over a missing file.
        fs::write(blobs.join(sha256_hex(b"{}")), b"{}").unwrap();

        // Build index.json exactly as the production downloader does.
        let index_descriptor = DescriptorBuilder::default()
            .media_type(MediaType::ImageManifest)
            .size(manifest_bytes.len() as u64)
            .digest(Sha256Digest::from_str(&manifest_digest_hex).unwrap())
            .build()
            .unwrap();

        let index = ImageIndexBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageIndex)
            .manifests(vec![index_descriptor])
            .build()
            .unwrap();

        fs::write(
            dir.path().join("index.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();

        // This must parse without error.
        let manifest = load_manifest(dir.path())
            .expect("load_manifest must parse an oci-spec-generated index.json");

        assert_eq!(manifest.layers.len(), 1, "must find exactly one layer");
        assert!(
            manifest.layers[0].media_type.ends_with("+gzip"),
            "layer media type must be preserved; got {:?}",
            manifest.layers[0].media_type
        );

        // resolve_layers must also succeed and produce a blob pointing at the
        // correct file.
        let layers = resolve_layers(dir.path(), &manifest).expect("resolve_layers must succeed");
        assert_eq!(layers.len(), 1);
        assert!(
            layers[0].path.exists(),
            "resolved layer path must exist on disk"
        );
        assert_eq!(layers[0].index, 0);
    }
}
