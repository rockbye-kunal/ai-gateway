//! Shared state for rate limits, token budgets and the response cache.
//!
//! `Redis` is shared across replicas and is what you run behind a load
//! balancer. `Memory` keeps everything in-process and suits a single instance
//! or local development.

use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use dashmap::DashMap;
use redis::{aio::ConnectionManager, AsyncCommands, Script};

use crate::config::{StoreBackend, StoreConfig};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("store operation timed out")]
    Timeout,
}

#[derive(Clone)]
pub enum Store {
    Memory(Arc<MemoryStore>),
    Redis(RedisStore),
}

/// INCRBY with an expiry set only when the key has none, in one round trip.
const INCR_LUA: &str = r#"
local v = redis.call('INCRBY', KEYS[1], ARGV[1])
if redis.call('TTL', KEYS[1]) < 0 then
  redis.call('EXPIRE', KEYS[1], ARGV[2])
end
return v
"#;

/// Like INCR_LUA, but applied at most once per `KEYS[2]` marker, so a client
/// retry after a lost reply cannot count the same tokens twice. The marker
/// lives only `ARGV[3]` seconds (retries finish long before that). Both keys
/// must share a `{hash tag}` so this works on Redis Cluster.
const INCR_ONCE_LUA: &str = r#"
if not redis.call('SET', KEYS[2], '1', 'NX', 'EX', ARGV[3]) then
  return -1
end
local v = redis.call('INCRBY', KEYS[1], ARGV[1])
if redis.call('TTL', KEYS[1]) < 0 then
  redis.call('EXPIRE', KEYS[1], ARGV[2])
end
return v
"#;

/// How long an idempotency marker for a usage write is kept.
const USAGE_MARKER_TTL_SECS: u64 = 600;

impl Store {
    pub async fn connect(cfg: &StoreConfig) -> anyhow::Result<Self> {
        match cfg.backend {
            StoreBackend::Memory => {
                let mem = Arc::new(MemoryStore::new(cfg.memory_max_cache_entries));
                let weak = Arc::downgrade(&mem);
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_secs(30));
                    loop {
                        tick.tick().await;
                        match weak.upgrade() {
                            Some(m) => m.purge_expired(),
                            None => break,
                        }
                    }
                });
                Ok(Store::Memory(mem))
            }
            StoreBackend::Redis => {
                let url =
                    cfg.redis_url.as_ref().context("store.redis_url is required for the redis backend")?;
                let client = redis::Client::open(url.expose()).context("invalid store.redis_url")?;
                let conn = tokio::time::timeout(Duration::from_secs(10), client.get_connection_manager())
                    .await
                    .context("timed out connecting to redis")?
                    .context("connecting to redis")?;
                let store = RedisStore {
                    conn,
                    prefix: cfg.key_prefix.clone(),
                    timeout: Duration::from_millis(cfg.op_timeout_ms),
                    incr: Arc::new(Script::new(INCR_LUA)),
                    incr_once: Arc::new(Script::new(INCR_ONCE_LUA)),
                };
                store.ping().await.context("redis did not answer PING")?;
                Ok(Store::Redis(store))
            }
        }
    }

    /// Adds `delta` to a counter and returns the new value. The counter
    /// expires `ttl_secs` after it is first created.
    pub async fn incr(&self, key: &str, delta: u64, ttl_secs: u64) -> Result<u64, StoreError> {
        match self {
            Store::Memory(m) => Ok(m.incr(key, delta, ttl_secs)),
            Store::Redis(r) => r.incr(key, delta, ttl_secs).await,
        }
    }

    /// Adds `delta` to a counter at most once for a given `once_key`, so it
    /// is safe to retry.
    pub async fn incr_once(
        &self,
        key: &str,
        once_key: &str,
        delta: u64,
        ttl_secs: u64,
    ) -> Result<(), StoreError> {
        match self {
            // In-process writes cannot have an ambiguous outcome.
            Store::Memory(m) => {
                m.incr(key, delta, ttl_secs);
                Ok(())
            }
            Store::Redis(r) => r.incr_once(key, once_key, delta, ttl_secs).await,
        }
    }

    pub async fn get_counter(&self, key: &str) -> Result<u64, StoreError> {
        match self {
            Store::Memory(m) => Ok(m.get_counter(key)),
            Store::Redis(r) => r.get_counter(key).await,
        }
    }

    pub async fn cache_get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Store::Memory(m) => Ok(m.cache_get(key)),
            Store::Redis(r) => r.cache_get(key).await,
        }
    }

    pub async fn cache_set(&self, key: &str, value: &[u8], ttl_secs: u64) -> Result<(), StoreError> {
        match self {
            Store::Memory(m) => {
                m.cache_set(key, value, ttl_secs);
                Ok(())
            }
            Store::Redis(r) => r.cache_set(key, value, ttl_secs).await,
        }
    }

    pub async fn ping(&self) -> Result<(), StoreError> {
        match self {
            Store::Memory(_) => Ok(()),
            Store::Redis(r) => r.ping().await,
        }
    }
}

#[derive(Clone)]
pub struct RedisStore {
    conn: ConnectionManager,
    prefix: String,
    timeout: Duration,
    incr: Arc<Script>,
    incr_once: Arc<Script>,
}

impl RedisStore {
    fn key(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }

    /// Every Redis call is bounded so a slow Redis cannot stall requests.
    async fn bounded<T, F>(&self, fut: F) -> Result<T, StoreError>
    where
        F: Future<Output = redis::RedisResult<T>>,
    {
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(r) => Ok(r?),
            Err(_) => Err(StoreError::Timeout),
        }
    }

    async fn incr(&self, key: &str, delta: u64, ttl_secs: u64) -> Result<u64, StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let script = self.incr.clone();
        let value: u64 = self
            .bounded(
                async move { script.key(key).arg(delta).arg(ttl_secs.max(1)).invoke_async(&mut conn).await },
            )
            .await?;
        Ok(value)
    }

    async fn incr_once(
        &self,
        key: &str,
        once_key: &str,
        delta: u64,
        ttl_secs: u64,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let once_key = self.key(once_key);
        let script = self.incr_once.clone();
        let _: i64 = self
            .bounded(async move {
                script
                    .key(key)
                    .key(once_key)
                    .arg(delta)
                    .arg(ttl_secs.max(1))
                    .arg(USAGE_MARKER_TTL_SECS)
                    .invoke_async(&mut conn)
                    .await
            })
            .await?;
        Ok(())
    }

    async fn get_counter(&self, key: &str) -> Result<u64, StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let value: Option<u64> = self.bounded(async move { conn.get(key).await }).await?;
        Ok(value.unwrap_or(0))
    }

    async fn cache_get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let value: Option<Vec<u8>> = self.bounded(async move { conn.get(key).await }).await?;
        Ok(value)
    }

    async fn cache_set(&self, key: &str, value: &[u8], ttl_secs: u64) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let value = value.to_vec();
        let ttl = ttl_secs.max(1);
        let _: () = self.bounded(async move { conn.set_ex(key, value, ttl).await }).await?;
        Ok(())
    }

    async fn ping(&self) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let _: String = self.bounded(async move { redis::cmd("PING").query_async(&mut conn).await }).await?;
        Ok(())
    }
}

pub struct MemoryStore {
    counters: DashMap<String, (u64, Instant)>,
    cache: DashMap<String, (Arc<Vec<u8>>, Instant)>,
    max_cache_entries: usize,
}

impl MemoryStore {
    fn new(max_cache_entries: usize) -> Self {
        Self { counters: DashMap::new(), cache: DashMap::new(), max_cache_entries }
    }

    fn incr(&self, key: &str, delta: u64, ttl_secs: u64) -> u64 {
        let now = Instant::now();
        let ttl = Duration::from_secs(ttl_secs.max(1));
        let mut entry = self.counters.entry(key.to_owned()).or_insert((0, now + ttl));
        let v = entry.value_mut();
        if v.1 <= now {
            *v = (0, now + ttl);
        }
        v.0 = v.0.saturating_add(delta);
        v.0
    }

    fn get_counter(&self, key: &str) -> u64 {
        let now = Instant::now();
        match self.counters.get(key) {
            Some(e) if e.value().1 > now => e.value().0,
            _ => 0,
        }
    }

    fn cache_get(&self, key: &str) -> Option<Vec<u8>> {
        let now = Instant::now();
        match self.cache.get(key) {
            Some(e) if e.value().1 > now => Some(e.value().0.as_ref().clone()),
            _ => None,
        }
    }

    fn cache_set(&self, key: &str, value: &[u8], ttl_secs: u64) {
        if self.cache.len() >= self.max_cache_entries && !self.cache.contains_key(key) {
            let now = Instant::now();
            self.cache.retain(|_, v| v.1 > now);
            if self.cache.len() >= self.max_cache_entries {
                return;
            }
        }
        let expires = Instant::now() + Duration::from_secs(ttl_secs.max(1));
        self.cache.insert(key.to_owned(), (Arc::new(value.to_vec()), expires));
    }

    fn purge_expired(&self) {
        let now = Instant::now();
        self.counters.retain(|_, v| v.1 > now);
        self.cache.retain(|_, v| v.1 > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_counters_and_cache() {
        let m = MemoryStore::new(2);
        assert_eq!(m.incr("a", 1, 60), 1);
        assert_eq!(m.incr("a", 5, 60), 6);
        assert_eq!(m.get_counter("a"), 6);
        assert_eq!(m.get_counter("missing"), 0);

        m.cache_set("k1", b"v1", 60);
        m.cache_set("k2", b"v2", 60);
        m.cache_set("k3", b"v3", 60); // over capacity: dropped, not evicting live entries
        assert_eq!(m.cache_get("k1").as_deref(), Some(&b"v1"[..]));
        assert_eq!(m.cache_get("k3"), None);
    }
}
