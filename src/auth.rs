//! Client authentication. Only SHA-256 hashes of gateway keys are stored in
//! config or the store, so a leaked config file or Redis dump does not leak
//! usable keys.
//!
//! Keys come from two places: the config file (changed by a rolling restart)
//! and the admin API (stored in the shared store and reloaded by every
//! replica every `admin.key_refresh_secs`).

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use axum::http::{header::AUTHORIZATION, HeaderMap};
use parking_lot::RwLock;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::{config::KeyConfig, error::ApiError};

/// Store hash holding keys created through the admin API: field = key hash,
/// value = JSON `StoredKey`.
pub const STORED_KEYS: &str = "apikeys";

/// A key created through the admin API. The key itself is never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredKey {
    pub name: String,
    /// First characters of the key, shown so people can tell keys apart.
    pub key_hint: String,
    pub created_at: u64,
    #[serde(default)]
    pub requests_per_minute: Option<u64>,
    #[serde(default)]
    pub daily_token_budget: Option<u64>,
    #[serde(default)]
    pub max_concurrent_requests: Option<usize>,
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub zero_data_retention: bool,
}

/// Where a tenant's key is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    Config,
    Admin,
}

const MAX_KEY_LEN: usize = 512;

#[derive(Debug)]
pub struct Tenant {
    pub name: String,
    pub requests_per_minute: Option<u64>,
    pub daily_token_budget: Option<u64>,
    /// Per-instance in-flight cap for this tenant, if configured.
    pub concurrency: Option<Arc<Semaphore>>,
    pub max_concurrent_requests: Option<usize>,
    pub source: KeySource,
    pub zero_data_retention: bool,
    allowed: Vec<String>,
}

impl Tenant {
    fn from_config(k: &KeyConfig) -> Self {
        Tenant {
            name: k.name.clone(),
            requests_per_minute: k.requests_per_minute,
            daily_token_budget: k.daily_token_budget,
            concurrency: k.max_concurrent_requests.map(|n| Arc::new(Semaphore::new(n))),
            max_concurrent_requests: k.max_concurrent_requests,
            source: KeySource::Config,
            zero_data_retention: k.zero_data_retention,
            allowed: k.allowed_models.clone(),
        }
    }

    fn from_stored(k: &StoredKey) -> Self {
        Tenant {
            name: k.name.clone(),
            requests_per_minute: k.requests_per_minute,
            daily_token_budget: k.daily_token_budget,
            concurrency: k.max_concurrent_requests.map(|n| Arc::new(Semaphore::new(n))),
            max_concurrent_requests: k.max_concurrent_requests,
            source: KeySource::Admin,
            zero_data_retention: k.zero_data_retention,
            allowed: k.allowed_models.clone(),
        }
    }

    pub fn allowed_models(&self) -> &[String] {
        &self.allowed
    }

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

struct ConfigKeys {
    by_hash: HashMap<String, Arc<Tenant>>,
    names: HashSet<String>,
    defs: HashMap<String, (KeyConfig, Arc<Tenant>)>,
}

impl ConfigKeys {
    /// Builds config tenants, reusing `prev` tenants whose definition is
    /// unchanged so their concurrency semaphores carry over a reload.
    fn build(keys: &[KeyConfig], prev: Option<&ConfigKeys>) -> Self {
        let mut by_hash = HashMap::new();
        let mut names = HashSet::new();
        let mut defs = HashMap::new();
        for k in keys {
            let tenant = match prev.and_then(|p| p.defs.get(&k.name)) {
                Some((old, t)) if old == k => t.clone(),
                _ => Arc::new(Tenant::from_config(k)),
            };
            names.insert(k.name.clone());
            // Current and previous keys map to the same tenant, so a rotation
            // keeps the tenant's limits, budget and metrics.
            for hash in std::iter::once(&k.key_sha256).chain(&k.previous_key_sha256) {
                by_hash.insert(hash.clone(), tenant.clone());
            }
            defs.insert(k.name.clone(), (k.clone(), tenant));
        }
        Self { by_hash, names, defs }
    }
}

pub struct KeyRing {
    config: RwLock<ConfigKeys>,
    /// Admin-created keys by hash, with the record they were built from so a
    /// refresh keeps the same tenant (and its concurrency semaphore) when
    /// nothing changed.
    dynamic: RwLock<HashMap<String, (StoredKey, Arc<Tenant>)>>,
    /// Bumped on every local change, so a background refresh that read the
    /// store before the change cannot undo it.
    generation: AtomicU64,
}

impl KeyRing {
    pub fn new(keys: &[KeyConfig]) -> Self {
        Self {
            config: RwLock::new(ConfigKeys::build(keys, None)),
            dynamic: RwLock::new(HashMap::new()),
            generation: AtomicU64::new(0),
        }
    }

    /// Replaces the config-file keys (config reload).
    pub fn set_config(&self, keys: &[KeyConfig]) {
        let mut cur = self.config.write();
        let next = ConfigKeys::build(keys, Some(&cur));
        *cur = next;
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Arc<Tenant>, ApiError> {
        let key = extract_key(headers)
            .ok_or_else(|| ApiError::unauthorized("missing API key; send 'Authorization: Bearer <key>'"))?;
        // Keys are high-entropy random strings, so looking up their hash is
        // safe against timing attacks: learning which hash prefix matched says
        // nothing useful about any real key.
        let hash = hash_key(key);
        if let Some(t) = self.config.read().by_hash.get(&hash) {
            return Ok(t.clone());
        }
        self.dynamic
            .read()
            .get(&hash)
            .map(|(_, t)| t.clone())
            .ok_or_else(|| ApiError::unauthorized("invalid API key"))
    }

    pub fn is_config_name(&self, name: &str) -> bool {
        self.config.read().names.contains(name)
    }

    pub fn is_config_hash(&self, hash: &str) -> bool {
        self.config.read().by_hash.contains_key(hash)
    }

    /// Replaces the admin-created keys with `records` (hash → key), reusing
    /// existing tenants whose record is unchanged. Records whose name clashes
    /// with a config key, or whose hash is a config key, are ignored.
    pub fn replace_dynamic(&self, records: HashMap<String, StoredKey>) {
        let mut cur = self.dynamic.write();
        self.apply_records(&mut cur, records);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Like `replace_dynamic`, but skipped (returning false) if a key was
    /// created or revoked locally since `seen` was read.
    pub fn replace_dynamic_if_unchanged(&self, seen: u64, records: HashMap<String, StoredKey>) -> bool {
        let mut cur = self.dynamic.write();
        if self.generation.load(Ordering::SeqCst) != seen {
            return false;
        }
        self.apply_records(&mut cur, records);
        true
    }

    fn apply_records(
        &self,
        cur: &mut HashMap<String, (StoredKey, Arc<Tenant>)>,
        records: HashMap<String, StoredKey>,
    ) {
        let mut next = HashMap::with_capacity(records.len());
        let cfg = self.config.read();
        for (hash, rec) in records {
            if cfg.names.contains(&rec.name) || cfg.by_hash.contains_key(&hash) {
                continue;
            }
            let tenant = match cur.remove(&hash) {
                Some((old, t)) if old == rec => t,
                _ => Arc::new(Tenant::from_stored(&rec)),
            };
            next.insert(hash, (rec, tenant));
        }
        *cur = next;
    }

    pub fn upsert_dynamic(&self, hash: String, rec: StoredKey) {
        let tenant = Arc::new(Tenant::from_stored(&rec));
        let mut cur = self.dynamic.write();
        self.generation.fetch_add(1, Ordering::SeqCst);
        cur.insert(hash, (rec, tenant));
    }

    pub fn remove_dynamic(&self, hash: &str) {
        let mut cur = self.dynamic.write();
        self.generation.fetch_add(1, Ordering::SeqCst);
        cur.remove(hash);
    }

    /// Every tenant, config keys first, each once, sorted by name within its group.
    pub fn tenants(&self) -> Vec<(Arc<Tenant>, Option<StoredKey>)> {
        let mut config: Vec<(Arc<Tenant>, Option<StoredKey>)> =
            self.config.read().defs.values().map(|(_, t)| (t.clone(), None)).collect();
        config.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let mut dynamic: Vec<(Arc<Tenant>, Option<StoredKey>)> =
            self.dynamic.read().values().map(|(rec, t)| (t.clone(), Some(rec.clone()))).collect();
        dynamic.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        config.extend(dynamic);
        config
    }
}

pub fn extract_key(headers: &HeaderMap) -> Option<&str> {
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

    fn stored(name: &str) -> StoredKey {
        StoredKey {
            name: name.into(),
            key_hint: "gw_1234".into(),
            created_at: 0,
            requests_per_minute: None,
            daily_token_budget: None,
            max_concurrent_requests: Some(2),
            allowed_models: vec!["*".into()],
            zero_data_retention: false,
        }
    }

    #[test]
    fn dynamic_keys_authenticate_and_refresh() {
        let r = ring();
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer dyn-key"));
        assert!(r.authenticate(&h).is_err());

        r.upsert_dynamic(hash_key("dyn-key"), stored("beta"));
        let t1 = r.authenticate(&h).unwrap();
        assert_eq!(t1.name, "beta");
        assert_eq!(t1.source, KeySource::Admin);

        // An unchanged record keeps the same tenant (and semaphore).
        r.replace_dynamic(HashMap::from([(hash_key("dyn-key"), stored("beta"))]));
        assert!(Arc::ptr_eq(&t1, &r.authenticate(&h).unwrap()));

        // Names that clash with config keys are ignored.
        r.replace_dynamic(HashMap::from([(hash_key("dyn-key"), stored("acme"))]));
        assert!(r.authenticate(&h).is_err());

        // A refresh that started before a local change does not undo it.
        let seen = r.generation();
        r.upsert_dynamic(hash_key("dyn-key"), stored("beta"));
        assert!(!r.replace_dynamic_if_unchanged(seen, HashMap::new()));
        assert!(r.authenticate(&h).is_ok());

        r.remove_dynamic(&hash_key("dyn-key"));
        assert!(r.authenticate(&h).is_err());
        assert_eq!(r.tenants().len(), 1);
    }

    fn ring() -> KeyRing {
        KeyRing::new(&[KeyConfig {
            name: "acme".into(),
            key_sha256: hash_key("secret-key"),
            previous_key_sha256: vec![hash_key("old-key")],
            requests_per_minute: None,
            daily_token_budget: None,
            max_concurrent_requests: None,
            allowed_models: vec!["smart".into(), "openai/*".into(), "anthropic/claude-x".into()],
            zero_data_retention: false,
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
