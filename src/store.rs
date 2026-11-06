//! Shared state for rate limits, token budgets and the response cache.
//!
//! `Redis` is shared across replicas and is what you run behind a load
//! balancer. `Memory` keeps everything in-process and suits a single instance
//! or local development.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use dashmap::DashMap;
use parking_lot::Mutex;
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
    Redis(Box<RedisStore>),
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
                    // Admin reads and background writes (logs, analytics)
                    // are off the request path and may take longer.
                    slow_timeout: Duration::from_millis(cfg.op_timeout_ms.max(5_000)),
                    incr: Arc::new(Script::new(INCR_LUA)),
                    incr_once: Arc::new(Script::new(INCR_ONCE_LUA)),
                };
                store.ping().await.context("redis did not answer PING")?;
                Ok(Store::Redis(Box::new(store)))
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

    /// Pushes `value` to the front of a list and trims it to `max` items.
    pub async fn list_push_capped(&self, key: &str, value: &str, max: usize) -> Result<(), StoreError> {
        match self {
            Store::Memory(m) => {
                m.list_push_capped(key, value, max);
                Ok(())
            }
            Store::Redis(r) => r.list_push_capped(key, value, max).await,
        }
    }

    /// Up to `limit` items from the front of a list (newest first).
    pub async fn list_range(&self, key: &str, limit: usize) -> Result<Vec<String>, StoreError> {
        match self {
            Store::Memory(m) => Ok(m.list_range(key, limit)),
            Store::Redis(r) => r.list_range(key, limit).await,
        }
    }

    pub async fn kv_set(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), StoreError> {
        match self {
            Store::Memory(m) => {
                m.kv_set(key, value, ttl_secs);
                Ok(())
            }
            Store::Redis(r) => r.kv_set(key, value, ttl_secs).await,
        }
    }

    pub async fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        match self {
            Store::Memory(m) => Ok(m.kv_get(key)),
            Store::Redis(r) => r.kv_get(key).await,
        }
    }

    pub async fn kv_get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        match self {
            Store::Memory(m) => Ok(keys.iter().map(|k| m.kv_get(k)).collect()),
            Store::Redis(r) => r.kv_get_many(keys).await,
        }
    }

    /// Deletes a key of any type.
    pub async fn del(&self, key: &str) -> Result<(), StoreError> {
        match self {
            Store::Memory(m) => {
                m.kv.remove(key);
                m.lists.remove(key);
                m.hashes.remove(key);
                m.counter_hashes.remove(key);
                m.counters.remove(key);
                Ok(())
            }
            Store::Redis(r) => r.del(key).await,
        }
    }

    /// Replaces a list's contents (front first) atomically.
    pub async fn list_replace(&self, key: &str, items: &[String]) -> Result<(), StoreError> {
        match self {
            Store::Memory(m) => {
                m.lists.insert(key.to_owned(), Mutex::new(items.iter().cloned().collect()));
                Ok(())
            }
            Store::Redis(r) => r.list_replace(key, items).await,
        }
    }

    /// Adds each `(field, delta)` to a hash of counters and sets its expiry.
    pub async fn hincr_many(
        &self,
        key: &str,
        fields: &[(String, i64)],
        ttl_secs: u64,
    ) -> Result<(), StoreError> {
        if fields.is_empty() {
            return Ok(());
        }
        match self {
            Store::Memory(m) => {
                m.hincr_many(key, fields, ttl_secs);
                Ok(())
            }
            Store::Redis(r) => r.hincr_many(key, fields, ttl_secs).await,
        }
    }

    /// Reads several counter hashes in one round trip. Missing keys are empty.
    pub async fn hgetall_many(&self, keys: &[String]) -> Result<Vec<HashMap<String, i64>>, StoreError> {
        match self {
            Store::Memory(m) => Ok(keys.iter().map(|k| m.hgetall_counters(k)).collect()),
            Store::Redis(r) => r.hgetall_many(keys).await,
        }
    }

    /// Sets a field of a persistent (non-expiring) string hash.
    pub async fn hset(&self, key: &str, field: &str, value: &str) -> Result<(), StoreError> {
        match self {
            Store::Memory(m) => {
                m.hashes.entry(key.to_owned()).or_default().insert(field.to_owned(), value.to_owned());
                Ok(())
            }
            Store::Redis(r) => r.hset(key, field, value).await,
        }
    }

    /// Removes a field; returns whether it existed.
    pub async fn hdel(&self, key: &str, field: &str) -> Result<bool, StoreError> {
        match self {
            Store::Memory(m) => Ok(m.hashes.get_mut(key).is_some_and(|mut h| h.remove(field).is_some())),
            Store::Redis(r) => r.hdel(key, field).await,
        }
    }

    pub async fn hgetall(&self, key: &str) -> Result<HashMap<String, String>, StoreError> {
        match self {
            Store::Memory(m) => Ok(m.hashes.get(key).map(|h| h.value().clone()).unwrap_or_default()),
            Store::Redis(r) => r.hgetall(key).await,
        }
    }
}

#[derive(Clone)]
pub struct RedisStore {
    conn: ConnectionManager,
    prefix: String,
    timeout: Duration,
    slow_timeout: Duration,
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

    async fn bounded_slow<T, F>(&self, fut: F) -> Result<T, StoreError>
    where
        F: Future<Output = redis::RedisResult<T>>,
    {
        match tokio::time::timeout(self.slow_timeout, fut).await {
            Ok(r) => Ok(r?),
            Err(_) => Err(StoreError::Timeout),
        }
    }

    async fn list_push_capped(&self, key: &str, value: &str, max: usize) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("LPUSH")
            .arg(&key)
            .arg(value)
            .ignore()
            .cmd("LTRIM")
            .arg(&key)
            .arg(0)
            .arg(max.max(1) as i64 - 1)
            .ignore();
        let _: () = self.bounded_slow(async move { pipe.query_async(&mut conn).await }).await?;
        Ok(())
    }

    async fn list_range(&self, key: &str, limit: usize) -> Result<Vec<String>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let end = limit as isize - 1;
        let items: Vec<String> = self.bounded_slow(async move { conn.lrange(key, 0, end).await }).await?;
        Ok(items)
    }

    async fn kv_set(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let value = value.to_string();
        let ttl = ttl_secs.max(1);
        let _: () = self.bounded_slow(async move { conn.set_ex(key, value, ttl).await }).await?;
        Ok(())
    }

    async fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let value: Option<String> = self.bounded_slow(async move { conn.get(key).await }).await?;
        Ok(value)
    }

    async fn kv_get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>, StoreError> {
        let mut conn = self.conn.clone();
        let mut pipe = redis::pipe();
        for k in keys {
            pipe.cmd("GET").arg(self.key(k));
        }
        let out: Vec<Option<String>> =
            self.bounded_slow(async move { pipe.query_async(&mut conn).await }).await?;
        Ok(out)
    }

    async fn del(&self, key: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let _: () = self.bounded_slow(async move { conn.del(key).await }).await?;
        Ok(())
    }

    async fn list_replace(&self, key: &str, items: &[String]) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let mut pipe = redis::pipe();
        pipe.atomic().cmd("DEL").arg(&key).ignore();
        for chunk in items.chunks(1000) {
            pipe.cmd("RPUSH").arg(&key).arg(chunk).ignore();
        }
        let _: () = self.bounded_slow(async move { pipe.query_async(&mut conn).await }).await?;
        Ok(())
    }

    async fn hincr_many(&self, key: &str, fields: &[(String, i64)], ttl_secs: u64) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let mut pipe = redis::pipe();
        for (field, delta) in fields {
            pipe.cmd("HINCRBY").arg(&key).arg(field).arg(*delta).ignore();
        }
        pipe.cmd("EXPIRE").arg(&key).arg(ttl_secs.max(1)).ignore();
        let _: () = self.bounded_slow(async move { pipe.query_async(&mut conn).await }).await?;
        Ok(())
    }

    async fn hgetall_many(&self, keys: &[String]) -> Result<Vec<HashMap<String, i64>>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn.clone();
        let mut pipe = redis::pipe();
        for k in keys {
            pipe.cmd("HGETALL").arg(self.key(k));
        }
        let out: Vec<HashMap<String, i64>> =
            self.bounded_slow(async move { pipe.query_async(&mut conn).await }).await?;
        Ok(out)
    }

    async fn hset(&self, key: &str, field: &str, value: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let (field, value) = (field.to_string(), value.to_string());
        let _: () = self.bounded_slow(async move { conn.hset(key, field, value).await }).await?;
        Ok(())
    }

    async fn hdel(&self, key: &str, field: &str) -> Result<bool, StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let field = field.to_string();
        let removed: i64 = self.bounded_slow(async move { conn.hdel(key, field).await }).await?;
        Ok(removed > 0)
    }

    async fn hgetall(&self, key: &str) -> Result<HashMap<String, String>, StoreError> {
        let mut conn = self.conn.clone();
        let key = self.key(key);
        let value: HashMap<String, String> =
            self.bounded_slow(async move { conn.hgetall(key).await }).await?;
        Ok(value)
    }
}

/// Upper bound on expiring key/value entries (request details) kept in memory.
const MEMORY_MAX_KV_ENTRIES: usize = 50_000;

type CounterHash = (HashMap<String, i64>, Instant);

pub struct MemoryStore {
    counters: DashMap<String, (u64, Instant)>,
    cache: DashMap<String, (Arc<Vec<u8>>, Instant)>,
    max_cache_entries: usize,
    lists: DashMap<String, Mutex<VecDeque<String>>>,
    kv: DashMap<String, (String, Instant)>,
    counter_hashes: DashMap<String, CounterHash>,
    hashes: DashMap<String, HashMap<String, String>>,
}

impl MemoryStore {
    fn new(max_cache_entries: usize) -> Self {
        Self {
            counters: DashMap::new(),
            cache: DashMap::new(),
            max_cache_entries,
            lists: DashMap::new(),
            kv: DashMap::new(),
            counter_hashes: DashMap::new(),
            hashes: DashMap::new(),
        }
    }

    fn list_push_capped(&self, key: &str, value: &str, max: usize) {
        let entry = self.lists.entry(key.to_owned()).or_default();
        let mut list = entry.value().lock();
        list.push_front(value.to_owned());
        list.truncate(max.max(1));
    }

    fn list_range(&self, key: &str, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(l) = self.lists.get(key) {
            out.extend(l.value().lock().iter().take(limit).cloned());
        }
        out
    }

    fn kv_set(&self, key: &str, value: &str, ttl_secs: u64) {
        let now = Instant::now();
        if self.kv.len() >= MEMORY_MAX_KV_ENTRIES && !self.kv.contains_key(key) {
            self.kv.retain(|_, v| v.1 > now);
            if self.kv.len() >= MEMORY_MAX_KV_ENTRIES {
                return;
            }
        }
        self.kv.insert(key.to_owned(), (value.to_owned(), now + Duration::from_secs(ttl_secs.max(1))));
    }

    fn kv_get(&self, key: &str) -> Option<String> {
        match self.kv.get(key) {
            Some(e) if e.value().1 > Instant::now() => Some(e.value().0.clone()),
            _ => None,
        }
    }

    fn hincr_many(&self, key: &str, fields: &[(String, i64)], ttl_secs: u64) {
        let now = Instant::now();
        let expires = now + Duration::from_secs(ttl_secs.max(1));
        let mut entry =
            self.counter_hashes.entry(key.to_owned()).or_insert_with(|| (HashMap::new(), expires));
        let v = entry.value_mut();
        if v.1 <= now {
            v.0.clear();
        }
        v.1 = expires;
        for (field, delta) in fields {
            *v.0.entry(field.clone()).or_insert(0) += *delta;
        }
    }

    fn hgetall_counters(&self, key: &str) -> HashMap<String, i64> {
        match self.counter_hashes.get(key) {
            Some(e) if e.value().1 > Instant::now() => e.value().0.clone(),
            _ => HashMap::new(),
        }
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
        self.kv.retain(|_, v| v.1 > now);
        self.counter_hashes.retain(|_, v| v.1 > now);
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

    #[test]
    fn memory_lists_hashes_and_kv() {
        let m = MemoryStore::new(10);
        for i in 0..5 {
            m.list_push_capped("l", &i.to_string(), 3);
        }
        assert_eq!(m.list_range("l", 10), vec!["4", "3", "2"]);
        assert_eq!(m.list_range("l", 1), vec!["4"]);

        m.hincr_many("h", &[("a".into(), 2), ("b".into(), 1)], 60);
        m.hincr_many("h", &[("a".into(), 3)], 60);
        let h = m.hgetall_counters("h");
        assert_eq!(h["a"], 5);
        assert_eq!(h["b"], 1);
        assert!(m.hgetall_counters("missing").is_empty());

        m.kv_set("k", "v", 60);
        assert_eq!(m.kv_get("k").as_deref(), Some("v"));
        assert_eq!(m.kv_get("nope"), None);
    }
}
