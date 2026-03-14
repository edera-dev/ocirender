//! OCI image reference parsing.
//!
//! Normalises Docker Hub short-form names and applies the default tag `latest`
//! so that every reference the client sees is fully qualified.

use anyhow::Result;

/// A parsed, fully-qualified OCI image reference.
#[derive(Debug, Clone)]
pub struct ImageReference {
    /// API hostname used in HTTP requests.
    ///
    /// For Docker Hub this is `registry-1.docker.io`; the human-facing name
    /// `docker.io` is not used for API calls.
    pub registry: String,
    /// Repository name including namespace (e.g. `library/nginx`, `myorg/myimage`).
    pub repository: String,
    /// Tag or digest used to address the manifest.
    pub reference: Ref,
}

/// The addressing component of an image reference.
#[derive(Debug, Clone)]
pub enum Ref {
    Tag(String),
    Digest(String),
}

impl std::fmt::Display for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ref::Tag(t) => write!(f, "{t}"),
            Ref::Digest(d) => write!(f, "{d}"),
        }
    }
}

impl std::fmt::Display for ImageReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}:{}",
            self.registry, self.repository, self.reference
        )
    }
}

impl ImageReference {
    /// Parse a human-supplied image reference string.
    ///
    /// # Normalisation rules
    ///
    /// - No registry component → `docker.io`
    /// - Docker Hub single-component name → `library/<name>` (e.g. `nginx` →
    ///   `library/nginx`)
    /// - `docker.io` API hostname → `registry-1.docker.io`
    /// - No tag or digest → tag `latest`
    ///
    /// # Examples
    ///
    /// ```text
    /// "nginx"                        → registry-1.docker.io / library/nginx :latest
    /// "nginx:1.25"                   → registry-1.docker.io / library/nginx :1.25
    /// "user/app"                     → registry-1.docker.io / user/app      :latest
    /// "ghcr.io/myorg/myimage:v2"     → ghcr.io              / myorg/myimage :v2
    /// "localhost:5000/myimage:dev"   → localhost:5000        / myimage       :dev
    /// "img@sha256:abc..."            → registry-1.docker.io / library/img   :sha256:abc...
    /// ```
    pub fn parse(s: &str) -> Result<Self> {
        // Peel off a @digest suffix before anything else.
        let (name_and_tag, digest) = match s.find('@') {
            Some(pos) => (&s[..pos], Some(s[pos + 1..].to_string())),
            None => (s, None),
        };

        // Peel off a :tag suffix.  A colon is a port separator (not a tag
        // delimiter) when a '/' appears after it, so we check the suffix.
        let (name_part, tag) = if digest.is_some() {
            (name_and_tag, None)
        } else {
            match name_and_tag.rfind(':') {
                Some(pos) if !name_and_tag[pos + 1..].contains('/') => (
                    &name_and_tag[..pos],
                    Some(name_and_tag[pos + 1..].to_string()),
                ),
                _ => (name_and_tag, None),
            }
        };

        if name_part.is_empty() {
            anyhow::bail!("empty image name in reference: {s:?}");
        }

        let (registry, repository) = split_registry_repo(name_part);

        let reference = match (digest, tag) {
            (Some(d), _) => Ref::Digest(d),
            (_, Some(t)) => Ref::Tag(t),
            _ => Ref::Tag("latest".into()),
        };

        Ok(Self {
            registry,
            repository,
            reference,
        })
    }

    /// The reference string (tag or digest) used in manifest API URL paths.
    pub fn reference_str(&self) -> &str {
        match &self.reference {
            Ref::Tag(t) => t,
            Ref::Digest(d) => d,
        }
    }

    /// Return a copy of this reference with the reference component replaced
    /// by `digest`.  Used to fetch a platform-specific manifest by content
    /// address after resolving an index.
    pub fn with_digest(&self, digest: &str) -> Self {
        Self {
            registry: self.registry.clone(),
            repository: self.repository.clone(),
            reference: Ref::Digest(digest.to_string()),
        }
    }

    /// The `scope` string for Bearer token requests against this repository.
    pub fn pull_scope(&self) -> String {
        format!("repository:{}:pull", self.repository)
    }
}

/// Split a name string (tag already stripped) into `(api_registry, repository)`.
fn split_registry_repo(name: &str) -> (String, String) {
    let (registry_raw, repo_raw) = match name.find('/') {
        Some(pos) if looks_like_registry(&name[..pos]) => {
            (name[..pos].to_string(), name[pos + 1..].to_string())
        }
        _ => ("docker.io".to_string(), name.to_string()),
    };

    // Docker Hub single-component names need the "library/" namespace.
    let repository = if registry_raw == "docker.io" && !repo_raw.contains('/') {
        format!("library/{repo_raw}")
    } else {
        repo_raw
    };

    // Docker Hub's REST API lives at registry-1.docker.io.
    let registry = if registry_raw == "docker.io" {
        "registry-1.docker.io".to_string()
    } else {
        registry_raw
    };

    (registry, repository)
}

/// Return `true` if `s` looks like a registry hostname rather than a
/// namespace component.  A component is a registry if it contains a dot
/// (domain), a colon (port), or is literally `localhost`.
fn looks_like_registry(s: &str) -> bool {
    s == "localhost" || s.contains('.') || s.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> (String, String, String) {
        let r = ImageReference::parse(s).unwrap();
        (r.registry, r.repository, r.reference.to_string())
    }

    #[test]
    fn docker_short_names() {
        assert_eq!(
            parse("nginx"),
            (
                "registry-1.docker.io".into(),
                "library/nginx".into(),
                "latest".into()
            )
        );
        assert_eq!(
            parse("nginx:1.25"),
            (
                "registry-1.docker.io".into(),
                "library/nginx".into(),
                "1.25".into()
            )
        );
    }

    #[test]
    fn docker_namespaced() {
        assert_eq!(
            parse("user/app:v2"),
            (
                "registry-1.docker.io".into(),
                "user/app".into(),
                "v2".into()
            )
        );
    }

    #[test]
    fn external_registry() {
        assert_eq!(
            parse("ghcr.io/myorg/myimage:main"),
            ("ghcr.io".into(), "myorg/myimage".into(), "main".into())
        );
    }

    #[test]
    fn localhost_with_port() {
        assert_eq!(
            parse("localhost:5000/myimage:dev"),
            ("localhost:5000".into(), "myimage".into(), "dev".into())
        );
    }

    #[test]
    fn registry_with_port() {
        assert_eq!(
            parse("registry.example.com:5000/foo/bar:tag"),
            (
                "registry.example.com:5000".into(),
                "foo/bar".into(),
                "tag".into()
            )
        );
    }

    #[test]
    fn digest_reference() {
        let r = ImageReference::parse("ubuntu@sha256:abcdef").unwrap();
        assert_eq!(r.repository, "library/ubuntu");
        assert!(matches!(r.reference, Ref::Digest(_)));
    }
}
