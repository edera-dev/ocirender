//! Registry credential storage backed by `~/.docker/config.json`.
//!
//! [`CredentialStore`] loads the `auths` map from the Docker credential file
//! and makes credentials available to the registry client for authenticated
//! Bearer token requests.  Credential helpers (`credHelpers`, `credsStore`)
//! are not supported; only inline `auth` entries are read.
//!
//! The file format is shared with Docker, crane, skopeo, and other OCI tools,
//! so credentials added by any of those tools are automatically available
//! here, and credentials added by `ocirender login` are automatically
//! available to those tools.
//!
//! Unknown fields in `~/.docker/config.json` (e.g. `HttpHeaders`,
//! `credHelpers`) are preserved when writing so that other tools' config is
//! not disturbed.

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{collections::HashMap, path::PathBuf};

/// A set of registry credentials loaded from `~/.docker/config.json`.
#[derive(Debug, Default, Clone)]
pub struct CredentialStore {
    /// Registry hostname → decoded credentials.
    auths: HashMap<String, Credentials>,
}

/// Username/password pair for a single registry.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl CredentialStore {
    /// Load credentials from `~/.docker/config.json` (or `$DOCKER_CONFIG/config.json`).
    ///
    /// Returns an empty store (not an error) if the file does not exist or
    /// contains no `auths` entries.  Entries that cannot be decoded (wrong
    /// base64, missing `:` separator) are silently skipped.
    pub fn load() -> Result<Self> {
        let path = docker_config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: serde_json::Value =
            serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;

        let mut auths = HashMap::new();
        if let Some(map) = config["auths"].as_object() {
            for (registry, entry) in map {
                let Some(encoded) = entry["auth"].as_str() else {
                    continue;
                };
                if encoded.is_empty() {
                    continue;
                }
                let Ok(decoded) = STANDARD.decode(encoded) else {
                    continue;
                };
                let Ok(s) = String::from_utf8(decoded) else {
                    continue;
                };
                let Some((username, password)) = s.split_once(':') else {
                    continue;
                };
                auths.insert(
                    registry.clone(),
                    Credentials {
                        username: username.to_string(),
                        password: password.to_string(),
                    },
                );
            }
        }
        Ok(Self { auths })
    }

    /// Look up credentials for `registry`.
    ///
    /// Tries the registry string as-is first, then the legacy Docker Hub
    /// key (`https://index.docker.io/v1/`) when the registry is
    /// `registry-1.docker.io`.  Returns `None` if no credentials are stored
    /// for this registry.
    pub fn lookup(&self, registry: &str) -> Option<&Credentials> {
        if let Some(c) = self.auths.get(registry) {
            return Some(c);
        }
        // Docker CLI stores Docker Hub creds under the legacy v1 key.
        if registry == "registry-1.docker.io" {
            return self.auths.get("https://index.docker.io/v1/");
        }
        None
    }

    /// Store credentials for `registry` in `~/.docker/config.json`.
    ///
    /// Creates the file (and `~/.docker/` directory) if they do not exist.
    /// Unknown fields already present in the file are preserved.
    pub fn save(registry: &str, username: &str, password: &str) -> Result<()> {
        let path = docker_config_path();

        // Load the existing file as a raw JSON value so unknown fields
        // (credHelpers, HttpHeaders, etc.) are round-tripped unchanged.
        let mut config: serde_json::Value = if path.exists() {
            let data = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?
        } else {
            serde_json::json!({})
        };

        let auth_value = STANDARD.encode(format!("{username}:{password}").as_bytes());

        // Ensure the auths object exists, then set our entry.
        if config["auths"].is_null() {
            config["auths"] = serde_json::json!({});
        }
        config["auths"][registry] = serde_json::json!({ "auth": auth_value });

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&config)?)
            .with_context(|| format!("writing {}", path.display()))?;

        Ok(())
    }
}

/// Return the path to `config.json` inside the Docker config directory.
///
/// Respects `$DOCKER_CONFIG` when set (Docker's own override mechanism),
/// otherwise falls back to `~/.docker/config.json`.
fn docker_config_path() -> PathBuf {
    if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
        return PathBuf::from(dir).join("config.json");
    }
    // $HOME on Linux/macOS, $USERPROFILE on Windows.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".docker").join("config.json")
}
