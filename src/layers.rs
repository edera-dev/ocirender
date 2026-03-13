//! Layer blob decompression.
//!
//! The single entry point, [`open_layer`], opens a layer blob from disk and
//! wraps it in a decompressor selected from the blob's OCI media type, returning
//! a [`DynArchive`] ready for entry iteration. No data is read from the blob
//! until the caller begins iterating entries.

use anyhow::{Result, bail};
use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};
use tar::Archive;

/// A type-erased decompressed tar archive.
///
/// The concrete reader type (gzip, zstd, etc.) is hidden behind a `Box<dyn
/// Read>` so that all callers can work with a single archive type regardless
/// of the layer's compression format.
pub type DynArchive = Archive<Box<dyn Read + Send + 'static>>;

/// Open a layer blob at `path` and return a decompressed tar archive.
///
/// The decompressor is selected from `media_type` using the OCI layer media
/// type conventions (e.g. `application/vnd.oci.image.layer.v1.tar+gzip`).
/// The legacy Docker media type
/// `application/vnd.docker.image.rootfs.diff.tar.gzip` is also accepted and
/// treated as gzip.
///
/// The returned archive has permissions, mtime, and xattr preservation
/// enabled, matching the settings expected by the merge pipeline and the
/// directory output sink.
///
/// Returns an error if the blob cannot be opened or if `media_type` is not
/// a recognised format.
pub fn open_layer(path: &Path, media_type: &str) -> Result<DynArchive> {
    let file = File::open(path)?;
    // BufReader amortises the many small reads the tar parser makes against
    // the underlying file descriptor.
    let buf = BufReader::new(file);
    let reader: Box<dyn Read + Send + 'static> = match media_type {
        t if t.ends_with("+gzip") || t == "application/vnd.docker.image.rootfs.diff.tar.gzip" => {
            Box::new(flate2::read::GzDecoder::new(buf))
        }
        t if t.ends_with("+zstd") => Box::new(zstd::stream::read::Decoder::new(buf)?),
        t if t.ends_with("+bzip2") => Box::new(bzip2::read::BzDecoder::new(buf)),
        t if t.ends_with("+xz") || t.ends_with("+lzma") => Box::new(xz2::read::XzDecoder::new(buf)),
        "application/vnd.oci.image.layer.v1.tar" => Box::new(buf),
        other => bail!("unsupported layer media type: {other}"),
    };
    let mut archive = Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_unpack_xattrs(true);
    Ok(archive)
}
