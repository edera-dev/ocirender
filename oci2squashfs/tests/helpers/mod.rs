//! Shared test helpers for integration and regression tests.
#![allow(dead_code)]

use oci2squashfs::canonical::CanonicalTarHeader;
use oci2squashfs::image::LayerBlob;
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
