# ocirender

A Rust library and CLI tool for converting OCI container images directly into
squashfs filesystem images, plain tar archives, or extracted directories —
without extracting layer contents to intermediate disk storage.

The library (`ocirender`) is the primary product. The CLI (`ocirender-cli`) is
built as a convenience wrapper for development, testing, and validation — it
demonstrates correct usage of the library's streaming API and serves as a
reference for callers building their own tooling on top of it.

---

## What this implementation does

OCI images are a stack of compressed tar archives (layers). This tool processes
those layer tarballs directly, merges them using an explicit in-memory overlay
algorithm, and streams the result to the chosen output sink. Nothing is
extracted to a temporary directory.

The pipeline for a single image conversion is:

```
layer blobs (gzip/zstd/etc)
        │
        │  decompress on the fly
        ▼
  tar entry stream  ─── overlay merge ───►  merged tar stream
  (per layer)            (in memory)                │
                                                    ├─► mksquashfs stdin → .squashfs
                                                    ├─► file             → .tar
                                                    └─► tar::unpack      → directory
```

The overlay merge processes layers in **reverse order** (newest first) so that
the first time a path is seen is always the winning version. Whiteouts are
tracked in a trie structure and checked as older layers are processed. Hard
links whose targets were suppressed by a whiteout are promoted to standalone
regular files. At no point is the full merged tar materialised in memory or
on disk — entries stream from the layer blobs directly into the output sink.

### Goals

- **No disk extraction.** Layer contents flow from the compressed blob directly
  to the output sink. No temporary directory of extracted files is required.
- **Minimal intermediate disk space.** Only the compressed layer blobs need to
  be present. The output is written directly to its final destination.
- **Output starts immediately.** Streaming begins as soon as the first entry is
  emitted from the first (newest) layer. There is no waiting for all layers to
  be processed before output can start.
- **Download-parallel processing.** The streaming API accepts layers in any
  arrival order as downloads complete. The merge engine resequences them
  internally and processes each layer as soon as its turn arrives, keeping the
  output sink busy while remaining layers are still in flight.
- **Explicit, tested overlay semantics.** Whiteout handling and overlay
  application are implemented as a self-contained algorithm with their own test
  suite, making the behaviour straightforward to reason about and verify.
- **Correct PAX header round-tripping.** Long paths, long symlink targets, and
  long hard link targets (all exceeding USTAR's 100-byte field limits) are
  preserved via PAX extended headers throughout the pipeline. A prior
  implementation had a latent bug where hard link targets over 100 bytes were
  silently truncated and the links dropped.

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

- **Simple whiteout** (`.wh.<name>`): suppresses the specific named path
  from all older layers.
- **Opaque whiteout** (`.wh..wh..opq`): suppresses all content under the
  parent directory from older layers, replacing it entirely with the newer
  layer's content. Files added to the same directory in the *same* layer as
  the opaque whiteout are not suppressed.

Suppression is keyed on layer index: a whiteout declared in layer N only
suppresses entries from layers with index < N.

### EmittedPathTracker

A `HashSet<PathBuf>`. Once a path has been written to the output stream, it is
recorded here. If the same path appears again in an older layer it is skipped —
the newest version always wins.

### HardLinkTracker

Hard links cannot be emitted immediately because their targets may not have
been seen yet (the target could live in an older layer that hasn't been
processed). Hard link entries are deferred and replayed in ascending layer
order after all layers have been processed.

Two cases are handled at replay time:

- **Normal deferred hardlink**: the target path was emitted normally. The link
  is emitted pointing at the target. If the target was suppressed by a whiteout
  or never appeared at all, the link is dropped.
- **Promoted hardlink**: the target path was suppressed by a higher-layer
  whiteout, but the hardlink path itself is live. In this case the link is
  *promoted* to a standalone regular file using the suppressed target's buffered
  content. If multiple hardlinks share the same suppressed target, they form a
  group: the oldest member is emitted as the regular file and the rest are
  emitted as hardlinks to it, preserving inode-sharing semantics.

---

## Library API

### `ImageSpec`

`ImageSpec` is a direction-neutral description of an image's format, location,
and any format-specific options. It is used both as a conversion output target
and as a verification input source.

```rust
pub enum ImageSpec {
    Squashfs { path: PathBuf, binpath: Option<PathBuf> },
    Tar      { path: PathBuf },
    Dir      { path: PathBuf },
}
```

### Batch conversion

```rust
// Convert an OCI image directory to any supported output format.
pub async fn convert(image_dir: &Path, spec: ImageSpec) -> Result<()>

// Named convenience wrappers (call through to convert()).
pub async fn convert_mksquashfs(image_dir, output, squashfs_binpath) -> Result<()>
pub async fn convert_tar(image_dir, output_tar) -> Result<()>
pub async fn convert_dir(image_dir, output_dir) -> Result<()>
```

### Streaming conversion

For use when layers are being downloaded concurrently. Layers may be delivered
in any order; the merge engine resequences them and processes each as soon as
its turn arrives.

```rust
// Streaming convenience wrappers.
pub async fn convert_mksquashfs_streaming(receiver, total_layers, output, binpath) -> Result<()>
pub async fn convert_tar_streaming(receiver, total_layers, output_tar) -> Result<()>
pub async fn convert_dir_streaming(receiver, total_layers, output_dir) -> Result<()>
```

#### `StreamingPacker`

For finer-grained control — including per-layer progress events and the ability
to signal download errors — use `StreamingPacker` directly:

```rust
// Construct and immediately begin processing. The output sink is opened and
// (for squashfs) mksquashfs is spawned at construction time.
let packer = StreamingPacker::new(layer_metas, spec, progress_tx);

// Notify the packer as each layer blob finishes downloading.
// May be called from any task in any order.
packer.notify_layer_ready(index, path).await?;

// Signal a download failure, causing the merge to abort.
packer.notify_error(err).await;

// Wait for the output to be finalised.
packer.finish().await?;
```

`progress_tx`, if supplied, receives `PackerProgress::LayerStarted(i)` and
`PackerProgress::LayerFinished(i)` events from the merge engine as each layer
is processed. These events are emitted regardless of output format.

For best throughput when downloading concurrently, deliver layers in descending
index order (newest first) with bounded concurrency. The merge engine processes
layers newest-first, so delivering the highest-index layer first minimises the
time the output sink spends waiting.

### Verification

```rust
pub fn verify(spec: ImageSpec, reference: &Path, ignore_ownership: bool) -> Result<VerifyReport>
```

Compares a generated image against a reference directory:

- `ImageSpec::Squashfs` — mounts via `squashfuse`, diffs the mount against
  `reference`, then unmounts.
- `ImageSpec::Dir` — diffs the directory directly against `reference`.
- `ImageSpec::Tar` — returns `Err`. Extract to a directory first with
  `convert-dir`, then use `ImageSpec::Dir`.

`ignore_ownership` skips uid/gid comparison. This is appropriate when comparing
a squashfs image (which preserves ownership from tar headers) against a
directory unpacked without root privileges (where `chown` silently fails).

`VerifyReport` contains:
- `only_in_generated` — paths present in the generated image but absent from
  the reference
- `only_in_reference` — paths present in the reference but absent from the
  generated image
- `differences` — per-file differences in type, mode, uid, gid, size, symlink
  target, and SHA-256 content hash

---

## Project structure

This is a Cargo workspace with two crates:

```
Cargo.toml                  # workspace root (also the library crate)
src/
  lib.rs          # ImageSpec, StreamingPacker, convert(), public async API
  canonical.rs    # CanonicalTarHeader: USTAR header + PAX extensions
  image.rs        # Parse index.json / manifest.json, resolve layer blobs
  layers.rs       # Open a layer blob and dispatch decompression
  overlay.rs      # Core merge algorithm (merge_layers_into_streaming)
  squashfs.rs     # Spawn mksquashfs, pipe merged tar into stdin
  tar.rs          # Write merged tar directly to a file
  dir.rs          # Unpack merged tar directly into a directory
  tracker.rs      # WhiteoutTracker, EmittedPathTracker, HardLinkTracker
  verify.rs       # Diff a generated image against a reference directory
tests/
  helpers/
    mod.rs        # LayerBuilder, blob(), merge(), tar inspection helpers
  integration.rs  # Synthetic tests for the merge pipeline
  regression.rs   # Per-bug regression tests from production verify runs
  streaming.rs    # Streaming merge and StreamingPacker tests

ocirender-cli/              # CLI crate (depends on the library)
  src/
    main.rs                 # CLI entry point and subcommand dispatch
    registry/
      client.rs             # OCI registry HTTP client (fetch, pull)
      auth.rs               # Bearer token challenge parsing
      credentials.rs        # ~/.docker/config.json credential store
      reference.rs          # Image reference parsing and normalisation
  tests/
    e2e.rs                  # End-to-end CLI tests
    fixtures/               # Synthetic OCI image fixture generator
```

### Key design decisions

**`CanonicalTarHeader`** (`canonical.rs`) pairs a tar `Header` with its PAX
extension key-value pairs, captured at read time. All header access goes
through this type so that PAX values (which override the truncated USTAR
fields) are never accidentally ignored. In particular, `link_name()` checks
`linkpath` in the PAX extensions before falling back to the 100-byte USTAR
field — the `tar` crate's `Header::link_name()` does not do this.

**`spawn_blocking` discipline** (`lib.rs`): all public async entry points
dispatch synchronous tar I/O into `tokio::task::spawn_blocking`. The
synchronous `tar` crate is used throughout — async tar crates have
unacceptable context-switch overhead for this workload.

**Streaming as the canonical implementation** (`overlay.rs`): the single
implementation is `merge_layers_into_streaming`, which accepts layers via a
`std::sync::mpsc` channel and a `Write` sink. The batch `convert()` path
pre-loads all layers into a channel and delegates to the same function. The
output format (squashfs, tar, directory) affects only what is passed as the
`Write` sink — the merge algorithm itself is format-agnostic.

**`ImageSpec` is direction-neutral**: the same type describes a conversion
output target and a verification input source. This avoids a parallel set of
types for read vs. write contexts while keeping the API surface small.

**CLI registry client** (`ocirender-cli/src/registry/`): the library
deliberately has no HTTP or credential dependencies. Registry pulling lives
entirely in the CLI crate. The client uses the same worker-queue architecture
recommended for `StreamingPacker` callers: N async tasks drain a shared queue
ordered highest-index-first, each calling `notify_layer_ready` as its download
completes.

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
with magic byte detection as a fallback for layouts that omit `LayerSources`.

---

## CLI usage

The CLI is a thin wrapper around the library intended for development and
validation. It is not a production container runtime client.

### Store registry credentials

```bash
# ghcr.io — username is your GitHub username, password is a PAT with
# read:packages scope or the output of `gh auth token`.
gh auth token | ocirender login ghcr.io -u YOUR_USERNAME --password-stdin

# Docker Hub
ocirender login registry-1.docker.io -u YOUR_USERNAME --password-stdin

# Credentials are stored in ~/.docker/config.json and are shared with
# Docker, crane, skopeo, and other OCI tools.
```

### Fetch an image to an OCI layout directory

```bash
# Full fetch (all layer blobs)
ocirender fetch --image alpine:latest --output ./alpine-layout

# Manifest only (no layer blobs) — useful for validating manifest parsing
ocirender fetch --image alpine:latest --output ./alpine-layout --manifest-only

# Private registry
ocirender fetch --image ghcr.io/myorg/myimage:tag --output ./my-layout
```

### Convert a local OCI layout to squashfs

```bash
ocirender convert-squashfs --image ./my-image-dir --output my-image.squashfs
# With an explicit mksquashfs binary:
ocirender convert-squashfs --image ./my-image-dir --output my-image.squashfs \
    --mksquashfs /usr/local/bin/mksquashfs
```

### Convert to tar or directory

```bash
ocirender convert-tar --image ./my-image-dir --output my-image.tar
ocirender convert-dir --image ./my-image-dir --output ./my-image-root
```

### Pull directly (fetch + convert in one step)

`pull` pipelines layer downloads with assembly via `StreamingPacker`, avoiding
any intermediate OCI layout directory on disk.

```bash
ocirender pull --image alpine:latest --output-squashfs alpine.squashfs
ocirender pull --image alpine:latest --output-tar alpine.tar
ocirender pull --image alpine:latest --output-dir ./alpine-root
```

### Verify

Compare a generated squashfs against a reference directory:

```bash
ocirender verify --squashfs my-image.squashfs --reference ./my-image-ref
```

Compare a generated directory against a reference directory:

```bash
ocirender verify --dir ./my-image-root --reference ./my-image-ref
```

Pass `--ignore-ownership` when comparing a squashfs against a directory
unpacked without root privileges.

The verify subcommand reports:
- Paths present only in the generated image (`+`)
- Paths present only in the reference (`-`)
- Per-file differences in type, mode, uid, gid, size, symlink target, and
  SHA-256 content hash (`~`)

Exits 0 if no differences are found, 1 otherwise.

Note that a `docker export` reference will always show a small number of
expected-only-in-reference paths (`.dockerenv`, `dev/console`, `dev/shm`,
`dev/pts`) and a small number of expected differences (`etc/hostname`,
`etc/hosts`, `etc/resolv.conf`, `etc/mtab`) because `docker export` captures
the live container filesystem including runtime-injected files and bind mounts.
These are not conversion bugs.

### Registry mirror

To avoid rate limits during development, set a Docker Hub mirror:

```bash
# Via flag (applies to the current invocation only)
ocirender --registry-mirror http://my-mirror.internal fetch --image alpine:latest ...

# Via environment variable (applies to all invocations in the session)
export OCIRENDER_REGISTRY_MIRROR=http://my-mirror.internal
```

---

## Dependencies

### Library (`ocirender`)

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime for public API and subprocess management |
| `tar` | Synchronous tar reading and writing |
| `flate2` | gzip decompression |
| `zstd` | zstd decompression |
| `bzip2` | bzip2 decompression |
| `xz2` | xz/lzma decompression |
| `serde` + `serde_json` | JSON parsing for index.json / manifest |
| `sha2` | SHA-256 hashing in the verify subcommand |
| `tempfile` | Temporary squashfuse mountpoint in verify |
| `anyhow` | Error handling throughout |

### CLI (`ocirender-cli`)

| Crate | Purpose |
|---|---|
| `clap` | Argument parsing |
| `reqwest` | HTTPS registry client |
| `indicatif` | Download and packing progress bars |
| `base64` | Encoding/decoding `~/.docker/config.json` auth entries |
| `futures-util` | Byte stream iteration for blob downloads |

## System dependencies

- `mksquashfs` ≥ 4.6 (for `-tar` stdin support); available in `squashfs-tools`
  on most distributions. Required only for squashfs output.
- `squashfuse` (for `verify --squashfs` only)
- `fusermount` or `umount` (for squashfuse unmounting; Linux and macOS/BSD
  respectively)
