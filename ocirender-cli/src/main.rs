use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use ocirender::ImageSpec;

mod registry;
use registry::{client::RegistryClient, credentials::CredentialStore, reference::ImageReference};

#[derive(Parser)]
#[command(
    name = "ocirender",
    about = "OCI container image rendering and verification"
)]
struct Cli {
    /// Registry mirror base URL for Docker Hub requests.
    ///
    /// When set, all requests to registry-1.docker.io are redirected to this
    /// base URL instead.  Useful for avoiding rate limits during development
    /// and testing.  The mirror must speak the OCI Distribution Specification
    /// API.
    ///
    /// If not supplied, the OCIRENDER_REGISTRY_MIRROR environment variable is
    /// consulted as a fallback.
    #[arg(long, global = true)]
    registry_mirror: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Download an OCI image from a registry to a local OCI image layout
    /// directory.
    ///
    /// The output directory is in the standard OCI image layout format and can
    /// be passed directly to the convert-squashfs, convert-tar, or convert-dir
    /// subcommands.
    Fetch {
        /// Image reference (e.g. nginx, ghcr.io/myorg/myimage:tag,
        /// docker.io/library/ubuntu:22.04).
        #[arg(short, long)]
        image: String,
        /// Output OCI image layout directory.
        #[arg(short, long)]
        output: PathBuf,
        /// Target platform in os/arch format. Defaults to linux/amd64.
        #[arg(long, default_value = "linux/amd64")]
        platform: String,
        /// Maximum number of concurrent layer downloads.
        #[arg(long, default_value = "3")]
        concurrency: usize,
        /// Fetch only the manifest; do not download layer blobs.
        ///
        /// The output directory will contain the oci-layout marker, index.json,
        /// and the manifest blob, but no layer blobs. Useful for validating
        /// manifest format and media type handling against real registries.
        #[arg(long)]
        manifest_only: bool,
    },

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

    /// Convert an OCI image directory to a erofs image.
    ConvertErofs {
        /// Path to the extracted OCI image directory.
        #[arg(short, long)]
        image: PathBuf,
        /// Output erofs file path.
        #[arg(short, long)]
        output: PathBuf,
        /// Path to the mkfs.erofs binary. If None, will attempt to resolve from PATH
        #[arg(long)]
        mkfs_erofs: Option<PathBuf>,
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

    /// Pull an OCI image from a registry and convert it directly to the target
    /// format, without writing an intermediate OCI layout directory.
    ///
    /// Layer downloads and image assembly are pipelined: the merge engine
    /// begins processing each layer as soon as it arrives while remaining
    /// layers are still downloading.
    ///
    /// For best throughput, layers are downloaded in descending index order
    /// (newest first) so the merge engine is never blocked waiting for a layer
    /// it needs urgently.
    Pull {
        /// Image reference (e.g. nginx, ghcr.io/myorg/myimage:tag).
        #[arg(short, long)]
        image: String,
        /// Output squashfs file path.
        #[arg(long, group = "output_dest")]
        output_squashfs: Option<PathBuf>,
        /// Output tar file path.
        #[arg(long, group = "output_dest")]
        output_tar: Option<PathBuf>,
        /// Output directory path.
        #[arg(long, group = "output_dest")]
        output_dir: Option<PathBuf>,
        /// Path to the mksquashfs binary (squashfs output only).
        #[arg(long)]
        mksquashfs: Option<PathBuf>,
        /// Target platform in os/arch format. Defaults to linux/amd64.
        #[arg(long, default_value = "linux/amd64")]
        platform: String,
        /// Maximum number of concurrent layer downloads.
        #[arg(long, default_value = "3")]
        concurrency: usize,
    },

    /// Store credentials for a registry in ~/.docker/config.json.
    ///
    /// Credentials are shared with Docker, crane, skopeo, and any other tool
    /// that reads the standard Docker credential file.
    ///
    /// For ghcr.io:
    ///   Username: your GitHub username
    ///   Password: a PAT with read:packages scope, or `gh auth token` output
    ///
    ///   Example:
    ///     gh auth token | ocirender login ghcr.io -u YOUR_USERNAME --password-stdin
    Login {
        /// Registry hostname (e.g. ghcr.io, myregistry.example.com:5000).
        /// For Docker Hub use: registry-1.docker.io
        registry: String,
        /// Username.
        #[arg(short, long)]
        username: String,
        /// Password or token.
        ///
        /// Prefer --password-stdin over this flag to avoid the credential
        /// appearing in shell history.
        #[arg(short, long, conflicts_with = "password_stdin")]
        password: Option<String>,
        /// Read the password from stdin (one line).
        ///
        /// Convenient for piping: gh auth token | ocirender login ghcr.io -u USER --password-stdin
        #[arg(long)]
        password_stdin: bool,
    },
    Verify {
        /// Path to a generated .squashfs file to verify.
        /// Mounted read-only via squashfuse for the duration of the comparison.
        #[arg(long, conflicts_with_all = ["dir", "erofs"])]
        squashfs: Option<PathBuf>,
        /// Path to a generated .erofs file to verify.
        /// Mounted read-only via erofsfuse for the duration of the comparison.
        #[arg(long, conflicts_with_all = ["dir", "squashfs"])]
        erofs: Option<PathBuf>,
        /// Path to a generated directory to verify.
        #[arg(long, conflicts_with_all = ["erofs", "squashfs"])]
        dir: Option<PathBuf>,
        /// Path to the reference directory.
        #[arg(short, long)]
        reference: PathBuf,
        /// Do not report uid/gid differences.
        ///
        /// Useful when comparing a squashfs image (which records ownership
        /// from tar headers) against a directory extracted without root
        /// privileges (where chown silently fails and all files are owned
        /// by the invoking user).
        #[arg(long)]
        ignore_ownership: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve registry mirror and load stored credentials once, before the
    // subcommand dispatch.  Both are used by Fetch and Pull; Login uses
    // CredentialStore::save directly and doesn't need a client.
    let mirror = cli
        .registry_mirror
        .or_else(|| std::env::var(registry::client::MIRROR_ENV_VAR).ok());
    let credentials = CredentialStore::load().unwrap_or_default();

    match cli.command {
        Commands::Fetch {
            image,
            output,
            platform,
            concurrency,
            manifest_only,
        } => {
            let image_ref = ImageReference::parse(&image)?;
            let (os, arch) = parse_platform(&platform)?;
            let client = RegistryClient::new(mirror.as_deref(), credentials)?;
            client
                .fetch_image(&image_ref, &os, &arch, &output, manifest_only, concurrency)
                .await?;
            if manifest_only {
                println!("Manifest fetched: {}", output.display());
            } else {
                println!("Image fetched: {}", output.display());
            }
        }

        Commands::ConvertSquashfs {
            image,
            output,
            mksquashfs,
        } => {
            println!("Converting {} → {}", image.display(), output.display());
            ocirender::convert_mksquashfs(&image, &output, mksquashfs.as_deref()).await?;
            println!("Done: {}", output.display());
        }

        Commands::ConvertErofs {
            image,
            output,
            mkfs_erofs,
        } => {
            println!("Converting {} → {}", image.display(), output.display());
            ocirender::convert_erofs(&image, &output, mkfs_erofs.as_deref()).await?;
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

        Commands::Pull {
            image,
            output_squashfs,
            output_tar,
            output_dir,
            mksquashfs,
            platform,
            concurrency,
        } => {
            let image_ref = ImageReference::parse(&image)?;
            let (os, arch) = parse_platform(&platform)?;

            let spec = match (output_squashfs, output_tar, output_dir) {
                (Some(p), None, None) => ImageSpec::Squashfs {
                    path: p,
                    binpath: mksquashfs,
                },
                (None, Some(p), None) => ImageSpec::Tar { path: p },
                (None, None, Some(p)) => ImageSpec::Dir { path: p },
                (None, None, None) => {
                    bail!("one of --output-squashfs, --output-tar, or --output-dir is required")
                }
                _ => unreachable!("clap group prevents multiple output formats"),
            };

            let output_display = spec.path().to_path_buf();
            let client = RegistryClient::new(mirror.as_deref(), credentials)?;
            client
                .pull_image(&image_ref, &os, &arch, spec, concurrency)
                .await?;
            println!("Done: {}", output_display.display());
        }

        Commands::Login {
            registry,
            username,
            password,
            password_stdin,
        } => {
            let password = if password_stdin {
                let mut s = String::new();
                std::io::stdin()
                    .read_line(&mut s)
                    .context("reading password from stdin")?;
                s.trim_end_matches(['\n', '\r']).to_string()
            } else {
                password.context("one of --password or --password-stdin is required")?
            };

            CredentialStore::save(&registry, &username, &password)
                .with_context(|| format!("saving credentials for {registry}"))?;
            println!("Credentials saved for {registry}.");
        }

        Commands::Verify {
            squashfs,
            erofs,
            dir,
            reference,
            ignore_ownership,
        } => {
            let spec = match (squashfs, erofs, dir) {
                (Some(p), None, None) => ImageSpec::Squashfs {
                    path: p,
                    binpath: None,
                },
                (None, Some(p), None) => ImageSpec::Erofs {
                    path: p,
                    binpath: None,
                },
                (None, None, Some(p)) => ImageSpec::Dir { path: p },
                (None, None, None) => bail!("one of --squashfs or --dir is required"),
                _ => unreachable!("clap conflicts_with prevents this"),
            };

            let report = tokio::task::spawn_blocking(move || {
                ocirender::verify::verify(spec, &reference, ignore_ownership)
            })
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

/// Parse a `"os/arch"` platform string.
fn parse_platform(s: &str) -> Result<(String, String)> {
    let mut it = s.splitn(2, '/');
    let os = it.next().unwrap().to_string();
    let arch = it.next().ok_or_else(|| {
        anyhow::anyhow!("--platform must be in os/arch format (e.g. linux/amd64); got {s:?}")
    })?;
    Ok((os, arch.to_string()))
}
