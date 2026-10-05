//! Errors are categorised by what failed, so that callers can act on them: a
//! bad layer is reported as that layer's error, an output that cannot be
//! written as an output error, and mksquashfs failing as its own failure,
//! whichever side of the pipe notices a problem first.

use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path, sync::mpsc};

use tempfile::TempDir;

use super::helpers::{LayerBuilder, blob, error_chain};
use crate::{Error, LayerBlob, LayerItem};

/// A channel already holding `layers`, as the sinks take them.
fn channel(layers: Vec<LayerBlob>) -> (mpsc::Receiver<LayerItem>, usize) {
    let total = layers.len();
    let (tx, rx) = mpsc::channel();
    for layer in layers {
        tx.send(Ok(layer)).unwrap();
    }
    (rx, total)
}

/// `len` bytes that do not compress, so a gzip stream of them is long enough
/// to cut off partway through.
fn incompressible(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A gzip layer whose stream is cut off partway through a file's data: its
/// header parses, and the failure only surfaces while the data is copied.
fn truncated_gzip_layer(index: usize) -> LayerBlob {
    let tar = LayerBuilder::new()
        .add_file("big.bin", &incompressible(256 * 1024), 0o644)
        .finish();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    let mut gz = gz.finish().unwrap();
    gz.truncate(gz.len() * 3 / 4);
    let mut layer = blob(gz, index);
    layer.media_type = "application/vnd.oci.image.layer.v1.tar+gzip".into();
    layer
}

fn fake_mksquashfs(dir: &TempDir, body: &str) -> std::path::PathBuf {
    let path = dir.path().join("mksquashfs");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Assert that `result` is an error of layer `index`, and return its message.
fn assert_layer_error(result: crate::Result<()>, index: usize, sink: &str) -> String {
    match result {
        Err(Error::Layer {
            index: got,
            message,
            ..
        }) => {
            assert_eq!(got, index, "{sink}: wrong layer");
            message
        }
        other => panic!("{sink}: expected a layer error; got {other:?}"),
    }
}

#[test]
fn corrupt_layer_is_a_layer_error_for_every_sink() {
    let work = TempDir::new().unwrap();

    let (rx, total) = channel(vec![truncated_gzip_layer(0)]);
    let tar = work.path().join("out.tar");
    let message = assert_layer_error(
        crate::tar::write_tar_with_progress(rx, total, &tar, None),
        0,
        "tar",
    );
    // The header parses; the corruption surfaces while the data is copied,
    // where a read failure and a write failure look alike.
    assert_eq!(message, "cannot emit big.bin");

    let (rx, total) = channel(vec![truncated_gzip_layer(0)]);
    let dir = work.path().join("out-dir");
    let _ = assert_layer_error(
        crate::dir::write_dir_with_progress(rx, total, &dir, None),
        0,
        "dir",
    );

    // mksquashfs drains its input and then fails, as the real one does on a
    // truncated stream; the corrupt layer is still the error to report.
    let scripts = TempDir::new().unwrap();
    let mksquashfs = fake_mksquashfs(&scripts, "cat > /dev/null; exit 1");
    let (rx, total) = channel(vec![truncated_gzip_layer(0)]);
    let sqfs = work.path().join("out.sqfs");
    let _ = assert_layer_error(
        crate::squashfs::write_squashfs_with_progress(rx, total, &sqfs, Some(&mksquashfs), None),
        0,
        "squashfs",
    );
}

#[test]
fn mksquashfs_exiting_early_is_its_own_failure_not_a_broken_pipe() {
    // mksquashfs gives up after reading a little, as on a fatal error, so the
    // merge's later writes fail with a broken pipe.
    let scripts = TempDir::new().unwrap();
    let mksquashfs = fake_mksquashfs(
        &scripts,
        "head -c 4096 > /dev/null; echo 'FATAL ERROR: boom' >&2; exit 1",
    );
    let layer = LayerBuilder::new()
        .add_file("big.bin", &incompressible(1024 * 1024), 0o644)
        .finish();
    let (rx, total) = channel(vec![blob(layer, 0)]);
    let work = TempDir::new().unwrap();
    let out = work.path().join("out.sqfs");

    match crate::squashfs::write_squashfs_with_progress(rx, total, &out, Some(&mksquashfs), None) {
        Err(Error::Mksquashfs { stderr, .. }) => assert!(
            stderr.contains("FATAL ERROR: boom"),
            "stderr must be kept; got {stderr:?}"
        ),
        other => panic!("expected mksquashfs's failure; got {other:?}"),
    }
}

#[test]
fn entry_that_cannot_be_encoded_is_a_layer_error() {
    // A `..` component cannot be written to a tar archive; that is a property
    // of the layer, not a failure of the output.
    let layer = LayerBuilder::new()
        .add_file_ustar_split("", "a/../b", b"x", 0o644)
        .finish();
    let (rx, total) = channel(vec![blob(layer, 0)]);
    let work = TempDir::new().unwrap();
    let result = crate::tar::write_tar_with_progress(rx, total, &work.path().join("out.tar"), None);

    let err = result.expect_err("a `..` path cannot be emitted");
    assert!(
        matches!(err, Error::Layer { index: 0, .. }),
        "expected a layer error; got {err:?}"
    );
    assert!(
        error_chain(&err).contains("a/../b"),
        "{}",
        error_chain(&err)
    );
}

#[test]
fn unwritable_output_is_an_output_error() {
    let layer = LayerBuilder::new().add_file("f", b"x", 0o644).finish();
    let (rx, total) = channel(vec![blob(layer, 0)]);
    let out = Path::new("/nonexistent-ocirender-dir/out.tar");

    match crate::tar::write_tar_with_progress(rx, total, out, None) {
        Err(Error::Output { path, .. }) => assert_eq!(path, out),
        other => panic!("expected an output error; got {other:?}"),
    }
}
