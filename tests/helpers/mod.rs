//! Shared test helpers for integration and regression tests.
#![allow(dead_code)]

use oci2squashfs::canonical::CanonicalTarHeader;
use oci2squashfs::image::LayerBlob;
use oci2squashfs::overlay::normalize_path;
use std::io::{Cursor, Write};
use tar::{Archive, Builder, EntryType, Header};

// ─── LayerBuilder ────────────────────────────────────────────────────────────

pub struct LayerBuilder {
    inner: Builder<Vec<u8>>,
}

impl LayerBuilder {
    pub fn new() -> Self {
        Self {
            inner: Builder::new(Vec::new()),
        }
    }

    pub fn add_file(mut self, path: &str, data: &[u8], mode: u32) -> Self {
        if path.len() > 99 {
            self.inner
                .append_pax_extensions([("path", path.as_bytes())])
                .unwrap();
        }
        let mut hdr = Header::new_ustar();
        hdr.set_path(truncate(path, 99)).unwrap();
        hdr.set_size(data.len() as u64);
        hdr.set_mode(mode);
        hdr.set_mtime(0);
        hdr.set_uid(0);
        hdr.set_gid(0);
        hdr.set_cksum();
        self.inner.append(&hdr, Cursor::new(data)).unwrap();
        self
    }

    pub fn add_dir(mut self, path: &str) -> Self {
        if path.len() > 99 {
            self.inner
                .append_pax_extensions([("path", path.as_bytes())])
                .unwrap();
        }
        let mut hdr = Header::new_ustar();
        hdr.set_path(truncate(path, 99)).unwrap();
        hdr.set_entry_type(EntryType::Directory);
        hdr.set_size(0);
        hdr.set_mode(0o755);
        hdr.set_mtime(0);
        hdr.set_uid(0);
        hdr.set_gid(0);
        hdr.set_cksum();
        self.inner.append(&hdr, Cursor::new(b"" as &[u8])).unwrap();
        self
    }

    pub fn add_symlink(mut self, path: &str, target: &str) -> Self {
        let mut pax: Vec<(&str, &[u8])> = Vec::new();
        if path.len() > 99 {
            pax.push(("path", path.as_bytes()));
        }
        if target.len() > 99 {
            pax.push(("linkpath", target.as_bytes()));
        }
        if !pax.is_empty() {
            self.inner.append_pax_extensions(pax).unwrap();
        }
        let mut hdr = Header::new_ustar();
        hdr.set_path(truncate(path, 99)).unwrap();
        hdr.set_entry_type(EntryType::Symlink);
        hdr.set_link_name(truncate(target, 99)).ok();
        hdr.set_size(0);
        hdr.set_mode(0o777);
        hdr.set_mtime(0);
        hdr.set_uid(0);
        hdr.set_gid(0);
        hdr.set_cksum();
        self.inner.append(&hdr, Cursor::new(b"" as &[u8])).unwrap();
        self
    }

    pub fn add_hardlink(mut self, path: &str, target: &str) -> Self {
        let mut pax: Vec<(&str, &[u8])> = Vec::new();
        if path.len() > 99 {
            pax.push(("path", path.as_bytes()));
        }
        if target.len() > 99 {
            pax.push(("linkpath", target.as_bytes()));
        }
        if !pax.is_empty() {
            self.inner.append_pax_extensions(pax).unwrap();
        }
        let mut hdr = Header::new_ustar();
        hdr.set_path(truncate(path, 99)).unwrap();
        hdr.set_entry_type(EntryType::Link);
        hdr.set_link_name(truncate(target, 99)).ok();
        hdr.set_size(0);
        hdr.set_mode(0o644);
        hdr.set_mtime(0);
        hdr.set_uid(0);
        hdr.set_gid(0);
        hdr.set_cksum();
        self.inner.append(&hdr, Cursor::new(b"" as &[u8])).unwrap();
        self
    }

    /// Add a FIFO (named pipe) entry.
    pub fn add_fifo(mut self, path: &str) -> Self {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(tar::EntryType::Fifo);
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        // Use the low-level append (not append_data) so the header is written
        // exactly as we built it.
        self.inner.append(&header, &[] as &[u8]).unwrap();
        self
    }

    /// Add a regular file entry whose path is written verbatim into the USTAR
    /// name field, bypassing the `tar` crate's `set_path` which strips leading
    /// `./`.  Use this to simulate tarballs produced by `docker save` and
    /// similar tools that always emit `./`-prefixed paths.
    ///
    /// `path` must be ≤ 99 bytes (i.e. fit in the USTAR name field after the
    /// NUL terminator); this is sufficient for the deduplication tests.
    pub fn add_file_dotslash(mut self, path: &str, data: &[u8], mode: u32) -> Self {
        assert!(
            path.len() <= 99,
            "add_file_dotslash: path must be ≤ 99 bytes, got {}",
            path.len()
        );
        let mut header = tar::Header::new_gnu();
        // Write the path directly into the raw USTAR name field (bytes 0..100)
        // so the ./ prefix is preserved.  set_path() would strip it.
        {
            let raw = header.as_mut_bytes();
            let field = &mut raw[0..100];
            field.fill(0);
            let bytes = path.as_bytes();
            field[..bytes.len()].copy_from_slice(bytes);
        }
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        self.inner.append(&header, data).unwrap();
        self
    }

    pub fn add_whiteout(self, dir: &str, name: &str) -> Self {
        let path = if dir.is_empty() {
            format!(".wh.{name}")
        } else {
            format!("{dir}/.wh.{name}")
        };
        self.add_file(&path, b"", 0o644)
    }

    pub fn add_opaque_whiteout(self, dir: &str) -> Self {
        self.add_file(&format!("{dir}/.wh..wh..opq"), b"", 0o644)
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.inner.finish().unwrap();
        self.inner.into_inner().unwrap()
    }
}

/// Truncate a string to at most `max_chars` characters.
fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

// ─── Blob / merge helpers ─────────────────────────────────────────────────────

pub fn blob(bytes: Vec<u8>, index: usize) -> LayerBlob {
    use tempfile::NamedTempFile;
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(&bytes).unwrap();
    let (_, path) = f.keep().unwrap();
    LayerBlob {
        path,
        media_type: "application/vnd.oci.image.layer.v1.tar".into(),
        index,
    }
}

pub fn merge(layers: Vec<LayerBlob>) -> Vec<u8> {
    let mut out = Vec::new();
    oci2squashfs::overlay::merge_layers_into(layers, &mut out).unwrap();
    out
}

// ─── Tar inspection helpers ───────────────────────────────────────────────────

pub fn paths_in_tar(tar_bytes: &[u8]) -> Vec<String> {
    let mut archive = Archive::new(Cursor::new(tar_bytes));
    archive
        .entries()
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            !matches!(
                e.header().entry_type(),
                EntryType::XHeader | EntryType::XGlobalHeader
            )
        })
        .map(|e| e.path().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// Read the symlink target for `link_path`, preferring PAX `linkpath`.
pub fn symlink_target_in_tar(tar_bytes: &[u8], link_path: &str) -> Option<String> {
    let mut archive = Archive::new(Cursor::new(tar_bytes));
    for mut entry in archive.entries().unwrap().flatten() {
        let canonical = CanonicalTarHeader::from_entry(&mut entry).ok()?;
        if canonical.entry_type() != EntryType::Symlink {
            continue;
        }
        if canonical.path().unwrap().to_string_lossy() != link_path {
            continue;
        }
        return canonical
            .link_name()
            .ok()
            .flatten()
            .map(|p| p.to_string_lossy().into_owned());
    }
    None
}

/// Read the hard link target for `link_path`, preferring PAX `linkpath`.
pub fn hardlink_target_in_tar(tar_bytes: &[u8], link_path: &str) -> Option<String> {
    let mut archive = Archive::new(Cursor::new(tar_bytes));
    for mut entry in archive.entries().unwrap().flatten() {
        let canonical = CanonicalTarHeader::from_entry(&mut entry).ok()?;
        if canonical.entry_type() != EntryType::Link {
            continue;
        }
        if canonical.path().unwrap().to_string_lossy() != link_path {
            continue;
        }
        return canonical
            .link_name()
            .ok()
            .flatten()
            .map(|p| p.to_string_lossy().into_owned());
    }
    None
}

/// Read the file at path `path`, returning the file contents in bytes
pub fn file_contents_in_tar(tar_bytes: &[u8], path: &str) -> Option<Vec<u8>> {
    let cursor = std::io::Cursor::new(tar_bytes);
    let mut archive = tar::Archive::new(cursor);
    let want = std::path::Path::new(path);

    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        let entry_path = entry.path().ok()?.into_owned();
        let entry_path = normalize_path(&entry_path);
        if entry_path == want {
            if entry.header().entry_type().is_file() {
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut buf).ok()?;
                return Some(buf);
            }
            return None;
        }
    }

    None
}

/// Return the Unix permission bits for the first entry in `tar_bytes` whose
/// normalised path matches `path`, or `None` if no such entry exists.
pub fn file_mode_in_tar(tar_bytes: &[u8], path: &str) -> Option<u32> {
    let mut archive = tar::Archive::new(tar_bytes);
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        let p = entry.path().unwrap().into_owned();
        let normalised = p
            .to_string_lossy()
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_string();
        if normalised == path {
            return Some(entry.header().mode().unwrap());
        }
    }
    None
}

/// Return the `EntryType` for the first entry in `tar_bytes` whose normalised
/// path matches `path`, or `None` if no such entry exists.
pub fn entry_type_in_tar(tar_bytes: &[u8], path: &str) -> Option<tar::EntryType> {
    let mut archive = tar::Archive::new(tar_bytes);
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        let p = entry.path().unwrap().into_owned();
        let normalised = p
            .to_string_lossy()
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_string();
        if normalised == path {
            return Some(entry.header().entry_type());
        }
    }
    None
}
