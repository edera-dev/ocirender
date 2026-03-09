//! CanonicalTarHeader: a tar Header paired with its PAX extensions.
//! Ported from production vfs.rs — lets the `tar` crate own PAX serialization.

use anyhow::{anyhow, Result};
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

    pub fn path(&self) -> Result<Cow<'_, Path>> {
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
