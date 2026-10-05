//! The error type returned throughout the library.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use crate::LayerBlob;

/// A boxed error from outside the library, such as a failed layer download
/// reported to the library by its caller.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// `Result` with [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong in this crate.
///
/// Each variant says what failed in terms a caller can act on. As is
/// conventional, an error's `Display` describes only that level; the
/// underlying cause, where there is one, is its
/// [`source`](std::error::Error::source). Error reporters such as `anyhow`'s
/// `{:#}` show the whole chain.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The image layout could not be read: `index.json` or `manifest.json`
    /// is missing or malformed, names a blob that is absent, or uses a digest
    /// algorithm other than SHA-256.
    #[error("invalid image layout at {}: {message}", path.display())]
    ImageLayout {
        /// The file or directory at fault.
        path: PathBuf,
        /// What was wrong with it.
        message: String,
        #[source]
        source: Option<BoxError>,
    },

    /// A layer could not be read or merged: it failed to decompress, is not a
    /// valid tar stream, has an unsupported media type, or holds an entry
    /// that cannot be represented (such as a malformed sparse file).
    #[error("layer {index} ({}): {message}", path.display())]
    Layer {
        /// The layer's position in the image, 0 being the base layer.
        index: usize,
        /// The layer blob.
        path: PathBuf,
        /// What failed.
        message: String,
        #[source]
        source: Option<BoxError>,
    },

    /// The caller reported that it could not deliver a layer, through
    /// [`StreamingPacker::notify_error`](crate::StreamingPacker::notify_error)
    /// or an `Err` in a layer stream. The source is the caller's own error.
    #[error("the layer source failed")]
    LayerSource(#[source] BoxError),

    /// The layers stopped arriving before the image was complete.
    #[error("the layer source ended after {received} of {expected} layers")]
    MissingLayers {
        /// Layers delivered.
        received: usize,
        /// Layers in the image.
        expected: usize,
    },

    /// A layer was delivered with an index the image does not have.
    #[error("layer index {index} is out of range for an image of {count} layers")]
    LayerIndexOutOfRange {
        /// The index delivered.
        index: usize,
        /// Layers in the image.
        count: usize,
    },

    /// The packer is no longer accepting layers, because the conversion has
    /// already stopped. [`StreamingPacker::finish`](crate::StreamingPacker::finish)
    /// returns the reason.
    #[error("the packer has stopped accepting layers")]
    PackerStopped,

    /// `mksquashfs` could not be started.
    #[error("could not run mksquashfs at {}", path.display())]
    MksquashfsSpawn {
        /// The binary that was run.
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `mksquashfs` exited unsuccessfully.
    #[error("mksquashfs failed ({status}): {}", stderr.trim_end())]
    Mksquashfs {
        /// Its exit status.
        status: ExitStatus,
        /// What it wrote to standard error.
        stderr: String,
    },

    /// The output could not be written.
    #[error("writing output {}", path.display())]
    Output {
        /// The output file or directory.
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A verification could not be carried out (as opposed to finding
    /// differences, which it reports in its result).
    #[error("verification failed: {message}")]
    Verify {
        /// What failed.
        message: String,
        #[source]
        source: Option<BoxError>,
    },
}

impl Error {
    pub(crate) fn image_layout(
        path: impl Into<PathBuf>,
        message: impl Into<String>,
        source: impl Into<BoxError>,
    ) -> Self {
        Self::ImageLayout {
            path: path.into(),
            message: message.into(),
            source: Some(source.into()),
        }
    }

    pub(crate) fn image_layout_msg(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::ImageLayout {
            path: path.into(),
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn layer(
        blob: &LayerBlob,
        message: impl Into<String>,
        source: impl Into<BoxError>,
    ) -> Self {
        Self::Layer {
            index: blob.index,
            path: blob.path.clone(),
            message: message.into(),
            source: Some(source.into()),
        }
    }

    pub(crate) fn output(path: &Path, source: std::io::Error) -> Self {
        Self::Output {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn verify(message: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self::Verify {
            message: message.into(),
            source: Some(source.into()),
        }
    }

    pub(crate) fn verify_msg(message: impl Into<String>) -> Self {
        Self::Verify {
            message: message.into(),
            source: None,
        }
    }
}
