//! Expansion of PAX-format sparse files.
//!
//! GNU tar has four encodings for a sparse file. The old-GNU one (typeflag
//! `S`) is decoded by the `tar` crate itself, and
//! [`CanonicalTarHeader::from_entry`] captures such an entry as the regular
//! file it reads back as. The three PAX encodings, formats 0.0, 0.1 and 1.0,
//! are invisible to the `tar` crate: the entry looks like a regular file whose
//! data is only the stored chunks run together, and formats 0.1 and 1.0 also
//! replace its path with a `GNUSparseFile.<n>/<name>` placeholder, carrying
//! the real one in a `GNU.sparse.name` record.
//!
//! [`PaxSparse`] recognises these entries by their `GNU.sparse.*` PAX records
//! so the merge can treat each as the plain regular file it describes: keyed
//! on its real path for whiteouts and duplicates, and emitted with its holes
//! filled back in.
//!
//! [`CanonicalTarHeader::from_entry`]: crate::canonical::CanonicalTarHeader::from_entry

use anyhow::{Context, Result, anyhow, bail};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::canonical::CanonicalTarHeader;

/// A PAX-format sparse entry, as described by its `GNU.sparse.*` records.
#[derive(Debug)]
pub struct PaxSparse {
    /// The real path, for the formats that replace the header's path with a
    /// placeholder.
    name: Option<PathBuf>,
    /// The size of the file, holes included.
    real_size: u64,
    map: MapSource,
}

#[derive(Debug)]
enum MapSource {
    /// Formats 0.0 and 0.1 carry the `(offset, length)` map in PAX records.
    Records(Vec<(u64, u64)>),
    /// Format 1.0 stores the map at the start of the entry's data.
    InData,
}

impl PaxSparse {
    /// Recognise a PAX-format sparse entry from its PAX records, returning
    /// `None` for any other entry.
    pub fn detect(pax: &[(String, Vec<u8>)]) -> Result<Option<Self>> {
        let get = |key: &str| {
            pax.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_slice())
        };
        let num = |key: &str| {
            get(key)
                .map(|v| parse_decimal(v).with_context(|| format!("PAX record {key}")))
                .transpose()
        };

        let map = if get("GNU.sparse.major").is_some() {
            let version = (num("GNU.sparse.major")?, num("GNU.sparse.minor")?);
            if version != (Some(1), Some(0)) {
                bail!("unsupported PAX sparse format {version:?}");
            }
            MapSource::InData
        } else if let Some(map) = get("GNU.sparse.map") {
            // Format 0.1: one record of comma-separated offset,length pairs.
            let numbers = map
                .split(|&b| b == b',')
                .map(parse_decimal)
                .collect::<Result<Vec<_>>>()
                .context("PAX record GNU.sparse.map")?;
            if numbers.len() % 2 != 0 {
                bail!("PAX record GNU.sparse.map has an odd number of values");
            }
            MapSource::Records(numbers.chunks(2).map(|p| (p[0], p[1])).collect())
        } else if get("GNU.sparse.offset").is_some() {
            // Format 0.0: an offset and a numbytes record per chunk, in order.
            let all = |key: &str| {
                pax.iter()
                    .filter(|(k, _)| k == key)
                    .map(|(_, v)| parse_decimal(v).with_context(|| format!("PAX record {key}")))
                    .collect::<Result<Vec<_>>>()
            };
            let (offsets, lengths) = (all("GNU.sparse.offset")?, all("GNU.sparse.numbytes")?);
            if offsets.len() != lengths.len() {
                bail!("PAX sparse map has unpaired GNU.sparse.offset/numbytes records");
            }
            MapSource::Records(offsets.into_iter().zip(lengths).collect())
        } else {
            return Ok(None);
        };

        let real_size = match num("GNU.sparse.realsize")? {
            Some(size) => size,
            None => num("GNU.sparse.size")?.context("PAX sparse entry has no real size")?,
        };
        let name = get("GNU.sparse.name")
            .map(|v| std::str::from_utf8(v).map(PathBuf::from))
            .transpose()
            .context("PAX record GNU.sparse.name is not valid UTF-8")?;
        Ok(Some(Self {
            name,
            real_size,
            map,
        }))
    }

    /// The entry's real path, if its header path is only a placeholder.
    pub fn name(&self) -> Option<&Path> {
        self.name.as_deref()
    }

    /// Rewrite `canonical` to describe the regular file this entry stands
    /// for. A PAX `path` record is dropped along with the sparse ones when
    /// the real path comes from `GNU.sparse.name`, since it would only name
    /// the placeholder.
    pub fn rewrite_header(&self, canonical: &mut CanonicalTarHeader) {
        canonical.make_regular(self.real_size);
        if self.name.is_some() {
            canonical.pax_extensions.retain(|(k, _)| k != "path");
        }
    }

    /// Wrap `data`, the entry's stored bytes, in a reader that yields the
    /// file's full contents. For format 1.0 this first reads the map off the
    /// front of `data`.
    pub fn expand<R: Read>(self, mut data: R) -> Result<SparseReader<R>> {
        let chunks = match self.map {
            MapSource::Records(chunks) => chunks,
            MapSource::InData => read_data_map(&mut data)?,
        };
        let mut end = 0;
        for &(offset, len) in &chunks {
            let chunk_end = offset
                .checked_add(len)
                .ok_or_else(|| anyhow!("PAX sparse chunk overflows"))?;
            if offset < end || chunk_end > self.real_size {
                bail!(
                    "PAX sparse map is out of order or exceeds the real size {}",
                    self.real_size
                );
            }
            end = chunk_end;
        }
        Ok(SparseReader {
            inner: data,
            chunks: chunks.into_iter(),
            chunk: None,
            pos: 0,
            real_size: self.real_size,
        })
    }
}

/// Read a format 1.0 sparse map off the front of an entry's data: the chunk
/// count, then each chunk's offset and length, every number in decimal and
/// newline-terminated, the whole map zero-padded to a 512-byte boundary.
fn read_data_map<R: Read>(data: &mut R) -> Result<Vec<(u64, u64)>> {
    let mut consumed = 0u64;
    let mut next_number = || -> Result<u64> {
        let mut digits = Vec::new();
        loop {
            let mut byte = [0u8];
            data.read_exact(&mut byte)
                .context("PAX sparse map ends early")?;
            consumed += 1;
            match byte[0] {
                b'\n' => return parse_decimal(&digits).context("PAX sparse map"),
                b if digits.len() < 20 => digits.push(b),
                _ => bail!("PAX sparse map number is too long"),
            }
        }
    };
    let count = next_number()?;
    let mut chunks = Vec::new();
    for _ in 0..count {
        chunks.push((next_number()?, next_number()?));
    }
    let padding = consumed.next_multiple_of(512) - consumed;
    io::copy(&mut data.take(padding), &mut io::sink()).context("PAX sparse map padding")?;
    Ok(chunks)
}

fn parse_decimal(bytes: &[u8]) -> Result<u64> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| anyhow!("invalid number {:?}", String::from_utf8_lossy(bytes)))
}

/// A reader over a sparse file's full contents: zeros for the holes, and the
/// stored chunks, in order, from the wrapped reader.
pub struct SparseReader<R> {
    inner: R,
    chunks: std::vec::IntoIter<(u64, u64)>,
    /// The chunk being read, as its offset and the bytes left in it.
    chunk: Option<(u64, u64)>,
    pos: u64,
    real_size: u64,
}

impl<R> SparseReader<R> {
    /// Fill the front of `buf` with zeros up to offset `until`.
    fn hole(&mut self, buf: &mut [u8], until: u64) -> usize {
        let n = (until - self.pos).min(buf.len() as u64) as usize;
        buf[..n].fill(0);
        self.pos += n as u64;
        n
    }
}

impl<R: Read> Read for SparseReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.chunk.is_none() {
                self.chunk = self.chunks.next();
            }
            match self.chunk {
                Some((offset, _)) if self.pos < offset => return Ok(self.hole(buf, offset)),
                Some((_, 0)) => self.chunk = None,
                Some((offset, left)) => {
                    let want = left.min(buf.len() as u64) as usize;
                    let n = self.inner.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "sparse entry data ends before its map does",
                        ));
                    }
                    self.pos += n as u64;
                    self.chunk = Some((offset + n as u64, left - n as u64));
                    return Ok(n);
                }
                None => return Ok(self.hole(buf, self.real_size)),
            }
        }
    }
}
