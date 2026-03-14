use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use ocirender::ImageSpec;

#[derive(Parser)]
#[command(name = "ocirender", about = "OCI container image rendering and verification")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Convert an OCI image directory to a squashfs image.
    ConvertSquashfs {
        /// Path to the extracted OCI image directory.
        #[arg(short, long)]
        image: PathBuf,
        /// Output squashfs file path.
        #[arg(short, long)]
        output: PathBuf,
        /// Path to the mksquashfs binary. If not set, resolved from PATH.
        #[arg(long)]
        mksquashfs: Option<PathBuf>,
    },
    /// Convert an OCI image directory to a tar file.
    ConvertTar {
        /// Path to the extracted OCI image directory.
        #[arg(short, long)]
        image: PathBuf,
        /// Output tar file path.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Convert an OCI image directory directly into a filesystem directory.
    ConvertDir {
        /// Path to the extracted OCI image directory.
        #[arg(short, long)]
        image: PathBuf,
        /// Output directory path.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Verify a generated image against a reference directory.
    ///
    /// Exactly one of --squashfs or --dir must be supplied as the generated
    /// image source. The reference is always a plain directory (e.g. a
    /// containerd-unpacked rootfs or a docker export extracted with tar -x -p).
    Verify {
        /// Path to a generated .squashfs file to verify.
        /// Mounted read-only via squashfuse for the duration of the comparison.
        #[arg(long, conflicts_with = "dir")]
        squashfs: Option<PathBuf>,
        /// Path to a generated directory to verify.
        #[arg(long, conflicts_with = "squashfs")]
        dir: Option<PathBuf>,
        /// Path to the reference directory.
        #[arg(short, long)]
        reference: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::ConvertSquashfs {
            image,
            output,
            mksquashfs,
        } => {
            println!("Converting {} → {}", image.display(), output.display());
            ocirender::convert_mksquashfs(&image, &output, mksquashfs.as_deref()).await?;
            println!("Done: {}", output.display());
        }
        Commands::ConvertTar { image, output } => {
            println!("Converting {} → {}", image.display(), output.display());
            ocirender::convert_tar(&image, &output).await?;
            println!("Done: {}", output.display());
        }
        Commands::ConvertDir { image, output } => {
            println!("Extracting {} → {}", image.display(), output.display());
            ocirender::convert_dir(&image, &output).await?;
            println!("Done: {}", output.display());
        }
        Commands::Verify {
            squashfs,
            dir,
            reference,
        } => {
            let spec = match (squashfs, dir) {
                (Some(p), None) => ImageSpec::Squashfs {
                    path: p,
                    binpath: None,
                },
                (None, Some(p)) => ImageSpec::Dir { path: p },
                (None, None) => bail!("one of --squashfs or --dir is required"),
                (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents this"),
            };

            let report =
                tokio::task::spawn_blocking(move || ocirender::verify::verify(spec, &reference))
                    .await??;

            if report.is_clean() {
                println!("✓ No differences found.");
                return Ok(());
            }

            for p in &report.only_in_generated {
                println!("+ generated only: {}", p.display());
            }
            for p in &report.only_in_reference {
                println!("- reference only: {}", p.display());
            }
            for d in &report.differences {
                println!("~ {}: {}", d.path.display(), d.detail);
            }
            std::process::exit(1);
        }
    }
    Ok(())
}
