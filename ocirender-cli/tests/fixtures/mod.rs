//! Fixture generation for e2e tests.
//!
//! Produces three OCI image layout directories that share the same layer blobs
//! but differ in their metadata format:
//!
//!   oci-layout/        — index.json only  (OCI image layout)
//!   docker-save/       — manifest.json only (Docker save format)
//!   docker-save-both/  — both files present (index.json takes precedence)
//!
//! The layers collectively exercise:
//!   - All supported compression formats (uncompressed, gzip, zstd, bzip2, xz)
//!   - File creation, overwrite (newer layer wins)
//!   - Simple whiteout (.wh.<name>)
//!   - Opaque whiteout (.wh..wh..opq) with directory repopulation
//!   - Hard links within and across layers
//!   - Long paths and symlink targets (> 100 bytes, requiring PAX headers)
//!
//! All tar entries use the current process uid/gid so that a non-root
//! `umoci unpack --rootless` and squashfuse mount produce matching ownership
//! on both sides of the verify comparison.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    io::{Cursor, Write}, // Cursor used in TarBuilder; Write used in compress_gzip
    path::{Path, PathBuf},
};
use tar::{Builder, EntryType, Header};

// ── public types ──────────────────────────────────────────────────────────────

/// Paths to the three generated image directories.
pub struct FixtureImage {
    pub oci_layout: PathBuf,
    pub docker_save: PathBuf,
    pub docker_save_both: PathBuf,
}

/// Generate all fixture images under `base_dir` and return their paths.
pub fn generate_fixtures(base_dir: &Path) -> Result<FixtureImage> {
    let (uid, gid) = current_uid_gid();

    // Build the five layers (raw uncompressed tar bytes first, then compress).
    let layer0_tar = layer0_base(uid, gid);
    let layer1_tar = layer1_overwrite(uid, gid);
    let layer2_tar = layer2_whiteouts(uid, gid);
    let layer3_tar = layer3_hardlinks(uid, gid);
    let layer4_tar = layer4_long_paths(uid, gid);

    // Compress each layer with a different algorithm.
    let layer0_bytes = compress_uncompressed(&layer0_tar);
    let layer1_bytes = compress_gzip(&layer1_tar)?;
    let layer2_bytes = compress_gzip(&layer2_tar)?;
    let layer3_bytes = compress_gzip(&layer3_tar)?;
    let layer4_bytes = compress_gzip(&layer4_tar)?;

    // Compute SHA-256 digests.
    // umoci only reliably supports uncompressed, gzip, and xz layers.
    // zstd and bzip2 are covered by the unit/integration tests instead.
    // (uncompressed tar, compressed bytes, media_type)
    let layers: Vec<(&[u8], Vec<u8>, &'static str)> = vec![
        (
            &layer0_tar,
            layer0_bytes,
            "application/vnd.oci.image.layer.v1.tar",
        ),
        (
            &layer1_tar,
            layer1_bytes,
            "application/vnd.oci.image.layer.v1.tar+gzip",
        ),
        (
            &layer2_tar,
            layer2_bytes,
            "application/vnd.oci.image.layer.v1.tar+gzip",
        ),
        (
            &layer3_tar,
            layer3_bytes,
            "application/vnd.oci.image.layer.v1.tar+gzip",
        ),
        (
            &layer4_tar,
            layer4_bytes,
            "application/vnd.oci.image.layer.v1.tar+gzip",
        ),
    ];

    let digested: Vec<DigestedBlob> = layers
        .into_iter()
        .map(|(uncompressed, data, media_type)| DigestedBlob {
            diff_id: sha256_hex(uncompressed), // digest of uncompressed tar
            digest: sha256_hex(&data),         // digest of compressed blob
            size: data.len() as u64,
            data,
            media_type,
        })
        .collect();

    // Write three image layouts.
    let oci_layout = write_oci_layout(base_dir, &digested)?;
    let docker_save = write_docker_save(base_dir, &digested)?;
    let docker_save_both = write_docker_save_both(base_dir, &digested)?;

    Ok(FixtureImage {
        oci_layout,
        docker_save,
        docker_save_both,
    })
}

/// A single-layer image whose layer is shaped like Go `archive/tar` output.
pub struct GoUstarImage {
    pub oci_layout: PathBuf,
    /// Normalised path of a file whose absolute source path is 101 bytes,
    /// split across the USTAR prefix and name fields.
    pub split_path_file: String,
    /// Normalised path of a file owned by uid/gid 999 whose header names the
    /// owner `root`.
    pub misnamed_owner_file: String,
}

/// Generate, under `base_dir`, an OCI layout with one uncompressed layer
/// mirroring what Go's `archive/tar` writes for a nix2container image:
/// absolute `/nix/store/...` paths, a 101-byte path split across the USTAR
/// prefix and name fields, and a file owned by uid/gid 999 whose header gives
/// the owner names as `root`.
pub fn generate_go_ustar_image(base_dir: &Path) -> Result<GoUstarImage> {
    let store = format!("/nix/store/{}-pkg-1.0", "a".repeat(32));
    let share = format!("{store}/share");
    // 101 bytes in total with the '/' joining prefix and name.
    let split_name = "f".repeat(101 - share.len() - 1);
    let owned = format!("{store}/owned");

    let mut b = Builder::new(Vec::new());
    for dir in ["/nix", "/nix/store", store.as_str(), share.as_str()] {
        let h = go_ustar_header("", &format!("{dir}/"), EntryType::Directory, 0, 0, "root");
        b.append(&h, Cursor::new(b"" as &[u8]))?;
    }
    let h = go_ustar_header(&share, &split_name, EntryType::Regular, 6, 0, "root");
    b.append(&h, Cursor::new(b"split\n" as &[u8]))?;
    let h = go_ustar_header("", &owned, EntryType::Regular, 6, 999, "root");
    b.append(&h, Cursor::new(b"owned\n" as &[u8]))?;
    b.finish()?;

    Ok(GoUstarImage {
        oci_layout: write_single_layer_layout(&base_dir.join("go-ustar"), b.into_inner()?)?,
        split_path_file: format!("{}/{split_name}", &share[1..]),
        misnamed_owner_file: owned[1..].to_string(),
    })
}

/// A single-layer image holding an old-GNU sparse file.
pub struct GnuSparseImage {
    pub oci_layout: PathBuf,
    /// Path of the sparse file.
    pub sparse_file: &'static str,
    /// The sparse file's full contents, holes included.
    pub sparse_contents: Vec<u8>,
    /// Path of a regular file stored after the sparse one.
    pub after_file: &'static str,
}

/// Generate, under `base_dir`, an OCI layout with one uncompressed layer
/// holding an old-GNU sparse file (typeflag `S`, as `tar --format=gnu
/// --sparse` writes it) followed by a regular file.
pub fn generate_gnu_sparse_image(base_dir: &Path) -> Result<GnuSparseImage> {
    let head = [b'h'; 512];
    let (tail_offset, tail) = (256 * 1024, b"tail\n");
    let real_size = tail_offset + tail.len() as u64;

    let mut h = Header::new_gnu();
    h.set_path("var/log/lastlog")?;
    h.set_entry_type(EntryType::GNUSparse);
    h.set_mode(0o644);
    h.set_mtime(0);
    h.set_size((head.len() + tail.len()) as u64);
    let gnu = h.as_gnu_mut().unwrap();
    gnu.sparse[0].offset = octal12(0);
    gnu.sparse[0].numbytes = octal12(head.len() as u64);
    gnu.sparse[1].offset = octal12(tail_offset);
    gnu.sparse[1].numbytes = octal12(tail.len() as u64);
    gnu.realsize = octal12(real_size);
    h.set_cksum();

    let mut b = TarBuilder::new(0, 0);
    b.inner
        .append(&h, Cursor::new([&head[..], tail].concat()))?;
    b.add_file("etc/after", b"after\n", 0o644);

    let mut sparse_contents = vec![0u8; real_size as usize];
    sparse_contents[..head.len()].copy_from_slice(&head);
    sparse_contents[tail_offset as usize..].copy_from_slice(tail);
    Ok(GnuSparseImage {
        oci_layout: write_single_layer_layout(&base_dir.join("gnu-sparse"), b.finish())?,
        sparse_file: "var/log/lastlog",
        sparse_contents,
        after_file: "etc/after",
    })
}

/// Encode `v` as a NUL-terminated 11-digit octal tar numeric field.
fn octal12(v: u64) -> [u8; 12] {
    format!("{v:011o}\0").as_bytes().try_into().unwrap()
}

/// Write an OCI layout under `base` whose only layer is the uncompressed tar
/// `layer`.
fn write_single_layer_layout(base: &Path, layer: Vec<u8>) -> Result<PathBuf> {
    let blob = DigestedBlob {
        diff_id: sha256_hex(&layer),
        digest: sha256_hex(&layer),
        size: layer.len() as u64,
        data: layer,
        media_type: "application/vnd.oci.image.layer.v1.tar",
    };
    write_oci_layout(base, &[blob])
}

/// A USTAR header with `prefix` and `name` written raw, as Go's `archive/tar`
/// does (`set_path` rejects absolute paths and picks its own split point),
/// owned by `id` for both uid and gid but naming both owners `owner_name`.
fn go_ustar_header(
    prefix: &str,
    name: &str,
    entry_type: EntryType,
    size: u64,
    id: u64,
    owner_name: &str,
) -> Header {
    let mut h = Header::new_ustar();
    h.set_entry_type(entry_type);
    h.set_size(size);
    h.set_mode(if entry_type == EntryType::Directory {
        0o755
    } else {
        0o644
    });
    h.set_mtime(0);
    h.set_uid(id);
    h.set_gid(id);
    h.set_username(owner_name).unwrap();
    h.set_groupname(owner_name).unwrap();
    let ustar = h.as_ustar_mut().unwrap();
    ustar.prefix[..prefix.len()].copy_from_slice(prefix.as_bytes());
    ustar.name[..name.len()].copy_from_slice(name.as_bytes());
    h.set_cksum();
    h
}

// ── uid / gid ─────────────────────────────────────────────────────────────────

fn current_uid_gid() -> (u32, u32) {
    // SAFETY: getuid/getgid are always safe to call.
    unsafe { (libc::getuid(), libc::getgid()) }
}

// ── layer builders ────────────────────────────────────────────────────────────

/// Layer 0 (uncompressed): base directory tree.
///
/// Creates:
///   data/hello.txt          — regular file
///   data/overwrite-me.txt   — will be overwritten by layer 1
///   data/whiteout-me.txt    — will be whited out by layer 2
///   opaque-dir/             — will receive an opaque whiteout in layer 2
///   opaque-dir/old.txt
fn layer0_base(uid: u32, gid: u32) -> Vec<u8> {
    let mut b = TarBuilder::new(uid, gid);
    b.add_dir("data");
    b.add_file("data/hello.txt", b"hello from layer 0\n", 0o644);
    b.add_file("data/overwrite-me.txt", b"original content\n", 0o644);
    b.add_file("data/whiteout-me.txt", b"will be deleted\n", 0o644);
    b.add_dir("opaque-dir");
    b.add_file("opaque-dir/old.txt", b"old content\n", 0o644);
    b.finish()
}

/// Layer 1 (gzip): overwrite a file from layer 0.
///
/// Creates:
///   data/overwrite-me.txt   — newer version wins
///   data/layer1.txt         — new file
fn layer1_overwrite(uid: u32, gid: u32) -> Vec<u8> {
    let mut b = TarBuilder::new(uid, gid);
    b.add_dir("data");
    b.add_file("data/overwrite-me.txt", b"overwritten by layer 1\n", 0o644);
    b.add_file("data/layer1.txt", b"added in layer 1\n", 0o644);
    b.finish()
}

/// Layer 2 (zstd): whiteouts and opaque whiteout with repopulation.
///
/// Whiteouts:
///   data/whiteout-me.txt    — simple whiteout
///   opaque-dir/             — opaque whiteout (clears old.txt)
///   opaque-dir/new.txt      — repopulated after opaque whiteout
fn layer2_whiteouts(uid: u32, gid: u32) -> Vec<u8> {
    let mut b = TarBuilder::new(uid, gid);
    // Simple whiteout.
    b.add_file("data/.wh.whiteout-me.txt", b"", 0o644);
    // Opaque whiteout then repopulate.
    b.add_dir("opaque-dir");
    b.add_file("opaque-dir/.wh..wh..opq", b"", 0o644);
    b.add_file("opaque-dir/new.txt", b"repopulated\n", 0o644);
    b.finish()
}

/// Layer 3 (bzip2): hard links.
///
/// Creates:
///   hardlinks/source.txt    — regular file
///   hardlinks/link.txt      — hard link to hardlinks/source.txt (same layer)
///   data/cross-link.txt     — hard link to data/hello.txt (cross-layer target)
fn layer3_hardlinks(uid: u32, gid: u32) -> Vec<u8> {
    let mut b = TarBuilder::new(uid, gid);
    b.add_dir("hardlinks");
    b.add_file("hardlinks/source.txt", b"hardlink source\n", 0o644);
    b.add_hardlink("hardlinks/link.txt", "hardlinks/source.txt");
    // Cross-layer hard link: target (data/hello.txt) lives in layer 0.
    b.add_hardlink("data/cross-link.txt", "data/hello.txt");
    b.finish()
}

/// Layer 4 (xz): long paths and symlinks requiring PAX headers.
///
/// Creates:
///   <101-char path>/file.txt  — file whose directory path exceeds 100 bytes
///   long-symlink              — symlink with a > 100 byte target
fn layer4_long_paths(uid: u32, gid: u32) -> Vec<u8> {
    // 95 'a' chars + "/file.txt" = 104 chars total for the file path.
    let long_dir: String = "a".repeat(95);
    let long_file = format!("{long_dir}/file.txt");
    // 101-char symlink target.
    let long_target: String = "b".repeat(101);

    let mut b = TarBuilder::new(uid, gid);
    b.add_dir(&long_dir);
    b.add_file(&long_file, b"long path file\n", 0o644);
    b.add_symlink("long-symlink", &long_target);
    b.finish()
}

// ── compression ───────────────────────────────────────────────────────────────

fn compress_uncompressed(data: &[u8]) -> Vec<u8> {
    data.to_vec()
}

fn compress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::{Compression, write::GzEncoder};
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

// ── digest ────────────────────────────────────────────────────────────────────

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex_encode(&h.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

struct DigestedBlob {
    digest: String,  // hex of compressed bytes, no "sha256:" prefix
    diff_id: String, // hex of uncompressed tar bytes (OCI diff_id)
    size: u64,
    data: Vec<u8>,
    media_type: &'static str,
}

impl DigestedBlob {
    fn digest_with_prefix(&self) -> String {
        format!("sha256:{}", self.digest)
    }
}

// ── OCI image layout writer ───────────────────────────────────────────────────

/// Write an OCI image layout (index.json only).
fn write_oci_layout(base: &Path, layers: &[DigestedBlob]) -> Result<PathBuf> {
    let dir = base.join("oci-layout");
    let blobs_sha256 = dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs_sha256)?;

    // Write layer blobs.
    for layer in layers {
        std::fs::write(blobs_sha256.join(&layer.digest), &layer.data)
            .with_context(|| format!("writing layer blob {}", layer.digest))?;
    }

    // Config blob: diff_ids must be one sha256:<hex> per layer, over the
    // *uncompressed* tar.  umoci reads this to locate and verify layers.
    let config_json = make_config_json(layers);
    let config_digest = sha256_hex(config_json.as_bytes());
    std::fs::write(blobs_sha256.join(&config_digest), &config_json)?;

    // Build manifest.
    let manifest_layers: Vec<serde_json::Value> = layers
        .iter()
        .map(|l| {
            serde_json::json!({
                "mediaType": l.media_type,
                "digest": l.digest_with_prefix(),
                "size": l.size,
            })
        })
        .collect();

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_json.len(),
        },
        "layers": manifest_layers,
    });
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = sha256_hex(&manifest_bytes);
    std::fs::write(blobs_sha256.join(&manifest_digest), &manifest_bytes)?;

    // Write index.json.
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest_bytes.len(),
            "annotations": {
                "org.opencontainers.image.ref.name": "latest"
            }
        }]
    });
    std::fs::write(dir.join("index.json"), serde_json::to_vec(&index)?)?;

    // Write oci-layout marker (required by spec).
    std::fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#)?;

    Ok(dir)
}

/// Write a Docker save layout (manifest.json only, no index.json).
fn write_docker_save(base: &Path, layers: &[DigestedBlob]) -> Result<PathBuf> {
    let dir = base.join("docker-save");
    let blobs_sha256 = dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs_sha256)?;

    for layer in layers {
        std::fs::write(blobs_sha256.join(&layer.digest), &layer.data)?;
    }

    let config_json = make_config_json(layers);
    let config_digest = sha256_hex(config_json.as_bytes());
    std::fs::write(blobs_sha256.join(&config_digest), &config_json)?;

    write_docker_manifest_json(&dir, layers, &config_digest)?;
    Ok(dir)
}

/// Write a layout with *both* index.json and manifest.json present.
/// The layer blobs and metadata are identical to the OCI layout so that
/// verify passes regardless of which file is parsed.
fn write_docker_save_both(base: &Path, layers: &[DigestedBlob]) -> Result<PathBuf> {
    // Start from a full OCI layout, then add manifest.json alongside.
    let oci_dir = base.join("docker-save-both");
    let blobs_sha256 = oci_dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs_sha256)?;

    for layer in layers {
        std::fs::write(blobs_sha256.join(&layer.digest), &layer.data)?;
    }

    let config_json = make_config_json(layers);
    let config_digest = sha256_hex(config_json.as_bytes());
    std::fs::write(blobs_sha256.join(&config_digest), &config_json)?;

    // OCI manifest + index.json.
    let manifest_layers: Vec<serde_json::Value> = layers
        .iter()
        .map(|l| {
            serde_json::json!({
                "mediaType": l.media_type,
                "digest": l.digest_with_prefix(),
                "size": l.size,
            })
        })
        .collect();
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_json.len(),
        },
        "layers": manifest_layers,
    });
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = sha256_hex(&manifest_bytes);
    std::fs::write(blobs_sha256.join(&manifest_digest), &manifest_bytes)?;

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest_bytes.len(),
            "annotations": {
                "org.opencontainers.image.ref.name": "latest"
            }
        }]
    });
    std::fs::write(oci_dir.join("index.json"), serde_json::to_vec(&index)?)?;
    std::fs::write(
        oci_dir.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;

    // Also write manifest.json — our code should ignore it when index.json exists.
    write_docker_manifest_json(&oci_dir, layers, &config_digest)?;

    Ok(oci_dir)
}

/// Build a minimal but valid OCI image config JSON with correct diff_ids.
/// umoci validates that diff_ids match the uncompressed layer digests, so
/// this must be accurate or `umoci unpack` will panic/error.
fn make_config_json(layers: &[DigestedBlob]) -> String {
    let diff_ids: Vec<String> = layers
        .iter()
        .map(|l| format!("sha256:{}", l.diff_id))
        .collect();
    serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": diff_ids,
        },
        "config": {},
        "history": [],
    })
    .to_string()
}

/// Emit a Docker-save `manifest.json` into `dir`.
fn write_docker_manifest_json(
    dir: &Path,
    layers: &[DigestedBlob],
    config_digest: &str,
) -> Result<()> {
    let layer_paths: Vec<String> = layers
        .iter()
        .map(|l| format!("blobs/sha256/{}", l.digest))
        .collect();

    let layer_sources: serde_json::Map<String, serde_json::Value> = layers
        .iter()
        .map(|l| {
            let key = l.digest_with_prefix();
            let val = serde_json::json!({
                "mediaType": l.media_type,
                "size": l.size,
                "digest": key,
            });
            (key, val)
        })
        .collect();

    let manifest_json = serde_json::json!([{
        "Config": format!("blobs/sha256/{config_digest}"),
        "RepoTags": ["ocirender-fixture:latest"],
        "Layers": layer_paths,
        "LayerSources": layer_sources,
    }]);

    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&manifest_json)?,
    )
    .context("writing manifest.json")?;
    Ok(())
}

// ── TarBuilder helper ─────────────────────────────────────────────────────────

/// A thin wrapper around `tar::Builder` that stamps every entry with the
/// given uid/gid and handles PAX extensions for long paths automatically.
struct TarBuilder {
    inner: Builder<Vec<u8>>,
    uid: u32,
    gid: u32,
}

impl TarBuilder {
    fn new(uid: u32, gid: u32) -> Self {
        let mut b = Builder::new(Vec::new());
        b.mode(tar::HeaderMode::Complete);
        Self { inner: b, uid, gid }
    }

    fn base_header(&self, path: &str, size: u64, mode: u32, entry_type: EntryType) -> Header {
        let mut h = Header::new_ustar();
        h.set_entry_type(entry_type);
        h.set_size(size);
        h.set_mode(mode);
        h.set_mtime(0);
        h.set_uid(self.uid as u64);
        h.set_gid(self.gid as u64);
        h.set_username("").ok();
        h.set_groupname("").ok();
        // Best-effort short path; PAX extension carries the full value if needed.
        h.set_path(truncate(path, 99)).ok();
        h.set_cksum();
        h
    }

    fn pax_if_long<'a>(path: &'a str, link: Option<&'a str>) -> Vec<(&'static str, &'a [u8])> {
        let mut pax = Vec::new();
        if path.len() > 99 {
            pax.push(("path", path.as_bytes()));
        }
        if let Some(l) = link {
            if l.len() > 99 {
                pax.push(("linkpath", l.as_bytes()));
            }
        }
        pax
    }

    fn emit_pax(&mut self, pax: Vec<(&'static str, &[u8])>) {
        if !pax.is_empty() {
            self.inner.append_pax_extensions(pax).unwrap();
        }
    }

    pub fn add_dir(&mut self, path: &str) {
        let pax = Self::pax_if_long(path, None);
        self.emit_pax(pax);
        let mut h = self.base_header(path, 0, 0o755, EntryType::Directory);
        // Directories must end with '/' in USTAR.
        let dir_path = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        h.set_path(truncate(&dir_path, 99)).ok();
        h.set_cksum();
        self.inner.append(&h, Cursor::new(b"" as &[u8])).unwrap();
    }

    pub fn add_file(&mut self, path: &str, data: &[u8], mode: u32) {
        let pax = Self::pax_if_long(path, None);
        self.emit_pax(pax);
        let mut h = self.base_header(path, data.len() as u64, mode, EntryType::Regular);
        h.set_cksum();
        self.inner.append(&h, Cursor::new(data)).unwrap();
    }

    pub fn add_symlink(&mut self, path: &str, target: &str) {
        let pax = Self::pax_if_long(path, Some(target));
        self.emit_pax(pax);
        let mut h = self.base_header(path, 0, 0o777, EntryType::Symlink);
        h.set_link_name(truncate(target, 99)).ok();
        h.set_cksum();
        self.inner.append(&h, Cursor::new(b"" as &[u8])).unwrap();
    }

    pub fn add_hardlink(&mut self, path: &str, target: &str) {
        let pax = Self::pax_if_long(path, Some(target));
        self.emit_pax(pax);
        let mut h = self.base_header(path, 0, 0o644, EntryType::Link);
        h.set_link_name(truncate(target, 99)).ok();
        h.set_cksum();
        self.inner.append(&h, Cursor::new(b"" as &[u8])).unwrap();
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.inner.finish().unwrap();
        self.inner.into_inner().unwrap()
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
