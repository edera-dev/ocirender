use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "oci2squashfs", about = "Convert an OCI image to squashfs")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Convert an OCI image directory to a squashfs image.
    Convert {
        /// Path to the extracted OCI image directory.
        #[arg(short, long)]
        image: PathBuf,
        /// Output squashfs file path.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Verify a squashfs image against a reference directory.
    Verify {
        /// Path to the .squashfs file.
        #[arg(short, long)]
        squashfs: PathBuf,
        /// Path to the reference directory (e.g. containerd-unpacked rootfs).
        #[arg(short, long)]
        reference: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Convert { image, output } => {
            println!("Converting {} → {}", image.display(), output.display());
            oci2squashfs::convert(&image, &output).await?;
            println!("Done: {}", output.display());
        }
        Commands::Verify {
            squashfs,
            reference,
        } => {
            let report = tokio::task::spawn_blocking(move || {
                oci2squashfs::verify::verify(&squashfs, &reference)
            })
            .await??;

            if report.only_in_squashfs.is_empty()
                && report.only_in_reference.is_empty()
                && report.differences.is_empty()
            {
                println!("✓ No differences found.");
                return Ok(());
            }

            for p in &report.only_in_squashfs {
                println!("+ squashfs only: {}", p.display());
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
