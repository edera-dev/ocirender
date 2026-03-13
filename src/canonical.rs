//! PAX-aware tar header type used throughout the merge pipeline.
//!
//! The [`tar`] crate's [`tar::Header`] type exposes only the fixed-width USTAR
//! fields, which are limited to 100 bytes for paths and link targets. The PAX
//! extended header format overcomes these limits by prepending a variable-length
//! key-value block before the main header; values in that block take precedence
//! over the corresponding truncated USTAR fields.
//!
//! [`CanonicalTarHeader`] pairs a [`tar::Header`] with its PAX extensions,
//! captured together at read time, and provides accessors that always prefer
//! the PAX value when one is present. Using this type throughout the pipeline
//! ensures that long paths and long link targets are never accidentally read
//! from the truncated USTAR fields.

use anyhow::{Result, anyhow};
use std::borrow::Cow;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tar::{Builder, EntryType, Header};

/// A tar header paired with its PAX extended header key-value pairs.
///
/// All path and link-target accessors on this type prefer the PAX value over
/// the corresponding USTAR field, avoiding silent truncation for values longer
/// than 100 bytes.
///
/// Construct with [`CanonicalTarHeader::from_entry`] while iterating over a
/// [`tar::Archive`]. The PAX extensions must be captured at that point because
/// they are part of the entry's data stream and are not accessible after the
/// entry is consumed.
#[derive(Clone, Debug)]
pub struct CanonicalTarHeader {
    /// The underlying USTAR header block.
    pub header: Header,
    /// PAX extended header key-value pairs, in the order they appeared in the
    /// archive. Empty if the entry had no PAX extensions.
    pub pax_extensions: Vec<(String, String)>,
}

impl CanonicalTarHeader {
    /// Capture the header and PAX extensions from a tar archive entry.
    ///
    /// Must be called before the entry's data is read, since the PAX extension
    /// block is part of the same data stream.
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
    /// USTAR header field.
    ///
    /// The USTAR name field is limited to 100 bytes, so paths longer than that
    /// are silently truncated in the raw header. The PAX `path` extension
    /// carries the full value and must be checked first.
    pub fn path(&self) -> Result<Cow<'_, Path>> {
        if let Some((_, v)) = self.pax_extensions.iter().find(|(k, _)| k == "path") {
            return Ok(Cow::Owned(PathBuf::from(v)));
        }
        self.header
            .path()
            .map_err(|e| anyhow!("reading path from header: {e}"))
    }

    /// Return the entry type (regular file, directory, symlink, hardlink, etc.).
    pub fn entry_type(&self) -> EntryType {
        self.header.entry_type()
    }

    /// Return the link target path, preferring the PAX `linkpath` extension
    /// over the USTAR header field.
    ///
    /// The USTAR linkname field is limited to 100 bytes. The `tar` crate's
    /// [`tar::Header::link_name`] reads only that field and silently returns a
    /// truncated path for targets longer than 100 bytes. The PAX `linkpath`
    /// extension carries the full value and must be checked first.
    ///
    /// Returns `Ok(None)` for entry types that have no link target.
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
    /// caller-supplied path.
    ///
    /// Used when promoting a surviving hardlink to a standalone regular file
    /// because its original target was suppressed by a whiteout. The clone:
    ///
    /// - sets the entry type to [`EntryType::Regular`]
    /// - strips the `path` and `linkpath` PAX extensions, since the path is
    ///   supplied externally to [`write_to_tar`] and a stale `path` extension
    ///   would override it
    /// - strips `GNU.sparse.*` extensions, since the promoted entry is written
    ///   from fully materialised bytes and is no longer sparse
    ///
    /// All other inode metadata (mode, uid, gid, mtime) is preserved.
    ///
    /// [`write_to_tar`]: CanonicalTarHeader::write_to_tar
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
    /// Long paths and long link targets are handled **entirely via PAX
    /// extensions** rather than through [`tar::Builder::append_data`]. This is
    /// required because `append_data` emits a GNU LongName auxiliary entry for
    /// paths exceeding 100 bytes, and that auxiliary entry is inserted *after*
    /// any PAX extensions already queued by `append_pax_extensions` — causing
    /// the reader to consume the PAX extensions against the LongName entry
    /// rather than the main entry, silently losing the `linkpath` extension and
    /// dropping the hardlink.
    ///
    /// Instead this method writes PAX extensions for `path` and `linkpath`
    /// only when needed (i.e. when the value exceeds 100 bytes), then calls
    /// [`tar::Builder::append`] directly with a manually constructed header,
    /// bypassing the GNU LongName path entirely.
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

        // Write link_path into the USTAR name field (bytes 0..100), truncated
        // to 99 bytes to leave room for a NUL terminator. The PAX `path`
        // extension above carries the full value when needed.
        {
            let raw = header.as_mut_bytes();
            let field = &mut raw[0..100];
            field.fill(0);
            let bytes = link_path_str.as_bytes();
            let len = bytes.len().min(99);
            field[..len].copy_from_slice(&bytes[..len]);
        }

        // Write target_path into the USTAR linkname field (bytes 157..257),
        // truncated to 100 bytes. The linkname field does not require a NUL
        // terminator in POSIX, so the full 100 bytes are usable. The PAX
        // `linkpath` extension above carries the full value when needed.
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

    /// Write this entry to `builder` at `path`, streaming `data` as the file
    /// content.
    ///
    /// Any PAX extensions stored on this header are emitted before the main
    /// entry via [`tar::Builder::append_pax_extensions`]. For hardlink entries,
    /// use [`write_hardlink_to_tar`] instead — see that method's documentation
    /// for why `append_data` is not suitable for hardlinks with long paths.
    ///
    /// [`write_hardlink_to_tar`]: CanonicalTarHeader::write_hardlink_to_tar
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
