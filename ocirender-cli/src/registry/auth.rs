//! OCI registry Bearer token authentication.
//!
//! Implements the standard two-step flow:
//!
//! 1. A request to the registry returns `401 Unauthorized` with a
//!    `WWW-Authenticate: Bearer realm="...",service="...",scope="..."` header.
//! 2. We fetch a short-lived token from the `realm` URL with `service` and
//!    `scope` query parameters.
//! 3. We retry the original request with `Authorization: Bearer <token>`.
//!
//! Token caching (per registry + scope) lives in [`crate::registry::client`]
//! so it can be shared across concurrent tasks.

use std::collections::HashMap;

/// Parsed parameters from a `WWW-Authenticate: Bearer ...` challenge header.
#[derive(Debug)]
pub struct BearerChallenge {
    /// Token endpoint URL (e.g. `https://auth.docker.io/token`).
    pub realm: String,
    /// `service` query parameter value, if present in the challenge.
    pub service: Option<String>,
}

impl BearerChallenge {
    /// Parse a `WWW-Authenticate` header value.
    ///
    /// Returns `None` for non-`Bearer` schemes or unparseable values.
    pub fn parse(header: &str) -> Option<Self> {
        let rest = header.strip_prefix("Bearer ")?;
        let params = parse_kv_pairs(rest);
        let realm = params.get("realm")?.clone();
        let service = params.get("service").cloned();
        Some(Self { realm, service })
    }

    /// Build the token endpoint URL for `scope`.
    ///
    /// Appends `service` (if present) and `scope` as query parameters.
    /// OCI registries accept unencoded `:` and `/` characters in query
    /// values, so no percent-encoding is applied.
    pub fn token_url(&self, scope: &str) -> String {
        let mut url = self.realm.clone();
        url.push(if url.contains('?') { '&' } else { '?' });
        if let Some(svc) = &self.service {
            url.push_str("service=");
            url.push_str(svc);
            url.push('&');
        }
        url.push_str("scope=");
        url.push_str(scope);
        url
    }
}

/// Parse comma-separated `key="value"` pairs from a Bearer challenge.
///
/// Not fully RFC 7235-compliant, but handles all known OCI registry
/// responses (Docker Hub, GHCR, Quay, GCR, ECR public).
fn parse_kv_pairs(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in s.split(',') {
        let part = part.trim();
        if let Some(eq) = part.find('=') {
            let key = part[..eq].trim().to_lowercase();
            let val = part[eq + 1..].trim().trim_matches('"').to_string();
            map.insert(key, val);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_hub_challenge() {
        let header = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#;
        let c = BearerChallenge::parse(header).unwrap();
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert_eq!(c.service.as_deref(), Some("registry.docker.io"));
        let url = c.token_url("repository:library/nginx:pull");
        assert!(url.contains("service=registry.docker.io"));
        assert!(url.contains("scope=repository:library/nginx:pull"));
    }

    #[test]
    fn challenge_without_service() {
        let header = r#"Bearer realm="https://ghcr.io/token""#;
        let c = BearerChallenge::parse(header).unwrap();
        assert!(c.service.is_none());
        let url = c.token_url("repository:myorg/myimage:pull");
        assert!(!url.contains("service="));
        assert!(url.contains("scope=repository:myorg/myimage:pull"));
    }

    #[test]
    fn non_bearer_scheme() {
        assert!(BearerChallenge::parse("Basic realm=\"registry\"").is_none());
    }
}
