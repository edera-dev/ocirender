//! End-to-end tests for the ocirender CLI.
//!
//! These tests require the following binaries to be present on PATH:
//!   - mksquashfs       (squashfs-tools, >= 4.6)
//!   - squashfuse
//!   - fusermount or umount
//!   - umoci
//!
//! Run with:
//!   cargo test --features e2e --test e2e
//!
//! Tests that contact real registries are additionally gated behind the
//! `network` feature flag:
//!   cargo test --features e2e,network --test e2e
//!
//! Network tests compare ocirender output against umoci as an independent
//! reference implementation.  Self-comparisons (ocirender vs ocirender) are
//! intentionally avoided: they can only catch regressions where two code paths
//! diverge from each other, not cases where both are wrong relative to the
//! spec.

// ── modules ──────────────────────────────────────────────────────────────────

mod fixtures;

use fixtures::generate_fixtures;

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};
use tempfile::TempDir;

// ── required-binary checking ──────────────────────────────────────────────────

/// Names and a short description of every external binary we depend on.
const REQUIRED_BINARIES: &[(&str, &str)] = &[
    ("mksquashfs", "squashfs-tools >= 4.6"),
    ("squashfuse", "squashfuse"),
    ("umoci", "umoci (OCI image unpacker)"),
];

/// Returns the path to the compiled `ocirender` binary produced by
/// the current `cargo test` invocation.  Panics if it cannot be located.
fn cli_bin() -> PathBuf {
    // CARGO_BIN_EXE_ocirender is set by Cargo for [[bin]] targets when
    // running tests.
    let var = env!("CARGO_BIN_EXE_ocirender");
    PathBuf::from(var)
}

/// Convenience: return a `Command` pointing at the compiled CLI binary.
fn ocirender() -> Command {
    Command::new(cli_bin())
}

/// Verify that every required external binary is present on PATH (or absolute).
/// Called once at test startup; panics with a clear message if anything is
/// missing.
fn require_binaries() {
    let cli = cli_bin();
    assert!(
        cli.exists(),
        "ocirender binary not found at {cli:?}; \
         ensure you ran `cargo test --features e2e`"
    );

    let mut missing = Vec::new();
    for (name, description) in REQUIRED_BINARIES {
        if which(name).is_none() {
            missing.push(format!("  {name}  ({description})"));
        }
    }

    let has_unmount = which("fusermount").is_some() || which("umount").is_some();
    if !has_unmount {
        missing.push("  fusermount or umount  (FUSE unmounting)".into());
    }

    if !missing.is_empty() {
        panic!(
            "E2E tests require the following binaries which were not found on PATH:\n{}\n\
             Install them and re-run `cargo test --features e2e --test e2e`.",
            missing.join("\n")
        );
    }
}

/// Returns the absolute path of `name` on PATH, or `None`.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .map(|path_var| {
            std::env::split_paths(&path_var)
                .map(|dir| dir.join(name))
                .find(|p| p.is_file())
        })
        .flatten()
}

// ── shared fixture state ──────────────────────────────────────────────────────

/// Holds the temp directory (keeping it alive) and the generated image paths.
struct Fixtures {
    _dir: TempDir,
    oci_layout: PathBuf,
    docker_save: PathBuf,
    docker_save_both: PathBuf,
}

/// Generate fixtures exactly once for the whole test run.
static FIXTURES: OnceLock<Fixtures> = OnceLock::new();

fn get_fixtures() -> &'static Fixtures {
    FIXTURES.get_or_init(|| {
        require_binaries();
        let dir = TempDir::new().expect("creating fixture temp dir");
        let images = generate_fixtures(dir.path()).expect("generating OCI fixtures");
        Fixtures {
            oci_layout: images.oci_layout,
            docker_save: images.docker_save,
            docker_save_both: images.docker_save_both,
            _dir: dir,
        }
    })
}

/// Fixtures for tests that only need the CLI binary (no squashfuse/umoci).
static FIXTURES_BASIC: OnceLock<Fixtures> = OnceLock::new();

fn get_fixtures_basic() -> &'static Fixtures {
    FIXTURES_BASIC.get_or_init(|| {
        let cli = cli_bin();
        assert!(cli.exists(), "ocirender binary not found at {cli:?}");
        let dir = TempDir::new().expect("creating fixture temp dir");
        let images = generate_fixtures(dir.path()).expect("generating OCI fixtures");
        Fixtures {
            oci_layout: images.oci_layout,
            docker_save: images.docker_save,
            docker_save_both: images.docker_save_both,
            _dir: dir,
        }
    })
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Returns `true` if the current process is running as root (uid 0).
#[cfg(feature = "network")]
fn is_root() -> bool {
    // SAFETY: getuid() has no preconditions and is always safe to call.
    unsafe { libc::getuid() == 0 }
}

/// Run `ocirender convert-squashfs` and return the path to the output file.
fn convert_squashfs(image_dir: &Path, out_dir: &Path, name: &str) -> PathBuf {
    let output = out_dir.join(format!("{name}.squashfs"));
    let status = ocirender()
        .args([
            "convert-squashfs",
            "--image",
            image_dir.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .status()
        .expect("spawning ocirender convert-squashfs");
    assert!(
        status.success(),
        "ocirender convert-squashfs failed for {name} (image: {})",
        image_dir.display()
    );
    output
}

fn convert_tar(image_dir: &Path, out_dir: &Path, name: &str) -> PathBuf {
    let output = out_dir.join(format!("{name}.tar"));
    let status = ocirender()
        .args([
            "convert-tar",
            "--image",
            image_dir.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .status()
        .expect("spawning ocirender convert-tar");
    assert!(
        status.success(),
        "ocirender convert-tar failed for {name} (image: {})",
        image_dir.display()
    );
    output
}

fn convert_dir(image_dir: &Path, out_dir: &Path, name: &str) -> PathBuf {
    let output = out_dir.join(name);
    let status = ocirender()
        .args([
            "convert-dir",
            "--image",
            image_dir.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .status()
        .expect("spawning ocirender convert-dir");
    assert!(
        status.success(),
        "ocirender convert-dir failed for {name} (image: {})",
        image_dir.display()
    );
    output
}

/// Annotate the first entry in a fetched layout's `index.json` with the given
/// tag, making it consumable by umoci.
///
/// `ocirender fetch` writes a valid OCI image layout but produces no named
/// tags (no `org.opencontainers.image.ref.name` annotation).  umoci requires
/// a named tag to unpack.  Rather than fighting with `umoci tag` (which errors
/// on tagless source layouts), we patch `index.json` directly — that's exactly
/// what the OCI image layout spec defines as the tag mechanism.
#[cfg(feature = "network")]
fn tag_fetched_layout(layout: &Path, tag: &str) {
    let index_path = layout.join("index.json");
    let data = std::fs::read_to_string(&index_path)
        .unwrap_or_else(|e| panic!("reading index.json from {}: {e}", layout.display()));
    let mut index: serde_json::Value = serde_json::from_str(&data).expect("parsing index.json");

    let manifests = index["manifests"]
        .as_array_mut()
        .expect("index.json has no manifests array");
    assert!(
        !manifests.is_empty(),
        "index.json manifests array is empty in {}",
        layout.display()
    );

    let entry = &mut manifests[0];
    entry["annotations"]
        .as_object_mut()
        .map(|a| {
            a.insert(
                "org.opencontainers.image.ref.name".into(),
                serde_json::Value::String(tag.into()),
            )
        })
        .unwrap_or_else(|| {
            entry["annotations"] = serde_json::json!({
                "org.opencontainers.image.ref.name": tag
            });
            None
        });

    std::fs::write(
        &index_path,
        serde_json::to_vec_pretty(&index).expect("serialising index.json"),
    )
    .unwrap_or_else(|e| panic!("writing index.json to {}: {e}", layout.display()));
}

/// Run `umoci unpack --rootless` and return the path to the unpacked rootfs.
fn umoci_unpack(oci_dir: &Path, reference_tag: &str, out_dir: &Path, name: &str) -> PathBuf {
    let bundle = out_dir.join(format!("{name}-bundle"));
    let status = Command::new("umoci")
        .args([
            "unpack",
            "--rootless",
            "--image",
            &format!("{}:{}", oci_dir.display(), reference_tag),
            bundle.to_str().unwrap(),
        ])
        .status()
        .expect("spawning umoci unpack");
    assert!(
        status.success(),
        "umoci unpack failed for {name} (image: {}:{})",
        oci_dir.display(),
        reference_tag
    );
    bundle.join("rootfs")
}

/// Run `ocirender verify --squashfs` and assert it exits successfully.
///
/// Pass `ignore_ownership: !is_root()` for squashfs-vs-dir comparisons:
/// non-root directory extraction silently skips `chown`, making uid/gid
/// mismatches spurious.
fn verify_squashfs_clean(squashfs: &Path, reference: &Path, ignore_ownership: bool, label: &str) {
    let mut cmd = ocirender();
    cmd.args([
        "verify",
        "--squashfs",
        squashfs.to_str().unwrap(),
        "--reference",
        reference.to_str().unwrap(),
    ]);
    if ignore_ownership {
        cmd.arg("--ignore-ownership");
    }
    let status = cmd.status().expect("spawning ocirender verify");
    assert!(
        status.success(),
        "verify reported differences for {label}\n  squashfs: {}\n  reference: {}",
        squashfs.display(),
        reference.display()
    );
}

/// Run `ocirender verify --dir` and assert it exits successfully.
#[cfg(feature = "network")]
fn verify_dir_clean(dir: &Path, reference: &Path, label: &str) {
    let status = ocirender()
        .args([
            "verify",
            "--dir",
            dir.to_str().unwrap(),
            "--reference",
            reference.to_str().unwrap(),
        ])
        .status()
        .expect("spawning ocirender verify --dir");
    assert!(
        status.success(),
        "verify reported differences for {label}\n  dir: {}\n  reference: {}",
        dir.display(),
        reference.display()
    );
}

// ── local-fixture tests ───────────────────────────────────────────────────────

#[test]
fn e2e_oci_layout_convert_and_verify() {
    let fx = get_fixtures();
    let work = TempDir::new().unwrap();
    let squashfs = convert_squashfs(&fx.oci_layout, work.path(), "oci-layout");
    let reference = umoci_unpack(&fx.oci_layout, "latest", work.path(), "oci-layout");
    verify_squashfs_clean(&squashfs, &reference, false, "oci-layout");
}

#[test]
fn e2e_docker_save_convert_and_verify() {
    let fx = get_fixtures();
    let work = TempDir::new().unwrap();
    let squashfs = convert_squashfs(&fx.docker_save, work.path(), "docker-save");
    let reference = umoci_unpack(&fx.oci_layout, "latest", work.path(), "docker-save-ref");
    verify_squashfs_clean(&squashfs, &reference, false, "docker-save");
}

#[test]
fn e2e_both_metadata_files_prefers_index_json() {
    let fx = get_fixtures();
    let work = TempDir::new().unwrap();
    let squashfs = convert_squashfs(&fx.docker_save_both, work.path(), "both");
    let reference = umoci_unpack(&fx.oci_layout, "latest", work.path(), "both-ref");
    verify_squashfs_clean(
        &squashfs,
        &reference,
        false,
        "docker-save-both (index.json preferred)",
    );
}

/// Verify that the squashfs root directory has sane permissions (0755, root/root).
/// A missing root tar entry causes mksquashfs to default to 0777 owned by the
/// build user.
#[test]
fn e2e_root_directory_permissions() {
    let fx = get_fixtures();
    let work = TempDir::new().unwrap();
    let squashfs = convert_squashfs(&fx.oci_layout, work.path(), "root-perms");

    let out = Command::new("unsquashfs")
        .args(["-lls", squashfs.to_str().unwrap(), "."])
        .output()
        .expect("spawning unsquashfs -lls");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let root_line = stdout
        .lines()
        .find(|l| l.ends_with("squashfs-root"))
        .unwrap_or_else(|| panic!("no squashfs-root line in unsquashfs output:\n{stdout}"));

    let mut fields = root_line.split_whitespace();
    let mode = fields.next().unwrap_or("");
    let owner = fields.next().unwrap_or("");

    assert_eq!(
        mode, "drwxr-xr-x",
        "squashfs root directory should be mode 0755; got {root_line:?}"
    );
    assert_eq!(
        owner, "root/root",
        "squashfs root directory should be owned by root/root; got {root_line:?}"
    );
}

#[test]
fn e2e_overlay_semantics_verified() {
    let fx = get_fixtures();
    let work = TempDir::new().unwrap();
    let squashfs = convert_squashfs(&fx.oci_layout, work.path(), "overlay-semantics");
    let reference = umoci_unpack(
        &fx.oci_layout,
        "latest",
        work.path(),
        "overlay-semantics-ref",
    );
    verify_squashfs_clean(&squashfs, &reference, false, "overlay semantics");
}

/// Convert OCI → tar and spot-check overlay semantics: expected files present,
/// whited-out files absent.
#[test]
fn e2e_convert_tar_overlay_semantics() {
    let fx = get_fixtures_basic();
    let work = TempDir::new().unwrap();
    let tar_path = convert_tar(&fx.oci_layout, work.path(), "oci-layout");
    let tar_bytes = std::fs::read(&tar_path).expect("reading output tar");

    let paths: std::collections::HashSet<String> = {
        use std::io::Cursor;
        tar::Archive::new(Cursor::new(&tar_bytes))
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.path().ok().map(|p| p.to_string_lossy().into_owned()))
            .collect()
    };

    for expected in &[
        "data/hello.txt",
        "data/layer1.txt",
        "data/overwrite-me.txt",
        "opaque-dir/new.txt",
        "hardlinks/source.txt",
    ] {
        assert!(paths.contains(*expected), "{expected} must be present");
    }
    for absent in &["data/whiteout-me.txt", "opaque-dir/old.txt"] {
        assert!(!paths.contains(*absent), "{absent} must be absent");
    }
}

/// Convert OCI → directory and verify overlay semantics by content inspection.
#[test]
fn e2e_convert_dir_overlay_semantics() {
    let fx = get_fixtures_basic();
    let work = TempDir::new().unwrap();
    let dir = convert_dir(&fx.oci_layout, work.path(), "oci-layout-dir");

    assert_eq!(
        std::fs::read(dir.join("data/hello.txt")).unwrap(),
        b"hello from layer 0\n"
    );
    assert_eq!(
        std::fs::read(dir.join("data/overwrite-me.txt")).unwrap(),
        b"overwritten by layer 1\n",
        "layer 1 must win the overwrite"
    );
    assert_eq!(
        std::fs::read(dir.join("opaque-dir/new.txt")).unwrap(),
        b"repopulated\n"
    );
    assert_eq!(
        std::fs::read(dir.join("hardlinks/source.txt")).unwrap(),
        b"hardlink source\n"
    );

    assert!(
        !dir.join("data/whiteout-me.txt").exists(),
        "data/whiteout-me.txt must be absent"
    );
    assert!(
        !dir.join("opaque-dir/old.txt").exists(),
        "opaque-dir/old.txt must be absent"
    );
}

// ── network tests ─────────────────────────────────────────────────────────────
//
// All network tests compare ocirender output against umoci as a reference
// implementation.  This catches cases where both ocirender code paths agree
// with each other but are both wrong — something self-comparison cannot detect.
//
// Run with:
//   cargo test --features e2e,network --test e2e

/// Fetch `image` (linux/amd64) into a temp directory inside `work`, annotate
/// the layout with a "latest" tag so umoci can consume it, then unpack with
/// umoci.  Returns `(layout_dir, umoci_rootfs_dir)`.
///
/// This is the shared setup for all network content-verification tests.
#[cfg(feature = "network")]
fn fetch_with_umoci_reference(image: &str, work: &Path, label: &str) -> (PathBuf, PathBuf) {
    let layout = work.join(format!("{label}-layout"));

    let status = ocirender()
        .args([
            "fetch",
            "--image",
            image,
            "--platform",
            "linux/amd64",
            "--output",
            layout.to_str().unwrap(),
        ])
        .status()
        .unwrap_or_else(|e| panic!("spawning ocirender fetch {image}: {e}"));
    assert!(status.success(), "ocirender fetch {image} failed: {status}");

    // Annotate index.json so umoci can find the manifest by tag.
    tag_fetched_layout(&layout, "latest");

    let reference = umoci_unpack(&layout, "latest", work, &format!("{label}-umoci"));

    (layout, reference)
}

/// `fetch --manifest-only` must write a parseable OCI layout without
/// downloading any layer blobs.
///
/// The bitnami postgres-exporter image uses Docker distribution media types
/// rather than OCI media types and previously exposed a manifest parsing bug,
/// making it a cheap regression target for the Docker-vs-OCI media type
/// handling path in `load_manifest_blob`.
#[test]
#[cfg(feature = "network")]
fn fetch_manifest_only_docker_media_types() {
    let work = TempDir::new().unwrap();
    let layout = work.path().join("layout");

    let status = ocirender()
        .args([
            "fetch",
            "--image",
            "docker.io/bitnamilegacy/postgres-exporter:0.15.0-debian-11-r7",
            "--output",
            layout.to_str().unwrap(),
            "--manifest-only",
        ])
        .status()
        .expect("spawning ocirender fetch --manifest-only");
    assert!(status.success(), "fetch --manifest-only failed: {status}");

    assert!(
        layout.join("oci-layout").exists(),
        "missing oci-layout marker"
    );
    assert!(layout.join("index.json").exists(), "missing index.json");

    let manifest = ocirender::image::load_manifest(&layout)
        .expect("load_manifest failed on fetch --manifest-only output");
    assert!(
        !manifest.layers.is_empty(),
        "manifest has no layers — broken Docker media type handling"
    );

    let blob_count = std::fs::read_dir(layout.join("blobs").join("sha256"))
        .expect("reading blobs/sha256")
        .filter_map(|e| e.ok())
        .count();
    assert_eq!(
        blob_count, 1,
        "expected exactly 1 blob (the manifest); found {blob_count} — \
         layer blobs must not be downloaded with --manifest-only"
    );
}

/// `fetch --manifest-only` on an OCI index must resolve to the linux/amd64
/// platform manifest and produce exactly one blob.
#[test]
#[cfg(feature = "network")]
fn fetch_manifest_only_oci_index() {
    let work = TempDir::new().unwrap();
    let layout = work.path().join("layout");

    let status = ocirender()
        .args([
            "fetch",
            "--image",
            "docker.io/library/alpine:latest",
            "--platform",
            "linux/amd64",
            "--output",
            layout.to_str().unwrap(),
            "--manifest-only",
        ])
        .status()
        .expect("spawning ocirender fetch --manifest-only alpine");
    assert!(
        status.success(),
        "fetch --manifest-only (alpine) failed: {status}"
    );

    let manifest = ocirender::image::load_manifest(&layout)
        .expect("load_manifest failed on alpine --manifest-only output");
    assert!(!manifest.layers.is_empty(), "alpine manifest has no layers");

    let blob_count = std::fs::read_dir(layout.join("blobs").join("sha256"))
        .expect("reading blobs/sha256")
        .filter_map(|e| e.ok())
        .count();
    assert_eq!(blob_count, 1, "expected exactly 1 blob; found {blob_count}");
}

/// `convert-dir` output must match umoci's unpacked rootfs for busybox:1.36.
///
/// busybox is a single-layer gzip image with a stable, frozen tag — ideal for
/// catching regressions in the fetch → convert-dir path against a reference
/// implementation without risking content changes between runs.
#[test]
#[cfg(feature = "network")]
fn fetch_convert_dir_matches_umoci_busybox() {
    require_binaries();
    let work = TempDir::new().unwrap();
    let (layout, reference) =
        fetch_with_umoci_reference("docker.io/library/busybox:1.36", work.path(), "busybox");

    let generated = convert_dir(&layout, work.path(), "busybox-dir");
    verify_dir_clean(
        &generated,
        &reference,
        "fetch+convert-dir vs umoci (busybox:1.36)",
    );
}

/// `convert-dir` output must match umoci's unpacked rootfs for alpine:3.19.
///
/// A second image guards against busybox-specific quirks and exercises the
/// alpine layer structure (apk database, etc.) through the merge pipeline.
#[test]
#[cfg(feature = "network")]
fn fetch_convert_dir_matches_umoci_alpine() {
    require_binaries();
    let work = TempDir::new().unwrap();
    let (layout, reference) =
        fetch_with_umoci_reference("docker.io/library/alpine:3.19", work.path(), "alpine");

    let generated = convert_dir(&layout, work.path(), "alpine-dir");
    verify_dir_clean(
        &generated,
        &reference,
        "fetch+convert-dir vs umoci (alpine:3.19)",
    );
}

/// `convert-squashfs` output must match umoci's unpacked rootfs for
/// busybox:1.36.
///
/// Verifies the squashfs output path against the same umoci reference used by
/// `fetch_convert_dir_matches_umoci_busybox`.  Ownership comparison is skipped
/// when non-root because umoci `--rootless` cannot preserve uid/gid in the
/// reference directory, while mksquashfs faithfully records them from the tar
/// headers.
#[test]
#[cfg(feature = "network")]
fn fetch_convert_squashfs_matches_umoci_busybox() {
    require_binaries();
    let work = TempDir::new().unwrap();
    let (layout, reference) =
        fetch_with_umoci_reference("docker.io/library/busybox:1.36", work.path(), "busybox");

    let squashfs = convert_squashfs(&layout, work.path(), "busybox");
    verify_squashfs_clean(
        &squashfs,
        &reference,
        !is_root(),
        "fetch+convert-squashfs vs umoci (busybox:1.36)",
    );
}

/// `pull --output-dir` output must match umoci's unpacked rootfs for alpine:3.19.
///
/// This validates the streaming packer path against the reference
/// implementation, not against another ocirender code path.  If `pull` and
/// `convert-dir` both agree with umoci independently, they trivially agree with
/// each other — the converse is not true.
#[test]
#[cfg(feature = "network")]
fn pull_dir_matches_umoci_alpine() {
    require_binaries();
    let work = TempDir::new().unwrap();

    // Fetch for umoci reference — we need the layout on disk regardless.
    let (_, reference) =
        fetch_with_umoci_reference("docker.io/library/alpine:3.19", work.path(), "alpine");

    let pull_dir = work.path().join("alpine-pull");
    let status = ocirender()
        .args([
            "pull",
            "--image",
            "docker.io/library/alpine:3.19",
            "--platform",
            "linux/amd64",
            "--output-dir",
            pull_dir.to_str().unwrap(),
        ])
        .status()
        .expect("spawning ocirender pull --output-dir alpine");
    assert!(
        status.success(),
        "pull --output-dir alpine failed: {status}"
    );

    verify_dir_clean(
        &pull_dir,
        &reference,
        "pull --output-dir vs umoci (alpine:3.19)",
    );
}
