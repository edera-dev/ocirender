//! OCI Distribution Specification registry HTTP client.
//!
//! [`RegistryClient`] wraps `reqwest` and handles the Bearer auth flow,
//! token caching, manifest resolution, and blob streaming.  The two
//! high-level operations exposed by the CLI are [`fetch_image`] and
//! [`pull_image`].
//!
//! Both operations use the same worker-queue architecture: N async tasks
//! drain a shared `VecDeque` ordered **highest-index-first**, so the merge
//! engine always receives the layer it needs next as early as possible.
//!
//! The client is `Arc`-backed and cheaply cloneable so worker tasks can
//! share it without lifetime friction.
//!
//! [`fetch_image`]: RegistryClient::fetch_image
//! [`pull_image`]: RegistryClient::pull_image

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::header::ACCEPT;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::Arc,
};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    sync::Mutex,
    task::JoinSet,
};

use ocirender::{ImageSpec, LayerMeta, PackerProgress, StreamingPacker};

use super::{auth::BearerChallenge, credentials::CredentialStore, reference::ImageReference};

/// Environment variable consulted when `--registry-mirror` is absent.
pub const MIRROR_ENV_VAR: &str = "OCIRENDER_REGISTRY_MIRROR";

// ── Media type constants ──────────────────────────────────────────────────────

const MT_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MT_DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

/// Accept header sent with all manifest requests, listed in preference order.
const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.docker.distribution.manifest.v2+json,",
    "application/json"
);

// ── Serde types ───────────────────────────────────────────────────────────────

/// Top-level structure for either a manifest index or a single-image manifest.
/// Both formats share the same field names we care about.
#[derive(Deserialize)]
struct RawEnvelope {
    /// Non-empty in manifest index types (OCI index, Docker manifest list).
    #[serde(default)]
    manifests: Vec<RawDescriptor>,
    /// Image config blob descriptor.  Present in single-image manifests;
    /// absent in index types.  Downloaded by `fetch_image` so that tools like
    /// umoci (which build a full OCI runtime bundle) can find it.
    #[serde(default)]
    config: Option<RawDescriptor>,
    #[serde(default)]
    layers: Vec<RawDescriptor>,
}

#[derive(Deserialize, Clone)]
struct RawDescriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    platform: Option<RawPlatform>,
}

#[derive(Deserialize, Clone)]
struct RawPlatform {
    os: String,
    architecture: String,
}

/// Docker Hub returns both `token` and `access_token` with identical values.
/// Other registries return only `access_token`.  We deserialize both as
/// optional and coalesce rather than using `#[serde(alias)]`, which errors
/// when both fields are present simultaneously.
#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

// ── RegistryClient ────────────────────────────────────────────────────────────

/// An HTTP client for the OCI Distribution Specification registry API.
///
/// Cheaply cloneable — both the HTTP connection pool and the token cache are
/// shared via `Arc`.
#[derive(Clone)]
pub struct RegistryClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    /// Bearer tokens, keyed by `"{registry}\n{scope}"`.
    tokens: Mutex<HashMap<String, String>>,
    /// Optional mirror base URL for Docker Hub (`registry-1.docker.io`)
    /// requests. See [`RegistryClient::base_url`].
    mirror: Option<String>,
    /// Registry credentials loaded from `~/.docker/config.json`.
    credentials: CredentialStore,
}

impl RegistryClient {
    /// Create a new client.
    ///
    /// `mirror`, if supplied, is the base URL substituted for
    /// `https://registry-1.docker.io` on Docker Hub requests.
    ///
    /// `credentials` is loaded from `~/.docker/config.json` by the caller
    /// (via [`CredentialStore::load`]) and consulted when fetching Bearer
    /// tokens for private registries.
    pub fn new(mirror: Option<&str>, credentials: CredentialStore) -> Result<Self> {
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                tokens: Mutex::new(HashMap::new()),
                mirror: mirror.map(|s| s.trim_end_matches('/').to_string()),
                credentials,
            }),
        })
    }

    /// Return the base URL for `registry`.
    ///
    /// Substitutes the configured mirror for `registry-1.docker.io` when
    /// one is set; uses `https://<registry>` for everything else.
    fn base_url(&self, registry: &str) -> String {
        if registry == "registry-1.docker.io"
            && let Some(mirror) = &self.inner.mirror
        {
            return mirror.clone();
        }
        format!("https://{registry}")
    }

    // ── Token management ──────────────────────────────────────────────────────

    async fn resolve_token(
        &self,
        cache_key: &str,
        challenge: &BearerChallenge,
        scope: &str,
        registry: &str,
    ) -> Result<String> {
        {
            let cache = self.inner.tokens.lock().await;
            if let Some(t) = cache.get(cache_key) {
                return Ok(t.clone());
            }
        }

        let url = challenge.token_url(scope);

        // Attach Basic auth when we have stored credentials for this registry.
        // The token endpoint uses them to issue a token with the appropriate
        // pull (or push) permissions for private repositories.
        let mut req = self.inner.http.get(&url);
        if let Some(creds) = self.inner.credentials.lookup(registry) {
            req = req.basic_auth(&creds.username, Some(&creds.password));
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("fetching token from {url}"))?;

        if !resp.status().is_success() {
            bail!("token endpoint returned {} for {url}", resp.status());
        }

        let body: TokenResponse = resp.json().await.context("parsing token response")?;
        let token = body
            .token
            .or(body.access_token)
            .context("token response contained neither 'token' nor 'access_token'")?;

        self.inner
            .tokens
            .lock()
            .await
            .insert(cache_key.to_string(), token.clone());
        Ok(token)
    }

    // ── Authenticated GET ─────────────────────────────────────────────────────

    /// Perform a GET with automatic Bearer token retry on 401.
    ///
    /// On a 401 response the `WWW-Authenticate` challenge is parsed, a token
    /// is fetched (or retrieved from cache), and the request is retried.
    async fn get_authed(
        &self,
        url: &str,
        accept: &str,
        registry: &str,
        scope: &str,
    ) -> Result<reqwest::Response> {
        // Attempt unauthenticated first — public images don't need a token.
        let resp = self
            .inner
            .http
            .get(url)
            .header(ACCEPT, accept)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return check_status(resp, url).await;
        }

        // Parse the Bearer challenge from the 401 response.
        let www = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let challenge = BearerChallenge::parse(www)
            .with_context(|| format!("no Bearer challenge in 401 from {url}"))?;

        let cache_key = format!("{registry}\n{scope}");
        let token = self
            .resolve_token(&cache_key, &challenge, scope, registry)
            .await?;

        let resp = self
            .inner
            .http
            .get(url)
            .header(ACCEPT, accept)
            .bearer_auth(&token)
            .send()
            .await
            .with_context(|| format!("GET {url} (authenticated)"))?;

        check_status(resp, url).await
    }

    // ── Manifest operations ───────────────────────────────────────────────────

    /// Fetch a manifest or index blob, returning `(content_type, raw_bytes)`.
    ///
    /// The raw bytes are exactly what the registry sent; the content-type
    /// tells the caller whether it received an index or a single-image
    /// manifest.
    async fn fetch_manifest_raw(&self, image_ref: &ImageReference) -> Result<(String, Vec<u8>)> {
        let url = format!(
            "{}/v2/{}/manifests/{}",
            self.base_url(&image_ref.registry),
            image_ref.repository,
            image_ref.reference_str()
        );
        let resp = self
            .get_authed(
                &url,
                MANIFEST_ACCEPT,
                &image_ref.registry,
                &image_ref.pull_scope(),
            )
            .await?;

        // Prefer the response Content-Type over the Accept we sent; strip
        // parameters (e.g. "; charset=utf-8").
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

        let body = resp.bytes().await.context("reading manifest body")?;
        Ok((ct, body.to_vec()))
    }

    /// Resolve `image_ref` to a single-platform manifest.
    ///
    /// If the registry returns an index, the entry for `(os, arch)` is
    /// selected and its manifest is fetched.  Returns
    /// `(content_type, raw_bytes, "sha256:<hex>")`.
    pub async fn resolve_manifest(
        &self,
        image_ref: &ImageReference,
        os: &str,
        arch: &str,
    ) -> Result<(String, Vec<u8>, String)> {
        let (ct, raw) = self.fetch_manifest_raw(image_ref).await?;

        if is_index_type(&ct) {
            // Parse the index and resolve to the requested platform.
            let envelope: RawEnvelope =
                serde_json::from_slice(&raw).context("parsing manifest index")?;

            let entry = select_platform(&envelope.manifests, os, arch)
                .with_context(|| format!("no {os}/{arch} manifest in index for {image_ref}"))?;

            let (pct, pbytes) = self
                .fetch_manifest_raw(&image_ref.with_digest(&entry.digest))
                .await?;

            let digest = format!("sha256:{}", sha256_hex(&pbytes));
            Ok((pct, pbytes, digest))
        } else {
            let digest = format!("sha256:{}", sha256_hex(&raw));
            Ok((ct, raw, digest))
        }
    }

    // ── Blob downloading ──────────────────────────────────────────────────────

    /// Stream a blob from the registry to `dest`, verifying its SHA-256.
    ///
    /// The receive loop runs with no hashing in the hot path — SHA-256 is
    /// computed in a `spawn_blocking` task after the file is fully written,
    /// so the network stream is never gated on CPU work.
    ///
    /// If `pb` is supplied it is incremented with batched byte counts as data
    /// arrives.  The caller is responsible for setting the bar's length and
    /// style before calling, and calling `finish`/`finish_with_message`
    /// afterwards.
    ///
    /// On digest mismatch `dest` is removed before the error is returned.
    pub async fn download_blob(
        &self,
        image_ref: &ImageReference,
        digest: &str,
        dest: &Path,
        pb: Option<&ProgressBar>,
    ) -> Result<()> {
        let url = format!(
            "{}/v2/{}/blobs/{}",
            self.base_url(&image_ref.registry),
            image_ref.repository,
            digest
        );
        let resp = self
            .get_authed(
                &url,
                "application/octet-stream",
                &image_ref.registry,
                &image_ref.pull_scope(),
            )
            .await?;

        let expected = digest
            .strip_prefix("sha256:")
            .with_context(|| format!("blob digest must start with sha256:, got {digest}"))?
            .to_string();

        // 8 MiB write buffer: most chunk writes become a pure memcpy, keeping
        // the receive loop decoupled from the spawn_blocking round-trips that
        // tokio::fs::File incurs on every actual write syscall.
        const WRITE_BUF: usize = 8 * 1024 * 1024;
        let file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("creating {}", dest.display()))?;
        let mut file = BufWriter::with_capacity(WRITE_BUF, file);

        // Batch progress bar updates: accumulate bytes locally and flush to
        // the bar at most once per interval to avoid atomic write pressure on
        // fast links.
        let mut pending_bytes: u64 = 0;
        let mut last_pb_update = std::time::Instant::now();
        const PB_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("error reading blob stream")?;
            file.write_all(&chunk)
                .await
                .with_context(|| format!("writing to {}", dest.display()))?;
            if let Some(pb) = pb {
                pending_bytes += chunk.len() as u64;
                let now = std::time::Instant::now();
                if now.duration_since(last_pb_update) >= PB_UPDATE_INTERVAL {
                    pb.inc(pending_bytes);
                    pending_bytes = 0;
                    last_pb_update = now;
                }
            }
        }
        file.flush().await?;
        if let Some(pb) = pb
            && pending_bytes > 0
        {
            pb.inc(pending_bytes);
        }

        // Verify digest off the receive loop in a blocking thread.  Reading
        // the completed file sequentially is fast (page cache) and keeps CPU
        // work off the async executor.
        let dest_buf = dest.to_path_buf();
        let actual = tokio::task::spawn_blocking(move || hash_file_sync(&dest_buf))
            .await
            .context("digest verification task panicked")??;
        if actual != expected {
            tokio::fs::remove_file(dest).await.ok();
            bail!("digest mismatch for {digest}: computed sha256:{actual}");
        }
        Ok(())
    }

    // ── `fetch` subcommand ────────────────────────────────────────────────────

    /// Fetch an OCI image from the registry and write an OCI image layout
    /// directory to `output_dir`.
    ///
    /// Up to `concurrency` layer blobs are downloaded in parallel.  Workers
    /// pull from a shared queue ordered **highest-index-first** so that the
    /// layers most likely to be needed first by a subsequent `convert` are
    /// prioritised.
    ///
    /// **Resumability**: if a blob file already exists at the expected path
    /// its SHA-256 is verified before the download is skipped.  A corrupt or
    /// incomplete file is removed and re-downloaded.
    ///
    /// If `manifest_only` is `true`, layer blobs are not downloaded.
    pub async fn fetch_image(
        &self,
        image_ref: &ImageReference,
        os: &str,
        arch: &str,
        output_dir: &Path,
        manifest_only: bool,
        concurrency: usize,
    ) -> Result<()> {
        let blobs_dir = output_dir.join("blobs").join("sha256");
        tokio::fs::create_dir_all(&blobs_dir)
            .await
            .with_context(|| format!("creating {}", blobs_dir.display()))?;

        // OCI image layout version marker (required by the spec).
        tokio::fs::write(
            output_dir.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .await
        .context("writing oci-layout")?;

        // Fetch and resolve to a single-platform manifest.
        let (manifest_ct, manifest_bytes, manifest_digest) =
            self.resolve_manifest(image_ref, os, arch).await?;

        // Write the manifest blob.
        let manifest_hex = manifest_digest
            .strip_prefix("sha256:")
            .expect("resolve_manifest always returns sha256: prefix");
        tokio::fs::write(blobs_dir.join(manifest_hex), &manifest_bytes)
            .await
            .context("writing manifest blob")?;

        // Write a synthetic index.json pointing at the resolved manifest.
        //
        // Using the actual content-type from the registry (which may be a
        // Docker manifest media type) ensures load_manifest_blob routes the
        // blob through the correct parsing branch.
        let index = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": MT_OCI_INDEX,
            "manifests": [{
                "mediaType": manifest_ct,
                "digest": manifest_digest,
                "size": manifest_bytes.len()
            }]
        });
        tokio::fs::write(
            output_dir.join("index.json"),
            serde_json::to_vec_pretty(&index)?,
        )
        .await
        .context("writing index.json")?;

        if manifest_only {
            return Ok(());
        }

        // Parse the resolved manifest to get the layer list.
        let envelope: RawEnvelope =
            serde_json::from_slice(&manifest_bytes).context("parsing manifest for layer list")?;

        if envelope.layers.is_empty() {
            bail!(
                "resolved manifest has no layers; the image may require a different \
                 --platform value (try linux/amd64 or linux/arm64)"
            );
        }

        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10));

        // Download the config blob if present.  ocirender's own convert paths
        // don't use it, but a complete OCI image layout requires it so that
        // other tools (e.g. umoci) can consume the layout directory.
        if let Some(config) = &envelope.config {
            let hex = config
                .digest
                .strip_prefix("sha256:")
                .with_context(|| {
                    format!("config digest missing sha256: prefix: {}", config.digest)
                })?
                .to_string();
            let dest = blobs_dir.join(&hex);
            if check_local_blob(&dest, &config.digest).await? {
                multi
                    .println(format!("  ✓ (cached) [config] {}", abbrev(&config.digest)))
                    .ok();
            } else {
                let pb = multi.add(ProgressBar::new(config.size));
                pb.set_style(download_style());
                pb.set_message(format!("[config] {}  ", abbrev(&config.digest)));
                self.download_blob(image_ref, &config.digest, &dest, Some(&pb))
                    .await
                    .context("downloading config blob")?;
                pb.finish_with_message(format!("[config] {} ✓", abbrev(&config.digest)));
            }
        }

        let total = envelope.layers.len();

        // Populate the work queue highest-index-first so workers prioritise
        // the layers that a downstream streaming convert would consume first.
        let queue: Arc<Mutex<VecDeque<(usize, RawDescriptor)>>> = Arc::new(Mutex::new(
            envelope
                .layers
                .iter()
                .enumerate()
                .map(|(i, l)| (i, l.clone()))
                .rev()
                .collect(),
        ));

        let mut tasks: JoinSet<Result<()>> = JoinSet::new();

        for _ in 0..concurrency.max(1).min(total) {
            let queue = Arc::clone(&queue);
            let client = self.clone();
            let image_ref = image_ref.clone();
            let blobs_dir = blobs_dir.clone();
            let multi = multi.clone();

            tasks.spawn(async move {
                loop {
                    let item = queue.lock().await.pop_front();
                    let Some((idx, layer)) = item else { break };

                    let hex = layer
                        .digest
                        .strip_prefix("sha256:")
                        .with_context(|| {
                            format!("layer digest missing sha256: prefix: {}", layer.digest)
                        })?
                        .to_string();
                    let dest = blobs_dir.join(&hex);

                    if check_local_blob(&dest, &layer.digest).await? {
                        multi
                            .println(format!(
                                "  ✓ (cached) [{}/{}] {}",
                                idx + 1,
                                total,
                                abbrev(&layer.digest)
                            ))
                            .ok();
                        continue;
                    }

                    let pb = multi.add(ProgressBar::new(layer.size));
                    pb.set_style(download_style());
                    // Trailing "  " matches the width of the " ✓" suffix used
                    // on completion, keeping all columns aligned.
                    pb.set_message(format!(
                        "[{:2}/{:2}] {}  ",
                        idx + 1,
                        total,
                        abbrev(&layer.digest)
                    ));

                    client
                        .download_blob(&image_ref, &layer.digest, &dest, Some(&pb))
                        .await
                        .with_context(|| format!("downloading layer {}", layer.digest))?;

                    pb.finish_with_message(format!(
                        "[{:2}/{:2}] {} ✓",
                        idx + 1,
                        total,
                        abbrev(&layer.digest)
                    ));
                }
                Ok(())
            });
        }

        while let Some(res) = tasks.join_next().await {
            res.context("download task panicked")??;
        }
        multi.clear().ok();
        Ok(())
    }

    // ── `pull` subcommand ─────────────────────────────────────────────────────

    /// Pull an OCI image from the registry and write it directly to `spec`,
    /// using [`StreamingPacker`] to pipeline layer downloads with assembly.
    ///
    /// Up to `concurrency` worker tasks run in parallel, each pulling from a
    /// shared queue ordered **highest-index-first**.  The merge engine
    /// processes layers in the same order, so a worker that finishes the
    /// highest-remaining layer immediately unblocks the next packing step.
    ///
    /// A [`MultiProgress`] display shows per-layer download throughput and an
    /// overall packing progress bar driven by [`PackerProgress`] events from
    /// the merge engine.
    pub async fn pull_image(
        &self,
        image_ref: &ImageReference,
        os: &str,
        arch: &str,
        spec: ImageSpec,
        concurrency: usize,
    ) -> Result<()> {
        let (_ct, manifest_bytes, _digest) = self.resolve_manifest(image_ref, os, arch).await?;
        let envelope: RawEnvelope =
            serde_json::from_slice(&manifest_bytes).context("parsing manifest")?;

        if envelope.layers.is_empty() {
            bail!("resolved manifest has no layers; check --platform");
        }

        let total = envelope.layers.len();
        let layer_metas: Vec<LayerMeta> = envelope
            .layers
            .iter()
            .enumerate()
            .map(|(idx, l)| LayerMeta {
                index: idx,
                media_type: l.media_type.clone(),
            })
            .collect();

        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10));

        // The packing bar is added first so it appears at the top of the
        // multi-progress display, above the per-layer download bars.
        let pack_pb = multi.add(ProgressBar::new(total as u64));
        pack_pb.set_style(pack_style());

        // Relay PackerProgress events from the merge engine to the packing bar
        // in a detached task.  The task exits naturally when the progress
        // sender is dropped inside `packer.finish()`.
        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::channel::<PackerProgress>(total * 2 + 1);
        let pack_pb_relay = pack_pb.clone();
        tokio::spawn(async move {
            while let Some(event) = progress_rx.recv().await {
                match event {
                    PackerProgress::LayerStarted(idx) => {
                        pack_pb_relay.set_message(format!("layer {}", idx + 1));
                    }
                    PackerProgress::LayerFinished(_) => {
                        pack_pb_relay.inc(1);
                    }
                }
            }
            pack_pb_relay.finish_with_message("done");
        });

        let packer = Arc::new(StreamingPacker::new(layer_metas, spec, Some(progress_tx)));
        // Temp files must outlive `packer.finish()` — the merge thread reads
        // them via the PathBufs supplied to `notify_layer_ready`.
        let tmp = Arc::new(tempfile::tempdir().context("creating temp dir for layer blobs")?);

        let queue: Arc<Mutex<VecDeque<(usize, RawDescriptor)>>> = Arc::new(Mutex::new(
            envelope
                .layers
                .iter()
                .enumerate()
                .map(|(i, l)| (i, l.clone()))
                .rev()
                .collect(),
        ));

        let mut tasks: JoinSet<Result<()>> = JoinSet::new();

        for _ in 0..concurrency.max(1).min(total) {
            let queue = Arc::clone(&queue);
            let packer = Arc::clone(&packer);
            let tmp = Arc::clone(&tmp);
            let client = self.clone();
            let image_ref = image_ref.clone();
            let multi = multi.clone();

            tasks.spawn(async move {
                loop {
                    let item = queue.lock().await.pop_front();
                    let Some((idx, layer)) = item else { break };

                    let dest = tmp.path().join(format!("layer-{idx}"));

                    let pb = multi.add(ProgressBar::new(layer.size));
                    pb.set_style(download_style());
                    pb.set_message(format!(
                        "[{:2}/{:2}] {}  ",
                        idx + 1,
                        total,
                        abbrev(&layer.digest)
                    ));

                    client
                        .download_blob(&image_ref, &layer.digest, &dest, Some(&pb))
                        .await
                        .with_context(|| format!("downloading layer {idx}"))?;

                    pb.finish_with_message(format!(
                        "[{:2}/{:2}] {} ✓",
                        idx + 1,
                        total,
                        abbrev(&layer.digest)
                    ));

                    // Notify the packer immediately — the merge engine can
                    // start processing this layer while remaining downloads
                    // are still in flight.
                    packer
                        .notify_layer_ready(idx, dest)
                        .await
                        .context("merge engine channel closed — packer likely failed")?;
                }
                Ok(())
            });
        }

        // Drain all tasks, collecting the first download error so it can be
        // forwarded to the packer for a clean shutdown rather than leaving
        // the merge thread blocked on recv.
        let mut first_err: Option<anyhow::Error> = None;
        while let Some(res) = tasks.join_next().await {
            let result = match res {
                Ok(inner) => inner,
                Err(e) => Err(anyhow::anyhow!("download task panicked: {e}")),
            };
            if let Err(e) = result
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }

        if let Some(e) = first_err {
            packer.notify_error(e).await;
        }

        // All task Arc clones are dropped — try_unwrap is guaranteed to succeed.
        let packer = Arc::try_unwrap(packer)
            .map_err(|_| anyhow::anyhow!("bug: packer Arc still held after JoinSet drained"))?;

        let result = packer.finish().await;
        drop(tmp); // keep temp files alive until merge thread is done
        multi.clear().ok();
        result
    }
}

// ── free functions ────────────────────────────────────────────────────────────

fn is_index_type(ct: &str) -> bool {
    ct == MT_OCI_INDEX || ct == MT_DOCKER_LIST
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex_encode(&h.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

/// Select the descriptor from an index whose platform matches `(os, arch)`.
fn select_platform<'a>(
    descs: &'a [RawDescriptor],
    os: &str,
    arch: &str,
) -> Option<&'a RawDescriptor> {
    descs.iter().find(|d| {
        d.platform
            .as_ref()
            .is_some_and(|p| p.os == os && p.architecture == arch)
    })
}

/// Return the first 19 characters of `digest` for display: `sha256:<12 hex>`.
fn abbrev(digest: &str) -> &str {
    &digest[..digest.len().min(19)]
}

async fn check_status(resp: reqwest::Response, url: &str) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_else(|_| "(unreadable)".into());
    let snippet: String = body.chars().take(300).collect();
    bail!("HTTP {status} from {url}: {snippet}");
}

/// Verify that `dest` holds a blob matching `digest`.
///
/// Returns `true` if the file exists and its SHA-256 matches the expected
/// value.  If the file exists but fails verification (corrupt or incomplete
/// prior download), it is removed and `false` is returned so the caller
/// re-downloads cleanly.
async fn check_local_blob(dest: &Path, digest: &str) -> Result<bool> {
    if tokio::fs::metadata(dest).await.is_err() {
        return Ok(false);
    }
    let expected = match digest.strip_prefix("sha256:") {
        Some(h) => h.to_string(),
        None => return Ok(false),
    };
    let dest_buf = dest.to_path_buf();
    let actual = tokio::task::spawn_blocking(move || hash_file_sync(&dest_buf))
        .await
        .context("spawning digest verification task")??;
    if actual == expected {
        return Ok(true);
    }
    // Digest mismatch — remove the corrupt or incomplete file.
    tokio::fs::remove_file(dest).await.ok();
    Ok(false)
}

/// Compute SHA-256 of a file synchronously, suitable for `spawn_blocking`.
fn hash_file_sync(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {} for digest verification", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

/// Progress bar style for an active layer download.
///
/// Example rendering:
/// ```text
///  ⠋ [4/5] sha256:a1b2c3d   ████████████░░░░░░░░░░░░░░░░░░   8.3 MiB /  12.1 MiB  10.2 MiB/s
/// ```
fn download_style() -> ProgressStyle {
    ProgressStyle::with_template(
        " {spinner:.cyan} {msg:<22}  {bar:30.cyan/blue}  {bytes:>10} / {total_bytes:<10}  {bytes_per_sec}",
    )
    .unwrap()
    .progress_chars("█▉▊▋▌▍▎▏  ")
}

/// Progress bar style for the packing stage.
///
/// Example rendering:
/// ```text
///  ⠋ [packing]   ████████████████░░░░░░░░░░░░░░   3 / 5 layers  layer 4
/// ```
fn pack_style() -> ProgressStyle {
    ProgressStyle::with_template(
        " {spinner:.yellow} [packing]   {bar:30.yellow/white}  {pos:>2} / {len} layers  {msg}",
    )
    .unwrap()
    .progress_chars("█▉▊▋▌▍▎▏  ")
}
