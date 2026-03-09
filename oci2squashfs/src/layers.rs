//! Open a layer blob and return a decompressed tar::Archive.

use anyhow::{bail, Result};
use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::Path,
};
use tar::Archive;

/// A type-erased decompressed tar reader.
pub type DynArchive = Archive<Box<dyn Read + Send + 'static>>;

pub fn open_layer(path: &Path, media_type: &str) -> Result<DynArchive> {
    let file = File::open(path)?;
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

/// Read all raw bytes of a layer entry into memory.
/// Returns (header_bytes, data_bytes) — header_bytes is the raw 512-byte block(s).
pub fn read_entry_data(entry: &mut tar::Entry<impl Read>) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    io::copy(entry, &mut buf)?;
    Ok(buf)
}
