# oci2squashfs

A Rust library and CLI tool for converting OCI container images directly into
squashfs filesystem images, without extracting layer contents to disk.

---

## Background: how squashfs images were built before

The previous approach (`protect-daemon`, `assemble.rs` + `vfs.rs` +
`backend.rs`) worked in two distinct phases:

**Phase 1 — Extract and build VFS.** Each layer tarball was decompressed and
its entries extracted to a temporary directory on disk, one file per entry.
A `VfsTree` was built in parallel: an in-memory tree of `VfsNode`s, each
holding the original `CanonicalTarHeader` (USTAR + PAX extensions) and a
pointer to the corresponding on-disk file. Whiteouts were applied to the VFS
as each layer was processed in manifest order, removing nodes (and their
backing files) from the tree. The `BatchedExtractor` optimised this somewhat
by buffering small files in memory and flushing to disk in batches, but the
end result was still every surviving file written to a temporary location on
disk.

**Phase 2 — Stream VFS to packer.** Once the VFS was fully assembled,
`VfsTree::write_to_tar` walked the tree and streamed a tar to the packer's
stdin — reading each file back off disk, pairing it with its in-memory
header, and writing the combined entry to `mksquashfs` (or `mkfs.erofs`).

This approach has a few significant costs:

- **Every file is written to disk twice.** First during extraction (phase 1),
  then read back during packing (phase 2). For a large image this means
  gigabytes of intermediate I/O that produces no lasting output.
- **Disk space proportional to image size.** The temporary directory must
  hold the fully extracted, merged filesystem for the duration of the pack
  step, requiring as much free space as the uncompressed image contents.
- **Extraction and packing are strictly sequential.** The packer cannot start
  until all layers are fully extracted and the VFS is complete, so there is
  no pipeline parallelism between decompression and compression.

---

## What this implementation does instead

This tool processes the OCI image's layer tarballs directly, merges them
using an explicit in-memory overlay algorithm, and streams the result
straight into `mksquashfs`'s stdin. Nothing is extracted to disk.

The pipeline for a single image conversion is:

```
layer blobs (gzip/zstd/etc)
        │
        │  decompress on the fly
        ▼
  tar entry stream  ─── overlay merge ───►  merged tar stream
  (per layer)            (in memory)                │
                                                    │  piped to stdin
                                                    ▼
                                               mksquashfs
                                                    │
                                                    ▼
                                            output .squashfs
```

The overlay merge processes layers in reverse order (newest first) so that
the first time a path is seen is always the winning version. Whiteouts are
tracked in a trie structure and checked as older layers are processed.
Hard links are deferred until all content has been emitted, then replayed
in layer order. At no point is the full merged tar materialised in memory
or on disk — entries stream from the layer blobs directly into
`mksquashfs`'s stdin pipe.

### Why this is better

- **No disk extraction.** Layer contents flow from the compressed blob
  directly to `mksquashfs` via a pipe. The temporary directory full of
  extracted files is eliminated entirely.
- **No intermediate disk space requirement.** Previously, free disk space
  proportional to the uncompressed image size was required to hold the
  extracted VFS backing store. Now only the compressed layer blobs need
  to be present.
- **Packing starts immediately.** Because the merged tar is streamed
  directly into `mksquashfs`, compression begins as soon as the first
  entry is emitted from the first layer. There is no waiting for all
  layers to be fully extracted before packing can start.
- **Whiteout and overlay logic is explicit and tested.** Rather than being
  a side effect of how the VFS tree was mutated during extraction, the
  overlay semantics are implemented as a distinct, self-contained algorithm
  with its own test suite, making the behaviour easier to reason about and
  verify.
- **Correct PAX header round-tripping.** Long paths, long symlink targets,
  and long hard link targets (all exceeding USTAR's 100-byte field limits)
  are preserved via PAX extended headers throughout the pipeline. The
  previous approach had a latent bug here where hard link targets over 100
  bytes were silently truncated, causing links to be dropped.

---

## OCI layer merging: how it works

OCI images are a stack of tar archives (layers). To reconstruct the final
filesystem, layers are applied oldest-first, with newer layers overwriting
older ones. Deletions are represented as *whiteout* files rather than actual
deletions, since a tar archive can only add entries.

This tool processes layers in **reverse order** (newest first) and uses three
tracking data structures:

### WhiteoutTracker

A path trie. When a whiteout entry is encountered in a layer, it is recorded
in the trie along with the index of the layer that declared it. Two kinds of
whiteout are handled:

- **Simple whiteout** (`.wh.<n>`): suppresses the specific named path
  from all older layers.
- **Opaque whiteout** (`.wh..wh..opq`): suppresses all content under the
  parent directory from older layers, replacing it entirely with the newer
  layer's content. Files added to the same directory in the *same* layer as
  the opaque whiteout are not suppressed.

Suppression is keyed on layer index: a whiteout declared in layer N only
suppresses entries from layers with index < N.

### EmittedPathTracker

A `HashSet<PathBuf>`. Once a path has been written to the output tar stream,
it is recorded here. If the same path appears again in an older layer it is
skipped — the newest version always wins.

### HardLinkTracker

Hard links cannot be emitted immediately because their targets may not have
been emitted yet (the target could live in an older layer that hasn't been
processed). Hard link entries are deferred into a `Vec`, then after all
layers have been processed they are replayed in ascending layer order. Before
emitting each link, the target path is verified against the emitted-path
tracker; if the target was suppressed by a whiteout or never appeared at all,
the link is silently dropped.

---

## Project structure

```
oci2squashfs/               # Library crate — all reusable logic
  src/
    lib.rs                  # Public async convert() entry point
    canonical.rs            # CanonicalTarHeader: USTAR header + PAX extensions
    image.rs                # Parse index.json / manifest.json, resolve layer blobs
    layers.rs               # Open a layer blob and dispatch decompression
    overlay.rs              # Core merge algorithm (merge_layers_into)
    squashfs.rs             # Spawn mksquashfs, pipe merged tar into stdin
    tracker.rs              # WhiteoutTracker, EmittedPathTracker, HardLinkTracker
    verify.rs               # Mount squashfs via squashfuse, diff against reference
  tests/
    helpers/
      mod.rs                # Shared test helpers: LayerBuilder, blob(), merge(), etc.
    integration.rs          # Synthetic tests for the merge pipeline
    regression.rs           # Per-bug regression tests from production verify runs

oci2squashfs_cli/           # Binary crate — CLI only, depends on the library
  src/
    main.rs                 # `oci2squashfs_cli convert` and `oci2squashfs_cli verify`
```

### Key design decisions

**`CanonicalTarHeader`** (`canonical.rs`) pairs a tar `Header` with its PAX
extension key-value pairs, captured at read time. All header access goes
through this type so that PAX values (which override the truncated USTAR
fields) are never accidentally ignored. In particular, `link_name()` on this
type checks `linkpath` in the PAX extensions before falling back to the
100-byte USTAR field — the `tar` crate's `Header::link_name()` does not do
this, which was the source of a production bug where hard link targets longer
than 100 bytes were silently truncated and the links dropped. This type is
derived from the same `CanonicalTarHeader` used in `protect-daemon`'s
`vfs.rs`, extended with a PAX-aware `link_name()` method.

**`spawn_blocking` discipline** (`lib.rs`): the public `convert()` function
is `async` so it can be called from an async-heavy codebase, but all tar I/O
runs inside a single `tokio::task::spawn_blocking` call. The synchronous
`tar` crate is used throughout — async tar crates have unacceptable
context-switch overhead for this workload.

**Streaming, not buffering** (`overlay.rs`, `squashfs.rs`): `merge_layers_into`
takes a `Write` sink rather than returning a `Vec<u8>`. `write_squashfs`
spawns `mksquashfs` and passes its stdin handle as that sink. File data flows
from the layer blobs through the decompressor and tar parser directly into
the pipe — neither the extracted files nor the merged tar are ever fully
materialised on disk or in memory.

---

## Input format

The tool expects a directory in OCI image layout format, as produced by
`docker save <image> | tar -x` or `skopeo copy docker://... oci:<dir>`:

```
./blobs/sha256/<digest>     # layer tarballs and config blobs
./index.json                # points to the manifest
./manifest.json             # (Docker save format fallback)
./oci-layout
./repositories
```

Supported layer compression formats: gzip, zstd, bzip2, xz/lzma, and
uncompressed. Format is determined from the `mediaType` field in the manifest,
not from file contents.

---

## CLI usage

### Convert

```bash
oci2squashfs_cli convert --image ./my-image-dir --output my-image.squashfs
```

### Verify

Mounts the squashfs via `squashfuse` and compares it against a reference
directory (e.g. a `docker export` snapshot extracted with `tar -x -p` as
root):

```bash
oci2squashfs_cli verify --squashfs my-image.squashfs --reference ./my-image-ref
```

The verify subcommand reports:
- Paths present only in the squashfs
- Paths present only in the reference
- Per-file differences in type, mode, uid, gid, size, symlink target, and
  SHA-256 content hash

Note that a `docker export` reference will always show a small number of
expected-only-in-reference paths (`.dockerenv`, `dev/console`, `dev/shm`,
`dev/pts`) and a small number of expected differences (`etc/hostname`,
`etc/hosts`, `etc/resolv.conf`, `etc/mtab`) because `docker export` captures
the live container filesystem including runtime-injected files and bind mounts.
These are not conversion bugs.

---

## Dependencies

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime for public API and subprocess waiting |
| `tar` | Synchronous tar reading and writing |
| `flate2` | gzip decompression |
| `zstd` | zstd decompression |
| `bzip2` | bzip2 decompression |
| `xz2` | xz/lzma decompression |
| `serde` + `serde_json` | JSON parsing for index.json / manifest |
| `sha2` | SHA-256 hashing in the verify subcommand |
| `tempfile` | Temporary mountpoint directory in verify |
| `clap` | CLI argument parsing |
| `anyhow` | Error handling throughout |

## System dependencies

- `mksquashfs` ≥ 4.6 (for functionally correct `-tar` stdin support); available
  in `squashfs-tools` on most distributions
- `squashfuse` (for the `verify` subcommand only)
- `fusermount` or `umount` (for squashfuse unmounting in verify)
