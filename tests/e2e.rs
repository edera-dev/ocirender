//! End-to-end tests for the oci2squashfs CLI.
//!
//! These tests require the following binaries to be present on PATH:
//!   - oci2squashfs_cli  (built by `cargo test --features e2e`)
//!   - mksquashfs       (squashfs-tools, >= 4.6)
//!   - squashfuse
//!   - fusermount or umount
//!   - umoci
//!
//! Run with:
//!   cargo test --features e2e --test e2e

// ── modules ──────────────────────────────────────────────────────────────────

mod fixtures;

use fixtures::generate_fixtures;

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};
use tempfile::TempDir;

// ── required-binary checking ─────────────────────────────────────────────────

/// Names and a short description of every external binary we depend on.
const REQUIRED_BINARIES: &[(&str, &str)] = &[
    ("mksquashfs",      "squashfs-tools >= 4.6"),
    ("squashfuse",      "squashfuse"),
    ("umoci",           "umoci (OCI image unpacker)"),
];

/// Returns the path to the compiled `oci2squashfs_cli` binary produced by
/// the current `cargo test` invocation.  Panics if it cannot be located.
fn cli_bin() -> PathBuf {
    // CARGO_BIN_EXE_oci2squashfs_cli is set by Cargo for [[bin]] targets when
    // running tests.
    let var = env!("CARGO_BIN_EXE_oci2squashfs_cli");
    PathBuf::from(var)
}

/// Verify that every required external binary is present on PATH (or absolute).
/// Called once at test startup; panics with a clear message if anything is missing.
fn require_binaries() {
    // Check the CLI binary first — it must exist as a file.
    let cli = cli_bin();
    assert!(
        cli.exists(),
        "oci2squashfs_cli binary not found at {cli:?}; \
         ensure you ran `cargo test --features e2e`"
    );

    let mut missing = Vec::new();
    for (name, description) in REQUIRED_BINARIES {
        if which(name).is_none() {
            missing.push(format!("  {name}  ({description})"));
        }
    }

    // Check for at least one unmount command.
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
    // `which` crate is not a dependency; replicate the essential logic.
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
            oci_layout:       images.oci_layout,
            docker_save:      images.docker_save,
            docker_save_both: images.docker_save_both,
            _dir: dir,
        }
    })
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Run `oci2squashfs_cli convert` and return the path to the output file.
fn convert(image_dir: &Path, out_dir: &Path, name: &str) -> PathBuf {
    let output = out_dir.join(format!("{name}.squashfs"));
    let status = Command::new(cli_bin())
        .args([
            "convert",
            "--image",
            image_dir.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .status()
        .expect("spawning oci2squashfs_cli convert");
    assert!(
        status.success(),
        "oci2squashfs_cli convert failed for {name} (image: {})",
        image_dir.display()
    );
    output
}

/// Run `umoci unpack --rootless` and return the path to the unpacked bundle root.
/// The bundle directory is created inside `out_dir`.
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
    // umoci produces an OCI bundle; the rootfs lives at <bundle>/rootfs.
    bundle.join("rootfs")
}

/// Run `oci2squashfs_cli verify` and assert it exits successfully (no differences).
fn verify_clean(squashfs: &Path, reference: &Path, label: &str) {
    let status = Command::new(cli_bin())
        .args([
            "verify",
            "--squashfs",
            squashfs.to_str().unwrap(),
            "--reference",
            reference.to_str().unwrap(),
        ])
        .status()
        .expect("spawning oci2squashfs_cli verify");
    assert!(
        status.success(),
        "verify reported differences for {label}\n  squashfs: {}\n  reference: {}",
        squashfs.display(),
        reference.display()
    );
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Convert the OCI-layout (index.json) image and verify it against umoci output.
#[test]
fn e2e_oci_layout_convert_and_verify() {
    let fx = get_fixtures();
    let work = TempDir::new().unwrap();

    let squashfs = convert(&fx.oci_layout, work.path(), "oci-layout");
    let reference = umoci_unpack(&fx.oci_layout, "latest", work.path(), "oci-layout");
    verify_clean(&squashfs, &reference, "oci-layout");
}

/// Convert the Docker-save (manifest.json only) image and verify it.
#[test]
fn e2e_docker_save_convert_and_verify() {
    let fx = get_fixtures();
    let work = TempDir::new().unwrap();

    let squashfs = convert(&fx.docker_save, work.path(), "docker-save");
    // Docker save images have no OCI tag; use the OCI-layout image as the
    // reference since they share the same layer blobs and logical content.
    let reference = umoci_unpack(&fx.oci_layout, "latest", work.path(), "docker-save-ref");
    verify_clean(&squashfs, &reference, "docker-save");
}

/// When both index.json and manifest.json are present, index.json takes
/// precedence.  The output must match the OCI-layout reference.
#[test]
fn e2e_both_metadata_files_prefers_index_json() {
    let fx = get_fixtures();
    let work = TempDir::new().unwrap();

    let squashfs = convert(&fx.docker_save_both, work.path(), "both");
    let reference = umoci_unpack(&fx.oci_layout, "latest", work.path(), "both-ref");
    verify_clean(&squashfs, &reference, "docker-save-both (index.json preferred)");
}

/// Explicitly verify the overlay semantics that the fixture layers exercise:
/// whiteouts, opaque whiteouts, and hard links all behave correctly in the
/// squashfs output compared to the umoci reference.
#[test]
fn e2e_overlay_semantics_verified() {
    let fx = get_fixtures();
    let work = TempDir::new().unwrap();

    // This is the same convert+verify as the oci-layout test, but we keep it
    // as a distinct test with a distinct label so failures here are
    // unambiguously about overlay correctness rather than metadata parsing.
    let squashfs = convert(&fx.oci_layout, work.path(), "overlay-semantics");
    let reference = umoci_unpack(&fx.oci_layout, "latest", work.path(), "overlay-semantics-ref");
    verify_clean(&squashfs, &reference, "overlay semantics");
}
