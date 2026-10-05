//! Direct tests for canonical.rs and layers.rs.
//!
//! canonical.rs: focuses on write_hardlink_to_tar's PAX extension ordering —
//! the property that linkpath lands on the main entry, not on a GNU LongName
//! auxiliary entry that would consume the extensions before the reader sees
//! the hardlink itself. Also which fields of a source entry's header must not
//! carry over when it is re-emitted at another path.
//!
//! layers.rs: the unsupported media type error path in open_layer.

use ocirender::canonical::CanonicalTarHeader;
use tar::{Builder, EntryType};
use tempfile::NamedTempFile;

// ── helpers ───────────────────────────────────────────────────────────────────

/// A minimal regular-file header standing in for the source entry whose inode
/// metadata the written entry reuses.
fn src_header() -> tar::Header {
    let mut src_header = tar::Header::new_ustar();
    src_header.set_entry_type(EntryType::Regular);
    src_header.set_size(0);
    src_header.set_mode(0o644);
    src_header.set_mtime(0);
    src_header.set_uid(0);
    src_header.set_gid(0);
    src_header.set_path("placeholder").unwrap();
    src_header.set_cksum();
    src_header
}

/// A source header as Go's `archive/tar` writes it for a 101..=256-byte path:
/// split across the USTAR prefix and name fields.
fn split_src_header() -> tar::Header {
    let mut h = src_header();
    let ustar = h.as_ustar_mut().unwrap();
    ustar.prefix.fill(0);
    ustar.prefix[..14].copy_from_slice(b"/nix/store/abc");
    h.set_cksum();
    h
}

/// Build a tar containing a single hardlink entry via `write_hardlink_to_tar`
/// and return the raw bytes.
fn hardlink_tar(link_path: &str, target_path: &str) -> Vec<u8> {
    hardlink_tar_from(
        CanonicalTarHeader {
            header: src_header(),
            pax_extensions: vec![],
        },
        link_path,
        target_path,
    )
}

/// As [`hardlink_tar`], with `canonical` supplying the inode metadata. Entry
/// type and link fields are overwritten by write_hardlink_to_tar.
fn hardlink_tar_from(canonical: CanonicalTarHeader, link_path: &str, target_path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut builder = Builder::new(&mut out);
    canonical
        .write_hardlink_to_tar(
            std::path::Path::new(link_path),
            std::path::Path::new(target_path),
            &mut builder,
        )
        .expect("write_hardlink_to_tar must succeed");
    builder.finish().unwrap();
    drop(builder);
    out
}

/// Build a tar containing a single empty entry written at `path` via
/// `write_to_tar` with `canonical`'s header, and return the raw bytes.
fn file_tar_from(canonical: CanonicalTarHeader, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut builder = Builder::new(&mut out);
    canonical
        .write_to_tar(std::path::Path::new(path), &[] as &[u8], &mut builder)
        .expect("write_to_tar must succeed");
    builder.finish().unwrap();
    drop(builder);
    out
}

/// Read the first non-PAX entry from a tar and return its CanonicalTarHeader.
fn first_main_entry(tar_bytes: &[u8]) -> CanonicalTarHeader {
    let mut archive = tar::Archive::new(tar_bytes);
    for mut entry in archive.entries().unwrap().flatten() {
        let et = entry.header().entry_type();
        if matches!(et, EntryType::XHeader | EntryType::XGlobalHeader) {
            continue;
        }
        return CanonicalTarHeader::from_entry(&mut entry).unwrap();
    }
    panic!("no main entry found in tar");
}

// ── canonical.rs: write_hardlink_to_tar ──────────────────────────────────────

/// Short paths (≤ 100 bytes) must produce a hardlink entry whose link_name()
/// returns the correct target without any PAX extensions being required.
#[test]
fn write_hardlink_short_paths_roundtrip() {
    let tar = hardlink_tar("link.txt", "target.txt");
    let entry = first_main_entry(&tar);

    assert_eq!(entry.entry_type(), EntryType::Link);
    assert_eq!(
        entry.path().unwrap().to_string_lossy(),
        "link.txt",
        "link path must round-trip"
    );
    assert_eq!(
        entry.link_name().unwrap().unwrap().to_string_lossy(),
        "target.txt",
        "link target must round-trip"
    );
}

/// A long link path (> 100 bytes) must round-trip correctly via the PAX `path`
/// extension and must land on the main hardlink entry, not on a GNU LongName
/// auxiliary entry that would be invisible to PAX-aware readers.
#[test]
fn write_hardlink_long_link_path_pax_roundtrip() {
    let long_link: String = "l".repeat(110);
    let tar = hardlink_tar(&long_link, "short_target.txt");
    let entry = first_main_entry(&tar);

    assert_eq!(
        entry.entry_type(),
        EntryType::Link,
        "entry must be a hardlink"
    );
    assert_eq!(
        entry.path().unwrap().to_string_lossy(),
        long_link.as_str(),
        "long link path must round-trip via PAX path extension"
    );
    assert_eq!(
        entry.link_name().unwrap().unwrap().to_string_lossy(),
        "short_target.txt",
    );
}

/// A long target path (> 100 bytes) must round-trip correctly via the PAX
/// `linkpath` extension on the main entry.  This is the exact failure mode
/// that was the production bug: the linkpath extension was being consumed by
/// the GNU LongName auxiliary entry instead of the hardlink entry.
#[test]
fn write_hardlink_long_target_path_pax_roundtrip() {
    let long_target: String = "t".repeat(110);
    let tar = hardlink_tar("link.txt", &long_target);
    let entry = first_main_entry(&tar);

    assert_eq!(
        entry.entry_type(),
        EntryType::Link,
        "entry must be a hardlink"
    );
    assert_eq!(
        entry.link_name().unwrap().unwrap().to_string_lossy(),
        long_target.as_str(),
        "long target path must round-trip via PAX linkpath extension on the main entry, \
         not consumed by a GNU LongName auxiliary entry"
    );
}

/// Both link path and target path long: both PAX extensions must land on the
/// main entry.
#[test]
fn write_hardlink_both_paths_long_pax_roundtrip() {
    let long_link: String = "l".repeat(110);
    let long_target: String = "t".repeat(110);
    let tar = hardlink_tar(&long_link, &long_target);
    let entry = first_main_entry(&tar);

    assert_eq!(entry.entry_type(), EntryType::Link);
    assert_eq!(
        entry.path().unwrap().to_string_lossy(),
        long_link.as_str(),
        "long link path must survive"
    );
    assert_eq!(
        entry.link_name().unwrap().unwrap().to_string_lossy(),
        long_target.as_str(),
        "long target path must survive"
    );
}

/// Verify that the PAX extensions appear on the main entry by counting entry
/// types in the raw tar.  A GNU LongName auxiliary entry (type 'L') must not
/// appear when paths are handled via PAX.
#[test]
fn write_hardlink_long_paths_produce_no_gnu_longname_entry() {
    let long_link: String = "l".repeat(110);
    let long_target: String = "t".repeat(110);
    let tar = hardlink_tar(&long_link, &long_target);

    let mut archive = tar::Archive::new(tar.as_slice());
    for entry in archive.entries().unwrap().flatten() {
        assert_ne!(
            entry.header().entry_type(),
            EntryType::GNULongName,
            "GNU LongName auxiliary entry must not be emitted when PAX extensions are used; \
             its presence would cause linkpath to be consumed by the wrong entry"
        );
    }
}

/// A link path of exactly 100 bytes fills the USTAR name field and gets no PAX
/// `path` extension, so it must be written in full rather than truncated to 99
/// bytes for a NUL terminator the field does not need.
#[test]
fn write_hardlink_100_byte_link_path_roundtrip() {
    let link: String = "l".repeat(100);
    let tar = hardlink_tar(&link, "target.txt");
    let entry = first_main_entry(&tar);

    assert_eq!(entry.entry_type(), EntryType::Link);
    assert_eq!(
        entry.path().unwrap().to_string_lossy(),
        link.as_str(),
        "a 100-byte link path must not lose its last byte"
    );
}

/// The source header's USTAR prefix must not leak onto the hardlink's path.
#[test]
fn write_hardlink_drops_source_prefix() {
    let canonical = CanonicalTarHeader {
        header: split_src_header(),
        pax_extensions: vec![],
    };
    let tar = hardlink_tar_from(canonical, "bin/link", "bin/target");
    let entry = first_main_entry(&tar);

    assert_eq!(
        entry.path().unwrap().to_string_lossy(),
        "bin/link",
        "stale USTAR prefix must not be prepended to the link path"
    );
}

/// `write_to_tar` at a path that fits in the USTAR name field must not keep
/// the source header's prefix.
#[test]
fn write_to_tar_drops_source_prefix() {
    let canonical = CanonicalTarHeader {
        header: split_src_header(),
        pax_extensions: vec![],
    };
    let tar = file_tar_from(canonical, "bin/file");
    let entry = first_main_entry(&tar);

    assert_eq!(
        entry.path().unwrap().to_string_lossy(),
        "bin/file",
        "stale USTAR prefix must not be prepended to the path"
    );
}

// ── layers.rs: open_layer ─────────────────────────────────────────────────────

#[test]
fn open_layer_unsupported_media_type_returns_error() {
    let f = NamedTempFile::new().unwrap();
    let result = ocirender::layers::open_layer(f.path(), "application/vnd.edera.custom+lz4");
    assert!(
        result.is_err(),
        "unsupported media type must return an error"
    );
    let err = result.err().unwrap();
    let msg = err.to_string();
    assert!(
        msg.contains("unsupported layer media type"),
        "error must identify the problem; got: {msg}"
    );
    assert!(
        msg.contains("application/vnd.edera.custom+lz4"),
        "error must include the bad media type string; got: {msg}"
    );
}

#[test]
fn open_layer_empty_media_type_returns_error() {
    let f = NamedTempFile::new().unwrap();
    let result = ocirender::layers::open_layer(f.path(), "");
    assert!(result.is_err(), "empty media type must return an error");
}
