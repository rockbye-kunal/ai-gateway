//! Tests against a real Redis. Skipped unless AIGW_TEST_REDIS_URL is set
//! (CI sets it and starts a Redis service).

use ai_gateway::{
    config::{FailMode, Secret, StoreBackend, StoreConfig},
    store::Store,
};

async fn redis_store() -> Option<Store> {
    let url = std::env::var("AIGW_TEST_REDIS_URL").ok()?;
    let cfg = StoreConfig {
        backend: StoreBackend::Redis,
        redis_url: Some(Secret::new(url)),
        key_prefix: format!("aigw-store-test-{}:", uuid::Uuid::new_v4()),
        op_timeout_ms: 2_000,
        on_error: FailMode::Closed,
        memory_max_cache_entries: 100,
    };
    Some(Store::connect(&cfg).await.expect("connect to test redis"))
}

#[tokio::test]
async fn counters_and_expiry() {
    let Some(store) = redis_store().await else {
        eprintln!("skipping: AIGW_TEST_REDIS_URL not set");
        return;
    };
    assert_eq!(store.get_counter("c").await.unwrap(), 0);
    assert_eq!(store.incr("c", 3, 60).await.unwrap(), 3);
    assert_eq!(store.incr("c", 4, 60).await.unwrap(), 7);
    assert_eq!(store.get_counter("c").await.unwrap(), 7);

    store.incr("short", 1, 1).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
    assert_eq!(store.get_counter("short").await.unwrap(), 0, "counter expires");
}

#[tokio::test]
async fn usage_writes_are_idempotent() {
    let Some(store) = redis_store().await else {
        eprintln!("skipping: AIGW_TEST_REDIS_URL not set");
        return;
    };
    // Same tenant hash tag on both keys, as the gateway uses (Cluster-safe).
    let budget = "budget:{acme}:1";
    for _ in 0..3 {
        store.incr_once(budget, "usage-done:{acme}:req-1", 50, 60).await.unwrap();
    }
    assert_eq!(store.get_counter(budget).await.unwrap(), 50, "retries never double-count");
    store.incr_once(budget, "usage-done:{acme}:req-2", 25, 60).await.unwrap();
    assert_eq!(store.get_counter(budget).await.unwrap(), 75);
}

#[tokio::test]
async fn cache_round_trip() {
    let Some(store) = redis_store().await else {
        eprintln!("skipping: AIGW_TEST_REDIS_URL not set");
        return;
    };
    assert!(store.cache_get("k").await.unwrap().is_none());
    store.cache_set("k", b"{\"ok\":true}", 60).await.unwrap();
    assert_eq!(store.cache_get("k").await.unwrap().as_deref(), Some(&b"{\"ok\":true}"[..]));
}
