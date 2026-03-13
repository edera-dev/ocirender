//! OCI image conversion library.
//!
//! Converts OCI container images into squashfs filesystem images, plain tar
//! archives, or extracted directories by merging the image's layer tarballs
//! directly, without extracting them to intermediate disk storage.
//!
//! # Concepts
//!
//! An OCI image is a stack of compressed tar archives (layers). Converting an
//! image means applying them oldest-first, with newer layers overwriting older
//! ones and whiteout files representing deletions. This library implements that
//! merge as a streaming algorithm: entries flow from the compressed layer blobs
//! through the overlay logic directly into the chosen output sink.
//!
//! The output format and destination are described by [`ImageSpec`], which is
//! also used as the input source for [`verify::verify`]. The same type is used
//! in both directions to avoid a parallel set of read vs. write descriptors.
//!
//! # Conversion
//!
//! For simple one-shot conversions, use [`convert`] or one of its named
//! convenience wrappers ([`convert_mksquashfs`], [`convert_tar`],
//! [`convert_dir`]).
//!
//! When layers are being downloaded concurrently, use [`StreamingPacker`] or
//! one of the `_streaming` convenience wrappers. These accept layers in any
//! arrival order; the merge engine resequences them internally and processes
//! each layer as soon as its turn arrives, keeping the output sink busy while
//! remaining layers are still in flight.
//!
//! # Verification
//!
//! See [`verify::verify`] for comparing a generated image against a reference
//! directory.

pub mod canonical;
pub mod dir;
pub mod image;
pub mod layers;
pub mod overlay;
pub mod squashfs;
pub mod tar;
pub mod tracker;
pub mod verify;

use anyhow::Result;
use image::LayerBlob;
use std::path::{Path, PathBuf};

// ── ImageSpec ─────────────────────────────────────────────────────────────────

/// Format, location, and format-specific configuration of an image.
///
/// Direction-neutral: used both as a conversion output target (by [`convert`]
/// and [`StreamingPacker`]) and as a verification input source (by
/// [`verify::verify`]).
#[derive(Clone, Debug)]
pub enum ImageSpec {
    /// A squashfs filesystem image.
    ///
    /// `binpath` overrides the `mksquashfs` binary location when writing; if
    /// `None`, `mksquashfs` is resolved from `PATH`. Ignored when used as a
    /// verification source.
    Squashfs {
        path: PathBuf,
        binpath: Option<PathBuf>,
    },
    /// A plain tar archive.
    ///
    /// Note: [`verify::verify`] does not support tar sources. Extract to a
    /// directory first with [`convert_dir`], then verify with
    /// [`ImageSpec::Dir`].
    Tar { path: PathBuf },
    /// A directory containing the extracted filesystem tree.
    Dir { path: PathBuf },
}

impl ImageSpec {
    /// The filesystem path this spec refers to, regardless of variant.
    pub fn path(&self) -> &Path {
        match self {
            Self::Squashfs { path, .. } | Self::Tar { path } | Self::Dir { path } => path,
        }
    }
}

// ── PackerProgress ────────────────────────────────────────────────────────────

/// Progress events emitted by the merge engine as layers are processed.
///
/// Delivered via the optional `progress_tx` channel supplied to
/// [`StreamingPacker::new`]. Events are emitted regardless of output format —
/// they describe progress through the overlay merge, not the output sink.
///
/// Note that the caller's channel closes asynchronously after
/// [`StreamingPacker::finish`] returns; drain with `recv().await` rather than
/// `try_recv()` to ensure all events are received.
#[derive(Debug, Clone)]
pub enum PackerProgress {
    /// The merge engine has started processing the layer at this index.
    LayerStarted(usize),
    /// The merge engine has finished processing the layer at this index.
    LayerFinished(usize),
}

// ── LayerMeta ─────────────────────────────────────────────────────────────────

/// Manifest-derived metadata for a single layer, captured before downloading
/// begins.
///
/// Passed to [`StreamingPacker::new`] so the packer can reconstruct a
/// [`LayerBlob`] from each blob file as it arrives on disk via
/// [`StreamingPacker::notify_layer_ready`].
#[derive(Clone, Debug)]
pub struct LayerMeta {
    /// Zero-based position of this layer in the manifest's layer list.
    /// Layer 0 is the oldest (base) layer; the highest index is the newest.
    pub index: usize,
    /// OCI media type of the layer blob, used to select the correct
    /// decompressor. For example,
    /// `application/vnd.oci.image.layer.v1.tar+gzip`.
    pub media_type: String,
}

// ── internal helpers ──────────────────────────────────────────────────────────

/// Load the manifest from `image_dir` and pre-load all resolved layer blobs
/// into a std channel, so the batch conversion path can reuse the streaming
/// merge implementation without any code duplication.
fn layers_from_image_dir(
    image_dir: &Path,
) -> Result<(std::sync::mpsc::Receiver<Result<LayerBlob>>, usize)> {
    let manifest = image::load_manifest(image_dir)?;
    let layers = image::resolve_layers(image_dir, &manifest)?;
    let total = layers.len();
    let (tx, rx) = std::sync::mpsc::channel();
    for layer in layers {
        tx.send(Ok(layer)).unwrap();
    }
    Ok((rx, total))
}

/// Create a tokio→std channel bridge for layer delivery.
///
/// Returns a tokio sender for async callers to push [`LayerBlob`]s into, and a
/// std receiver to hand off to a `spawn_blocking` merge thread. Dropping the
/// tokio sender causes the relay task to exit, which drops the std sender and
/// signals EOF to the merge thread's `recv` loop.
fn make_layer_channel(
    cap: usize,
) -> (
    tokio::sync::mpsc::Sender<Result<LayerBlob>>,
    std::sync::mpsc::Receiver<Result<LayerBlob>>,
) {
    let (tokio_tx, tokio_rx) = tokio::sync::mpsc::channel(cap.max(1));
    let (std_tx, std_rx) = std::sync::mpsc::channel();
    tokio::spawn(relay_to_blocking(tokio_rx, std_tx));
    (tokio_tx, std_rx)
}

/// Relay items from a tokio mpsc receiver to a std mpsc sender.
///
/// Runs as a detached task. When the tokio sender is dropped the receiver
/// returns `None`, the task exits, and the std sender is dropped — signalling
/// EOF to the blocking merge thread.
async fn relay_to_blocking(
    mut tokio_rx: tokio::sync::mpsc::Receiver<Result<LayerBlob>>,
    std_tx: std::sync::mpsc::Sender<Result<LayerBlob>>,
) {
    while let Some(item) = tokio_rx.recv().await {
        if std_tx.send(item).is_err() {
            break;
        }
    }
}

/// Relay progress events from a std mpsc receiver back into the async world.
///
/// Runs a `spawn_blocking` call internally so the async task is never stalled
/// on the std `recv`. Events are buffered through an intermediate tokio channel
/// of capacity 1; the blocking thread and the async relay step proceed
/// independently with minimal coupling.
async fn relay_from_blocking(
    std_rx: std::sync::mpsc::Receiver<PackerProgress>,
    tokio_tx: tokio::sync::mpsc::Sender<PackerProgress>,
) {
    let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::channel(1);
    tokio::task::spawn_blocking(move || {
        while let Ok(item) = std_rx.recv() {
            if bridge_tx.blocking_send(item).is_err() {
                break;
            }
        }
    });
    while let Some(item) = bridge_rx.recv().await {
        if tokio_tx.send(item).await.is_err() {
            break;
        }
    }
}

/// Single dispatch point from an [`ImageSpec`] to the appropriate blocking
/// write function. All conversion paths — batch and streaming, sync and async
/// — converge here.
fn write_for_spec(
    receiver: std::sync::mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    spec: ImageSpec,
    progress_tx: Option<std::sync::mpsc::SyncSender<PackerProgress>>,
) -> Result<()> {
    match spec {
        ImageSpec::Squashfs { path, binpath } => squashfs::write_squashfs_with_progress(
            receiver,
            total_layers,
            &path,
            binpath.as_deref(),
            progress_tx,
        ),
        ImageSpec::Tar { path } => {
            tar::write_tar_with_progress(receiver, total_layers, &path, progress_tx)
        }
        ImageSpec::Dir { path } => {
            dir::write_dir_with_progress(receiver, total_layers, &path, progress_tx)
        }
    }
}

// ── Unified convert entry point ───────────────────────────────────────────────

/// Convert an OCI image directory into the format and location described by
/// `spec`.
///
/// All layers are resolved from disk before conversion begins. For concurrent
/// download-and-convert workflows, use [`StreamingPacker`] instead.
pub async fn convert(image_dir: &Path, spec: ImageSpec) -> Result<()> {
    let image_dir = image_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let (rx, total) = layers_from_image_dir(&image_dir)?;
        write_for_spec(rx, total, spec, None)
    })
    .await?
}

// ── Batch compatibility wrappers ──────────────────────────────────────────────

/// Convert an OCI image directory into a squashfs file.
///
/// Convenience wrapper around [`convert`] with [`ImageSpec::Squashfs`].
pub async fn convert_mksquashfs(
    image_dir: &Path,
    output_squashfs: &Path,
    squashfs_binpath: Option<&Path>,
) -> Result<()> {
    convert(
        image_dir,
        ImageSpec::Squashfs {
            path: output_squashfs.to_path_buf(),
            binpath: squashfs_binpath.map(Path::to_path_buf),
        },
    )
    .await
}

/// Convert an OCI image directory into a plain tar file.
///
/// Convenience wrapper around [`convert`] with [`ImageSpec::Tar`].
pub async fn convert_tar(image_dir: &Path, output_tar: &Path) -> Result<()> {
    convert(
        image_dir,
        ImageSpec::Tar {
            path: output_tar.to_path_buf(),
        },
    )
    .await
}

/// Extract an OCI image directory directly into `output_dir`.
///
/// Convenience wrapper around [`convert`] with [`ImageSpec::Dir`].
pub async fn convert_dir(image_dir: &Path, output_dir: &Path) -> Result<()> {
    convert(
        image_dir,
        ImageSpec::Dir {
            path: output_dir.to_path_buf(),
        },
    )
    .await
}

// ── Streaming compatibility wrappers ─────────────────────────────────────────

/// Streaming variant of [`convert_mksquashfs`].
///
/// Layers are delivered via `receiver` as downloads complete, in any order.
/// A download error sent as `Err` aborts the merge and cleans up the partial
/// output file.
pub async fn convert_mksquashfs_streaming(
    receiver: tokio::sync::mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    output_squashfs: &Path,
    squashfs_binpath: Option<&Path>,
) -> Result<()> {
    let spec = ImageSpec::Squashfs {
        path: output_squashfs.to_path_buf(),
        binpath: squashfs_binpath.map(Path::to_path_buf),
    };
    let (std_tx, std_rx) = std::sync::mpsc::channel();
    tokio::spawn(relay_to_blocking(receiver, std_tx));
    tokio::task::spawn_blocking(move || write_for_spec(std_rx, total_layers, spec, None)).await?
}

/// Streaming variant of [`convert_tar`].
///
/// On error the partially written output file is removed.
pub async fn convert_tar_streaming(
    receiver: tokio::sync::mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    output_tar: &Path,
) -> Result<()> {
    let spec = ImageSpec::Tar {
        path: output_tar.to_path_buf(),
    };
    let (std_tx, std_rx) = std::sync::mpsc::channel();
    tokio::spawn(relay_to_blocking(receiver, std_tx));
    tokio::task::spawn_blocking(move || write_for_spec(std_rx, total_layers, spec, None)).await?
}

/// Streaming variant of [`convert_dir`].
///
/// On error the partially populated output directory is left in place —
/// callers are responsible for cleanup.
pub async fn convert_dir_streaming(
    receiver: tokio::sync::mpsc::Receiver<Result<LayerBlob>>,
    total_layers: usize,
    output_dir: &Path,
) -> Result<()> {
    let spec = ImageSpec::Dir {
        path: output_dir.to_path_buf(),
    };
    let (std_tx, std_rx) = std::sync::mpsc::channel();
    tokio::spawn(relay_to_blocking(receiver, std_tx));
    tokio::task::spawn_blocking(move || write_for_spec(std_rx, total_layers, spec, None)).await?
}

// ── StreamingPacker ───────────────────────────────────────────────────────────

/// A streaming image packer that accepts layers in any order as downloads
/// complete.
///
/// Layers are resequenced internally and processed newest-first as each one
/// arrives, keeping the output sink busy without waiting for all downloads to
/// finish. Supports all output formats via [`ImageSpec`].
///
/// # Download ordering
///
/// The merge engine processes layers in strict descending index order: it
/// cannot begin processing layer N until layer N+1 is complete. A layer that
/// arrives early has its path held in a waiting queue until its turn comes.
///
/// To minimise buffering and keep the output sink as busy as possible,
/// initiate downloads in descending index order (highest index — i.e. the
/// newest layer — first) and keep the number of concurrent downloads small.
/// Fetching all layers in parallel may cause lower-indexed layers to finish
/// and sit in the queue while the engine is blocked waiting for a
/// higher-indexed one that is still in flight.
///
/// # Usage
///
/// ```no_run
/// # use oci2squashfs::{StreamingPacker, LayerMeta, ImageSpec};
/// # use std::path::PathBuf;
/// # async fn example() -> anyhow::Result<()> {
/// let metas: Vec<LayerMeta> = /* from manifest */ # vec![];
/// let packer = StreamingPacker::new(
///     metas,
///     ImageSpec::Squashfs { path: "out.squashfs".into(), binpath: None },
///     None,
/// );
///
/// // Call from any task as blobs finish downloading, in any order.
/// packer.notify_layer_ready(0, PathBuf::from("/tmp/layer0.tar.gz")).await?;
/// packer.notify_layer_ready(2, PathBuf::from("/tmp/layer2.tar.gz")).await?;
/// packer.notify_layer_ready(1, PathBuf::from("/tmp/layer1.tar.gz")).await?;
///
/// packer.finish().await?;
/// # Ok(())
/// # }
/// ```
pub struct StreamingPacker {
    /// Tokio sender end of the layer delivery channel. Dropped in `finish()`
    /// to signal EOF to the relay task and, transitively, the merge thread.
    layer_tx: tokio::sync::mpsc::Sender<Result<LayerBlob>>,
    /// Per-layer metadata indexed by manifest position, used to reconstruct
    /// a [`LayerBlob`] from a bare file path in `notify_layer_ready`.
    metas: Vec<LayerMeta>,
    /// Handle to the `spawn_blocking` task running the merge and output sink.
    task: tokio::task::JoinHandle<Result<()>>,
}

impl StreamingPacker {
    /// Construct a `StreamingPacker` and immediately begin processing.
    ///
    /// The output sink is opened and any required subprocess (e.g.
    /// `mksquashfs` for [`ImageSpec::Squashfs`]) is spawned at construction
    /// time. `layer_metas` must contain one entry per layer in manifest order.
    ///
    /// `progress_tx`, if supplied, receives [`PackerProgress`] events from the
    /// merge engine as each layer is processed. The channel closes
    /// asynchronously after [`finish`] returns; drain with `recv().await` to
    /// ensure all events are received before inspecting them.
    ///
    /// [`finish`]: StreamingPacker::finish
    pub fn new(
        layer_metas: Vec<LayerMeta>,
        spec: ImageSpec,
        progress_tx: Option<tokio::sync::mpsc::Sender<PackerProgress>>,
    ) -> Self {
        let total = layer_metas.len();
        let (tokio_tx, std_rx) = make_layer_channel(total);

        let std_progress_tx = if let Some(async_tx) = progress_tx {
            let (tx, rx) = std::sync::mpsc::sync_channel::<PackerProgress>(total.max(1) * 2);
            tokio::spawn(relay_from_blocking(rx, async_tx));
            Some(tx)
        } else {
            None
        };

        let task = tokio::task::spawn_blocking(move || {
            write_for_spec(std_rx, total, spec, std_progress_tx)
        });

        StreamingPacker {
            layer_tx: tokio_tx,
            metas: layer_metas,
            task,
        }
    }

    /// Notify the packer that the layer blob at `index` has finished
    /// downloading and is available at `path`.
    ///
    /// May be called from any task in any order. Returns an error only if the
    /// internal channel has already closed, which means the merge thread has
    /// hit a fatal error. In that case callers should stop sending and
    /// propagate the error from [`finish`].
    ///
    /// [`finish`]: StreamingPacker::finish
    pub async fn notify_layer_ready(&self, index: usize, path: PathBuf) -> Result<()> {
        let meta = self.metas.get(index).ok_or_else(|| {
            anyhow::anyhow!(
                "layer index {index} out of range (have {} layers)",
                self.metas.len()
            )
        })?;

        self.layer_tx
            .send(Ok(LayerBlob {
                path,
                media_type: meta.media_type.clone(),
                index,
            }))
            .await
            .map_err(|_| anyhow::anyhow!("packer channel closed unexpectedly"))
    }

    /// Signal a download failure to the packer, causing the merge to abort.
    ///
    /// After calling this, [`finish`] will return an error. Best-effort: if
    /// the merge has already failed and the channel is closed, this is a
    /// no-op.
    ///
    /// [`finish`]: StreamingPacker::finish
    pub async fn notify_error(&self, err: anyhow::Error) {
        let _ = self.layer_tx.send(Err(err)).await;
    }

    /// Wait for all output to be finalised and return the result.
    ///
    /// Must be called after all [`notify_layer_ready`] and [`notify_error`]
    /// calls. Dropping the packer without calling `finish` will leave the
    /// merge task running until the internal channel closes naturally.
    ///
    /// [`notify_layer_ready`]: StreamingPacker::notify_layer_ready
    /// [`notify_error`]: StreamingPacker::notify_error
    pub async fn finish(self) -> Result<()> {
        // Dropping the sender causes relay_to_blocking to see EOF on the tokio
        // receiver, which exits the relay task and drops the std sender,
        // unblocking the merge thread's recv loop.
        drop(self.layer_tx);
        self.task.await?
    }
}
