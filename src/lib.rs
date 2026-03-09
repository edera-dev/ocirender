pub mod canonical;
pub mod image;
pub mod layers;
pub mod overlay;
pub mod squashfs;
pub mod tracker;
pub mod verify;

use anyhow::Result;
use std::path::Path;

/// Convert an extracted OCI image directory into a squashfs file.
///
/// Layer content is streamed directly into mksquashfs's stdin — the merged
/// tar is never fully materialised in memory.
pub async fn convert_mksquashfs(image_dir: &Path, output_squashfs: &Path, squashfs_binpath: Option<&Path>) -> Result<()> {
    let image_dir = image_dir.to_path_buf();
    let output_squashfs = output_squashfs.to_path_buf();
    let squashfs_binpath = squashfs_binpath.map(Path::to_path_buf);

    tokio::task::spawn_blocking(move || {
        let manifest = image::load_manifest(&image_dir)?;
        let layers = image::resolve_layers(&image_dir, &manifest)?;
        squashfs::write_squashfs(layers, &output_squashfs, squashfs_binpath.as_deref())
    })
    .await?
}
