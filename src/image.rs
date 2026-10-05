//! OCI image layout parsing: reads `index.json` or `manifest.json` and
//! resolves layer descriptors to blob file paths on disk.
//!
//! Three image layout formats are supported:
//!
//! - **OCI image layout** (`index.json`): the standard format produced by
//!   `skopeo copy docker://... oci:<dir>` and containerd. Layer blobs live
//!   under `blobs/sha256/<hex-digest>`.
//! - **Docker save layout** (`manifest.json` holding an array): produced by
//!   `docker save <image> | tar -x`. Layer paths are stored directly in the
//!   manifest as relative paths like `blobs/sha256/<hex>`.
//! - **Bare image manifest** (`manifest.json` holding an object): a single
//!   OCI or Docker schema 2 image manifest, as served by a registry, with its
//!   layer blobs under `blobs/sha256/<hex-digest>`.
//!
//! The primary entry points are [`load_manifest`], which parses whichever
//! format is present and returns a normalised [`OciManifest`], and
//! [`resolve_layers`], which turns the manifest's layer descriptors into
//! [`LayerBlob`] values ready for the merge pipeline.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Top-level OCI image index (`index.json`).
///
/// May point directly at a single-image manifest, or at a nested platform
/// index for multi-platform images. `load_manifest_blob` handles both cases.
#[derive(Debug, Deserialize)]
pub struct OciIndex {
    pub manifests: Vec<OciDescriptor>,
}

/// A content-addressed reference to a blob, as it appears in an OCI index or
/// manifest.
#[derive(Debug, Deserialize)]
pub struct OciDescriptor {
    /// Content digest in `algorithm:hex` form, e.g. `sha256:abc123...`.
    pub digest: String,
    /// MIME type of the referenced blob. May be empty in older Docker save
    /// layouts that omit `LayerSources`; [`resolve_layers`] falls back to
    /// magic-byte detection in that case.
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
}

/// A normalised OCI image manifest containing an ordered list of layer
/// descriptors.
#[derive(Debug, Deserialize)]
pub struct OciManifest {
    /// Layer descriptors in stack order, oldest (base) first.
    pub layers: Vec<OciDescriptor>,
}

/// A single layer blob resolved to a file path on disk, ready for the merge
/// pipeline.
#[derive(Debug)]
pub struct LayerBlob {
    /// Absolute path to the compressed (or uncompressed) layer tar on disk.
    pub path: PathBuf,
    /// OCI media type string, used to select the correct decompressor.
    pub media_type: String,
    /// Zero-based position of this layer in the manifest's layer list.
    /// Layer 0 is the oldest (base) layer; the highest index is the newest.
    pub index: usize,
}

const MEDIA_TYPE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MEDIA_TYPE_INDEX: &str = "application/vnd.oci.image.index.v1+json";

// Docker distribution equivalents — same semantics, different type strings.
const MEDIA_TYPE_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const MEDIA_TYPE_DOCKER_MANIFEST_LIST: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";

fn is_index_media_type(mt: &str) -> bool {
    mt == MEDIA_TYPE_INDEX || mt == MEDIA_TYPE_DOCKER_MANIFEST_LIST
}

fn is_manifest_media_type(mt: &str) -> bool {
    mt == MEDIA_TYPE_MANIFEST || mt == MEDIA_TYPE_DOCKER_MANIFEST
}

/// Detect the compression format of a layer blob by inspecting its magic bytes.
///
/// Used as a fallback when the manifest does not carry a `mediaType` for a
/// layer — for example, minimal Docker save layouts that omit `LayerSources`.
/// Returns a static OCI media type string.
pub fn detect_media_type(path: &Path) -> Result<&'static str> {
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map_err(|e| Error::image_layout(path, "cannot read layer blob", e))?;
    Ok(match magic {
        [0x1f, 0x8b, ..] => "application/vnd.oci.image.layer.v1.tar+gzip",
        [0x28, 0xb5, 0x2f, 0xfd] => "application/vnd.oci.image.layer.v1.tar+zstd",
        [0x42, 0x5a, 0x68, ..] => "application/vnd.oci.image.layer.v1.tar+bzip2",
        [0xfd, 0x37, 0x7a, 0x58] => "application/vnd.oci.image.layer.v1.tar+xz",
        // No recognisable magic bytes — assume uncompressed tar.
        _ => "application/vnd.oci.image.layer.v1.tar",
    })
}

/// Load and return the image manifest from `image_dir`.
///
/// Tries `index.json` (OCI image layout) first, then falls back to
/// `manifest.json`, which may hold either a Docker save layout or a bare
/// image manifest. Every format is normalised into an [`OciManifest`] before
/// returning.
///
/// Returns an error if neither file is found, or if the manifest cannot be
/// parsed.
pub fn load_manifest(image_dir: &Path) -> Result<OciManifest> {
    let index_path = image_dir.join("index.json");
    if index_path.exists() {
        let index: OciIndex = read_json(&index_path, "image index")?;
        let desc = index.manifests.into_iter().next().ok_or_else(|| {
            Error::image_layout_msg(&index_path, "the image index lists no manifests")
        })?;
        return load_manifest_blob(image_dir, &desc, &index_path);
    }

    let manifest_path = image_dir.join("manifest.json");
    if manifest_path.exists() {
        let root: serde_json::Value = read_json(&manifest_path, "JSON document")?;
        // `docker save` writes an array, one entry per tagged image. A lone
        // object is instead a single image manifest, OCI or Docker schema 2
        // alike, as in a layout assembled from a registry's responses rather
        // than exported by a tool.
        return if root.is_array() {
            docker_save_manifest(&manifest_path, root)
        } else {
            serde_json::from_value(root)
                .map_err(|e| Error::image_layout(&manifest_path, "not a valid image manifest", e))
        };
    }

    Err(Error::image_layout_msg(
        image_dir,
        "no index.json or manifest.json found",
    ))
}

/// Normalise a Docker save `manifest.json` into an [`OciManifest`].
///
/// Docker save manifest.json has a different schema from the OCI manifest:
/// it is an array of objects (one per tagged image), each with a `Layers`
/// array of relative blob paths and an optional `LayerSources` map that
/// carries media types. Only the first image in the array is processed.
fn docker_save_manifest(path: &Path, root: serde_json::Value) -> Result<OciManifest> {
    #[derive(Deserialize)]
    struct LayerSource {
        #[serde(rename = "mediaType")]
        media_type: String,
    }

    #[derive(Deserialize)]
    struct DockerManifest {
        #[serde(rename = "Layers")]
        layers: Vec<String>,
        /// Present in Docker save layouts produced by newer Docker versions
        /// and by skopeo. Maps digest (`sha256:<hex>`) to a layer descriptor
        /// carrying the media type. Absent in older layouts.
        #[serde(rename = "LayerSources", default)]
        layer_sources: HashMap<String, LayerSource>,
    }

    let manifests: Vec<DockerManifest> = serde_json::from_value(root)
        .map_err(|e| Error::image_layout(path, "not a valid Docker save manifest", e))?;
    let dm = manifests
        .into_iter()
        .next()
        .ok_or_else(|| Error::image_layout_msg(path, "the Docker save manifest lists no images"))?;

    let layers = dm
        .layers
        .into_iter()
        .map(|l| {
            // Layer paths look like "blobs/sha256/<hex>".
            // LayerSources keys look like "sha256:<hex>".
            // Reconstruct the digest key from the path's final component.
            let digest = l
                .rsplit('/')
                .next()
                .map(|hex| format!("sha256:{hex}"))
                .unwrap_or_default();
            let media_type = dm
                .layer_sources
                .get(&digest)
                .map(|s| s.media_type.clone())
                // Empty string signals "unknown"; resolve_layers will fall
                // back to magic byte detection for this layer.
                .unwrap_or_default();
            OciDescriptor {
                digest: l,
                media_type,
            }
        })
        .collect();

    Ok(OciManifest { layers })
}

/// Resolve an [`OciDescriptor`] to an [`OciManifest`], following one level of
/// nested index indirection if necessary.
///
/// containerd and Docker Desktop commonly produce a two-level structure for
/// multi-platform images:
///
/// ```text
/// index.json → platform index  (mediaType: ...image.index...)
///            → per-platform manifest (mediaType: ...image.manifest...)
///            → layers
/// ```
///
/// When `desc` points at a nested index, the first entry whose `mediaType` is
/// a single-image manifest is selected. Entries that are themselves indexes are
/// skipped. Only one level of indirection is followed; deeper nesting is not
/// supported.
fn load_manifest_blob(
    image_dir: &Path,
    desc: &OciDescriptor,
    named_in: &Path,
) -> Result<OciManifest> {
    let path = blob_path(image_dir, &desc.digest, named_in)?;

    if is_index_media_type(&desc.media_type) {
        let nested: OciIndex = read_json(&path, "image index")?;
        let inner = nested
            .manifests
            .into_iter()
            .find(|d| is_manifest_media_type(&d.media_type))
            .ok_or_else(|| {
                Error::image_layout_msg(
                    &path,
                    "the nested image index lists no single-image manifest",
                )
            })?;
        let inner_path = blob_path(image_dir, &inner.digest, &path)?;
        return read_json(&inner_path, "image manifest");
    }

    // Direct single-image manifest.
    read_json(&path, "image manifest")
}

/// The path of the blob with SHA-256 `digest`, as named in the file
/// `named_in` (which an unsupported digest algorithm is blamed on).
fn blob_path(image_dir: &Path, digest: &str, named_in: &Path) -> Result<PathBuf> {
    let hex = strip_digest_prefix(digest).ok_or_else(|| {
        Error::image_layout_msg(
            named_in,
            format!("unsupported digest algorithm in {digest}"),
        )
    })?;
    Ok(image_dir.join("blobs").join("sha256").join(hex))
}

/// Read the JSON document at `path` as a `T`, called `what` in errors.
fn read_json<T: DeserializeOwned>(path: &Path, what: &str) -> Result<T> {
    let data = std::fs::read_to_string(path)
        .map_err(|e| Error::image_layout(path, format!("cannot read the {what}"), e))?;
    serde_json::from_str(&data)
        .map_err(|e| Error::image_layout(path, format!("not a valid {what}"), e))
}

/// Resolve the layer descriptors in `manifest` to [`LayerBlob`] values with
/// verified file paths.
///
/// Handles two path conventions used by different layout producers:
/// - OCI layout: digest-addressed (`sha256:<hex>` → `blobs/sha256/<hex>`)
/// - Docker save: relative path stored directly in the manifest
///
/// Some tools also store blobs as `<hash>/<index>` subdirectories rather than
/// flat files; this is detected and handled automatically.
///
/// If a layer's `mediaType` is absent, [`detect_media_type`] is called to
/// determine the compression format from the blob's magic bytes.
pub fn resolve_layers(image_dir: &Path, manifest: &OciManifest) -> Result<Vec<LayerBlob>> {
    manifest
        .layers
        .iter()
        .enumerate()
        .map(|(i, desc)| {
            let path = if desc.digest.contains(':') {
                // OCI layout: digest is "sha256:<hex>"
                blob_path(image_dir, &desc.digest, image_dir)?
            } else {
                // Docker save: digest field holds a relative path directly
                image_dir.join(&desc.digest)
            };

            // Some tools (e.g. older containerd export) write blobs as
            // <digest>/<manifest-index> rather than a flat file named <digest>.
            let path = if path.is_dir() {
                path.join(i.to_string())
            } else {
                path
            };

            if !path.exists() {
                return Err(Error::image_layout_msg(&path, "layer blob not found"));
            }

            let media_type = if desc.media_type.is_empty() {
                detect_media_type(&path)?.to_string()
            } else {
                desc.media_type.clone()
            };

            Ok(LayerBlob {
                path,
                media_type,
                index: i,
            })
        })
        .collect()
}

/// Strip the `sha256:` prefix from a digest string, returning just the hex
/// portion, or `None` for any other algorithm, since only SHA-256 blobs are
/// supported.
pub fn strip_digest_prefix(digest: &str) -> Option<&str> {
    digest.strip_prefix("sha256:")
}
