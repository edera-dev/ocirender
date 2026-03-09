//! Spawn mksquashfs and stream the merged tar directly into its stdin.

use anyhow::{bail, Context, Result};
use std::{
    path::Path,
    process::{Command, Stdio},
};

use crate::{image::LayerBlob, overlay::merge_layers_into};

/// Convert `layers` into a squashfs image at `output` by streaming a merged
/// tar directly into mksquashfs's stdin. No full tar buffer is held in memory.
pub fn write_squashfs(layers: Vec<LayerBlob>, output: &Path) -> Result<()> {
    if output.exists() {
        std::fs::remove_file(output)
            .with_context(|| format!("removing existing {}", output.display()))?;
    }

    let mut child = Command::new("mksquashfs")
        .args([
            "-",
            output.to_str().context("output path is not UTF-8")?,
            "-tar",
            "-noappend",
            "-no-fragments",
            "-comp",
            "zstd",
            "-Xcompression-level",
            "2",
            "-quiet",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning mksquashfs — is it installed?")?;

    let stdin = child.stdin.take().context("child stdin")?;

    // Drive the merge on the current thread, writing directly into the pipe.
    // mksquashfs reads and compresses concurrently in its own process, so
    // the pipe provides natural backpressure without a helper thread.
    let merge_result = merge_layers_into(layers, stdin);

    // Wait for mksquashfs regardless of whether the merge succeeded, so we
    // don't leave a zombie process behind.
    let exit = child.wait_with_output().context("waiting for mksquashfs")?;

    // Surface merge errors before subprocess errors — they're more actionable.
    merge_result.context("merging layers into mksquashfs stdin")?;

    if !exit.status.success() {
        let stderr = String::from_utf8_lossy(&exit.stderr);
        bail!("mksquashfs failed:\n{stderr}");
    }

    Ok(())
}
