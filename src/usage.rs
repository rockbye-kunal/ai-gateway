//! Per-request accounting.
//!
//! A `RequestGuard` is created for every request and cloned into streaming
//! bodies. When the last clone is dropped (response sent, stream finished, or
//! client disconnected) it writes the access log, records metrics and adds
//! the tokens used to the tenant's daily budget. Doing this on drop means a
//! client that hangs up mid-stream is still billed for what it consumed.

use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;

use crate::{app::AppState, auth::Tenant};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    /// Reads an OpenAI-style `usage` object from a response or chunk.
    pub fn from_openai(v: &Value) -> Option<Usage> {
        let u = v.get("usage")?;
        if !u.is_object() {
            return None;
        }
        let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        Some(Usage { prompt_tokens: get("prompt_tokens"), completion_tokens: get("completion_tokens") })
    }
}

#[derive(Default)]
pub struct RequestState {
    pub tenant: Option<Arc<Tenant>>,
    pub route: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status: u16,
    pub error: Option<String>,
    pub cache_hit: bool,
    pub stream: bool,
    pub attempts: u32,
    pub usage: Usage,
    pub price_input_per_mtok: Option<f64>,
    pub price_output_per_mtok: Option<f64>,
    /// Per-tenant concurrency slot, released when the request finishes.
    pub tenant_permit: Option<OwnedSemaphorePermit>,
}

struct Inner {
    app: Arc<AppState>,
    request_id: String,
    start: Instant,
    state: Mutex<RequestState>,
    // Held for the whole request, including the streaming body, so the
    // concurrency limit counts long streams correctly.
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub struct RequestGuard(Arc<Inner>);

impl RequestGuard {
    pub fn new(app: Arc<AppState>, request_id: String, permit: OwnedSemaphorePermit) -> Self {
        Self(Arc::new(Inner {
            app,
            request_id,
            start: Instant::now(),
            state: Mutex::new(RequestState::default()),
            _permit: permit,
        }))
    }

    pub fn request_id(&self) -> &str {
        &self.0.request_id
    }

    pub fn update(&self, f: impl FnOnce(&mut RequestState)) {
        f(&mut self.0.state.lock());
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let st = std::mem::take(&mut *self.state.lock());
        let latency = self.start.elapsed().as_secs_f64();
        let tenant = st.tenant.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| "-".into());
        let provider = st.provider.clone().unwrap_or_else(|| "-".into());
        let model = st.model.clone().unwrap_or_else(|| "-".into());
        let cache = if st.cache_hit { "hit" } else { "miss" };
        let billable = !st.cache_hit && st.usage.total() > 0;

        let cost_usd = match (st.price_input_per_mtok, st.price_output_per_mtok) {
            (Some(pi), Some(po)) if billable => Some(
                (st.usage.prompt_tokens as f64 * pi + st.usage.completion_tokens as f64 * po) / 1_000_000.0,
            ),
            _ => None,
        };

        metrics::counter!(
            "gateway_requests_total",
            "tenant" => tenant.clone(),
            "provider" => provider.clone(),
            "status" => st.status.to_string(),
            "cache" => cache
        )
        .increment(1);
        metrics::histogram!(
            "gateway_request_duration_seconds",
            "provider" => provider.clone(),
            "stream" => if st.stream { "true" } else { "false" }
        )
        .record(latency);
        if billable {
            metrics::counter!(
                "gateway_tokens_total",
                "tenant" => tenant.clone(), "provider" => provider.clone(),
                "model" => model.clone(), "type" => "prompt"
            )
            .increment(st.usage.prompt_tokens);
            metrics::counter!(
                "gateway_tokens_total",
                "tenant" => tenant.clone(), "provider" => provider.clone(),
                "model" => model.clone(), "type" => "completion"
            )
            .increment(st.usage.completion_tokens);
        }
        if let Some(c) = cost_usd {
            // Recorded in micro-dollars because counters are integers.
            metrics::counter!("gateway_cost_microdollars_total", "tenant" => tenant.clone())
                .increment((c * 1_000_000.0).round() as u64);
        }

        tracing::info!(
            request_id = %self.request_id,
            tenant = %tenant,
            route = st.route.as_deref().unwrap_or("-"),
            provider = %provider,
            model = %model,
            status = st.status,
            stream = st.stream,
            cache = cache,
            attempts = st.attempts,
            prompt_tokens = st.usage.prompt_tokens,
            completion_tokens = st.usage.completion_tokens,
            cost_usd = cost_usd.unwrap_or(0.0),
            latency_ms = (latency * 1000.0) as u64,
            error = st.error.as_deref().unwrap_or(""),
            "request completed"
        );

        if billable {
            if let Some(t) = &st.tenant {
                let key = crate::proxy::budget_key(&t.name);
                let app = self.app.clone();
                let total = st.usage.total();
                let tenant_name = t.name.clone();
                let request_id = self.request_id.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    // Counted so shutdown can wait for these writes instead of
                    // dropping usage that was already consumed.
                    app.pending_usage_writes.fetch_add(1, Ordering::SeqCst);
                    handle.spawn(async move {
                        record_usage(&app, &key, &request_id, total, &tenant_name).await;
                        app.pending_usage_writes.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }
        }
    }
}

/// Adds tokens to the tenant's daily counter, retrying transient store
/// failures for a few seconds before giving up. Writes are idempotent per
/// request id, so a retry after a timed-out reply never double-counts.
async fn record_usage(app: &AppState, key: &str, request_id: &str, tokens: u64, tenant: &str) {
    // Same {tenant} hash tag as the budget key, so both hash to one cluster slot.
    let once_key = format!("usage-done:{{{tenant}}}:{request_id}");
    let mut delay = Duration::from_millis(100);
    for attempt in 1..=5 {
        match app.store.incr_once(key, &once_key, tokens, 2 * 86_400).await {
            Ok(_) => return,
            Err(e) if attempt == 5 => {
                metrics::counter!("gateway_store_errors_total", "op" => "record_usage").increment(1);
                tracing::error!(error = %e, tenant, tokens, "failed to record token usage; budget will undercount");
            }
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
}
