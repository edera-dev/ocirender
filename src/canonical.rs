//! CanonicalTarHeader: a tar Header paired with its PAX extensions.
//! Ported from production vfs.rs — lets the `tar` crate own PAX serialization.

use anyhow::{Result, anyhow};
use std::borrow::Cow;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tar::{Builder, EntryType, Header};

#[derive(Clone, Debug)]
pub struct CanonicalTarHeader {
    pub header: Header,
    pub pax_extensions: Vec<(String, String)>,
}

impl CanonicalTarHeader {
    pub fn from_entry<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<Self> {
        let header = entry.header().clone();
        let pax_extensions = match entry.pax_extensions() {
            Err(e) => return Err(anyhow!("failed to read PAX extensions: {e}")),
            Ok(None) => vec![],
            Ok(Some(exts)) => {
                let mut pairs = Vec::new();
                for ext in exts {
                    let ext = ext.map_err(|e| anyhow!("invalid PAX extension: {e}"))?;
                    let key = ext
                        .key()
                        .map_err(|e| anyhow!("invalid PAX key: {e}"))?
                        .to_string();
                    let val = ext
                        .value()
                        .map_err(|e| anyhow!("invalid PAX value: {e}"))?
                        .to_string();
                    pairs.push((key, val));
                }
                pairs
            }
        };
        Ok(Self {
            header,
            pax_extensions,
        })
    }

    /// Return the entry path, preferring the PAX `path` extension over the
    /// USTAR header field.  This mirrors the PAX-aware treatment of `link_name`
    /// and is necessary for the same reason: the USTAR name field is limited to
    /// 100 bytes, so paths longer than that are silently truncated unless the
    /// full value is recovered from the PAX extension.
    pub fn path(&self) -> Result<Cow<'_, Path>> {
        if let Some((_, v)) = self.pax_extensions.iter().find(|(k, _)| k == "path") {
            return Ok(Cow::Owned(PathBuf::from(v)));
        }
        self.header
            .path()
            .map_err(|e| anyhow!("reading path from header: {e}"))
    }

    pub fn entry_type(&self) -> EntryType {
        self.header.entry_type()
    }

    /// Return the link target path, preferring the PAX `linkpath` extension
    /// over the USTAR header field. This is necessary because `Header::link_name()`
    /// only reads the raw 100-byte USTAR field and will return a truncated path
    /// for targets longer than 100 bytes, whereas the PAX extension carries the
    /// full value.
    pub fn link_name(&self) -> Result<Option<PathBuf>> {
        // Check PAX extensions first.
        if let Some((_, v)) = self.pax_extensions.iter().find(|(k, _)| k == "linkpath") {
            return Ok(Some(PathBuf::from(v)));
        }
        // Fall back to the USTAR field.
        Ok(self
            .header
            .link_name()
            .map_err(|e| anyhow!("reading link_name from header: {e}"))?
            .map(|p| p.into_owned()))
    }

    /// Return a clone of this header suitable for emitting a regular file at a
    /// caller-supplied path (used when promoting a surviving hardlink to a real
    /// file because its original target was suppressed by a whiteout).
    ///
    /// The clone:
    /// - sets the entry type to `Regular`
    /// - strips the `path` and `linkpath` PAX extensions (the path is provided
    ///   externally to `write_to_tar`, so a stale `path` extension would override it)
    /// - strips `GNU.sparse.*` extensions (we have the full materialised bytes,
    ///   so the entry is no longer sparse)
    pub fn clone_as_regular(&self) -> Self {
        let pax_extensions = self
            .pax_extensions
            .iter()
            .filter(|(k, _)| k != "path" && k != "linkpath" && !k.starts_with("GNU.sparse."))
            .cloned()
            .collect();
        let mut header = self.header.clone();
        header.set_entry_type(EntryType::Regular);
        Self {
            header,
            pax_extensions,
        }
    }

    /// Write a hardlink entry to `builder` pointing from `link_path` to
    /// `target_path`, using `self`'s header for inode metadata (mode, uid,
    /// gid, mtime).
    ///
    /// Unlike emitting a synthesized hardlink through `write_to_tar`, this
    /// method handles long paths and long link targets **entirely via PAX
    /// extensions** and calls `builder.append` directly.  This is required
    /// because `write_to_tar` uses `append_data`, which for GNU-format headers
    /// emits a GNU LongName auxiliary entry when the path exceeds 100 bytes.
    /// That LongName entry is emitted *after* any PAX extensions queued by a
    /// prior `append_pax_extensions` call, which causes the reader to consume
    /// the PAX extensions on the LongName entry rather than the main entry —
    /// silently losing the `linkpath` extension and the hardlink target.
    pub fn write_hardlink_to_tar<W: Write>(
        &self,
        link_path: &Path,
        target_path: &Path,
        builder: &mut Builder<W>,
    ) -> Result<()> {
        let link_path_str = link_path.to_string_lossy();
        let target_str = target_path.to_string_lossy();

        // Emit PAX extensions for path and/or linkpath when the values exceed
        // the 100-byte USTAR field limits.  These must be emitted before the
        // main entry; `append_pax_extensions` queues them for exactly that.
        let mut pax: Vec<(&str, Vec<u8>)> = Vec::new();
        if link_path_str.len() > 100 {
            pax.push(("path", link_path_str.as_bytes().to_vec()));
        }
        if target_str.len() > 100 {
            pax.push(("linkpath", target_str.as_bytes().to_vec()));
        }
        if !pax.is_empty() {
            builder
                .append_pax_extensions(pax.iter().map(|(k, v)| (*k, v.as_slice())))
                .map_err(|e| anyhow!("failed to append PAX extensions for hardlink: {e}"))?;
        }

        let mut header = self.header.clone();
        header.set_entry_type(EntryType::Link);
        header.set_size(0);

        // Write link_path into the USTAR name field (bytes 0..100), truncated.
        // The PAX `path` extension above carries the full value when needed.
        {
            let raw = header.as_mut_bytes();
            let field = &mut raw[0..100];
            field.fill(0);
            let bytes = link_path_str.as_bytes();
            let len = bytes.len().min(99);
            field[..len].copy_from_slice(&bytes[..len]);
        }

        // Write target_path into the USTAR linkname field (bytes 157..257),
        // truncated.  The PAX `linkpath` extension above carries the full value.
        {
            let raw = header.as_mut_bytes();
            let field = &mut raw[157..257];
            field.fill(0);
            let bytes = target_str.as_bytes();
            let len = bytes.len().min(100);
            field[..len].copy_from_slice(&bytes[..len]);
        }

        header.set_cksum();

        // Use builder.append (not append_data) so the tar crate does not
        // attempt to re-encode the path via GNU LongName, which would be
        // emitted between the already-queued PAX extensions and the main entry.
        builder
            .append(&mut header, &[] as &[u8])
            .map_err(|e| anyhow!("failed to append hardlink entry: {e}"))?;

        Ok(())
    }

    pub fn write_to_tar<W: Write, R: Read>(
        &self,
        path: &Path,
        data: R,
        builder: &mut Builder<W>,
    ) -> Result<()> {
        if !self.pax_extensions.is_empty() {
            builder
                .append_pax_extensions(
                    self.pax_extensions
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_bytes())),
                )
                .map_err(|e| anyhow!("failed to append PAX extensions: {e}"))?;
        }
        let mut header = self.header.clone();
        builder
            .append_data(&mut header, path, data)
            .map_err(|e| anyhow!("failed to append entry: {e}"))?;
        Ok(())
    }
}
