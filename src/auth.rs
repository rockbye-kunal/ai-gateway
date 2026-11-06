//! Client authentication. Only SHA-256 hashes of gateway keys are stored in
//! config, so a leaked config file does not leak usable keys.

use std::{collections::HashMap, sync::Arc};

use axum::http::{header::AUTHORIZATION, HeaderMap};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::{config::KeyConfig, error::ApiError};

const MAX_KEY_LEN: usize = 512;

#[derive(Debug)]
pub struct Tenant {
    pub name: String,
    pub requests_per_minute: Option<u64>,
    pub daily_token_budget: Option<u64>,
    /// Per-instance in-flight cap for this tenant, if configured.
    pub concurrency: Option<Arc<Semaphore>>,
    allowed: Vec<String>,
}

impl Tenant {
    /// Whether this key may call a configured route by name.
    pub fn allows_route(&self, route: &str) -> bool {
        self.allowed.iter().any(|p| p == "*" || p == route)
    }

    /// Whether this key may call `provider/model` directly, bypassing routes.
    pub fn allows_direct(&self, provider: &str, model: &str) -> bool {
        self.allowed.iter().any(|p| {
            if p == "*" {
                return true;
            }
            if let Some(prefix) = p.strip_suffix("/*") {
                return prefix == provider;
            }
            matches!(p.split_once('/'), Some((pp, mm)) if pp == provider && mm == model)
        })
    }
}

pub struct KeyRing {
    by_hash: HashMap<String, Arc<Tenant>>,
}

impl KeyRing {
    pub fn new(keys: &[KeyConfig]) -> Self {
        let mut by_hash = HashMap::new();
        for k in keys {
            let tenant = Arc::new(Tenant {
                name: k.name.clone(),
                requests_per_minute: k.requests_per_minute,
                daily_token_budget: k.daily_token_budget,
                concurrency: k.max_concurrent_requests.map(|n| Arc::new(Semaphore::new(n))),
                allowed: k.allowed_models.clone(),
            });
            // Current and previous keys map to the same tenant, so a rotation
            // keeps the tenant's limits, budget and metrics.
            for hash in std::iter::once(&k.key_sha256).chain(&k.previous_key_sha256) {
                by_hash.insert(hash.clone(), tenant.clone());
            }
        }
        Self { by_hash }
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Arc<Tenant>, ApiError> {
        let key = extract_key(headers)
            .ok_or_else(|| ApiError::unauthorized("missing API key; send 'Authorization: Bearer <key>'"))?;
        // Keys are high-entropy random strings, so looking up their hash is
        // safe against timing attacks: learning which hash prefix matched says
        // nothing useful about any real key.
        self.by_hash.get(&hash_key(key)).cloned().ok_or_else(|| ApiError::unauthorized("invalid API key"))
    }
}

fn extract_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get(AUTHORIZATION) {
        let v = v.to_str().ok()?.trim();
        // to_str() guarantees visible ASCII, so byte slicing is safe.
        if v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ") {
            let k = v[7..].trim();
            if !k.is_empty() && k.len() <= MAX_KEY_LEN {
                return Some(k);
            }
        }
        return None;
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.len() <= MAX_KEY_LEN)
}

pub fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

/// A new random gateway key: `gw_` followed by 256 bits of hex.
pub fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("gw_{}", hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ring() -> KeyRing {
        KeyRing::new(&[KeyConfig {
            name: "acme".into(),
            key_sha256: hash_key("secret-key"),
            previous_key_sha256: vec![hash_key("old-key")],
            requests_per_minute: None,
            daily_token_budget: None,
            max_concurrent_requests: None,
            allowed_models: vec!["smart".into(), "openai/*".into(), "anthropic/claude-x".into()],
        }])
    }

    #[test]
    fn previous_key_maps_to_same_tenant() {
        let r = ring();
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer old-key"));
        assert_eq!(r.authenticate(&h).unwrap().name, "acme");
    }

    #[test]
    fn bearer_and_x_api_key() {
        let r = ring();
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret-key"));
        assert_eq!(r.authenticate(&h).unwrap().name, "acme");

        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("secret-key"));
        assert!(r.authenticate(&h).is_ok());

        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer wrong"));
        assert!(r.authenticate(&h).is_err());

        assert!(r.authenticate(&HeaderMap::new()).is_err());
    }

    #[test]
    fn permissions() {
        let r = ring();
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("bearer secret-key"));
        let t = r.authenticate(&h).unwrap();
        assert!(t.allows_route("smart"));
        assert!(!t.allows_route("other"));
        assert!(t.allows_direct("openai", "gpt-4o"));
        assert!(t.allows_direct("anthropic", "claude-x"));
        assert!(!t.allows_direct("anthropic", "claude-y"));
    }

    #[test]
    fn generated_keys_are_unique() {
        let a = generate_key();
        assert!(a.starts_with("gw_") && a.len() == 67);
        assert_ne!(a, generate_key());
    }
}
