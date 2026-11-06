//! Per-request accounting.
//!
//! A `RequestGuard` is created for every request and cloned into streaming
//! bodies. When the last clone is dropped (response sent, stream finished, or
//! client disconnected) it records metrics and the access log, and then, off
//! the request path, the request log, token budget, limits and alerts. Doing
//! this on drop means a client that hangs up mid-stream is still billed for
//! what it consumed.

use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;

use crate::{
    app::AppState,
    auth::Tenant,
    limits::Applied,
    logs::{AttemptLog, LogEntry},
    options::{CustomCost, RequestOptions},
};

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's prompt cache (or written to it).
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    /// Actual cost in micro-dollars when the provider reports it
    /// (OpenRouter's `usage.cost`). Preferred over configured prices.
    pub cost_microusd: Option<u64>,
}

fn u64_at(v: &Value, path: &[&str]) -> u64 {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(x) => cur = x,
            None => return 0,
        }
    }
    cur.as_u64().unwrap_or(0)
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
        Some(Self::from_usage_object(u))
    }

    /// Reads any of the usage shapes providers use: OpenAI chat and
    /// embeddings (`prompt_tokens`/`completion_tokens`), the Responses API and
    /// Anthropic (`input_tokens`/`output_tokens`, with cache and reasoning
    /// details).
    pub fn from_usage_object(u: &Value) -> Usage {
        let cost_microusd = u
            .get("cost")
            .and_then(Value::as_f64)
            .filter(|c| c.is_finite() && *c >= 0.0)
            .map(|c| (c * 1_000_000.0).round() as u64);
        let anthropic_cache =
            u64_at(u, &["cache_read_input_tokens"]) + u64_at(u, &["cache_creation_input_tokens"]);
        let prompt = u64_at(u, &["prompt_tokens"]).max(u64_at(u, &["input_tokens"]) + anthropic_cache);
        let completion = u64_at(u, &["completion_tokens"]).max(u64_at(u, &["output_tokens"]));
        let cached = u64_at(u, &["prompt_tokens_details", "cached_tokens"])
            .max(u64_at(u, &["input_tokens_details", "cached_tokens"]))
            .max(anthropic_cache);
        let reasoning = u64_at(u, &["completion_tokens_details", "reasoning_tokens"])
            .max(u64_at(u, &["output_tokens_details", "reasoning_tokens"]));
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached,
            reasoning_tokens: reasoning,
            cost_microusd,
        }
    }

    /// Finds usage anywhere a provider puts it in a response body or event:
    /// `usage`, `response.usage` (Responses API events) or `message.usage`
    /// (Anthropic `message_start`).
    pub fn sniff(v: &Value) -> Option<Usage> {
        for path in [&["usage"][..], &["response", "usage"], &["message", "usage"]] {
            let mut cur = v;
            let mut ok = true;
            for p in path {
                match cur.get(p) {
                    Some(x) => cur = x,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && cur.is_object() {
                return Some(Self::from_usage_object(cur));
            }
        }
        None
    }

    /// Combines usage reported in several stream events (Anthropic reports
    /// input tokens at the start and output tokens at the end).
    pub fn merge(&mut self, other: Usage) {
        self.prompt_tokens = self.prompt_tokens.max(other.prompt_tokens);
        self.completion_tokens = self.completion_tokens.max(other.completion_tokens);
        self.cached_tokens = self.cached_tokens.max(other.cached_tokens);
        self.reasoning_tokens = self.reasoning_tokens.max(other.reasoning_tokens);
        if other.cost_microusd.is_some() {
            self.cost_microusd = other.cost_microusd;
        }
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
    pub endpoint: &'static str,
    pub guardrails: Vec<String>,
    /// Captured only when `logging.capture_bodies` is on.
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub options: Option<Arc<RequestOptions>>,
    pub ttft_ms: Option<u64>,
    pub attempt_log: Vec<AttemptLog>,
    /// `config`, `stored` or `request`.
    pub credential: Option<&'static str>,
    pub limits: Vec<Applied>,
    pub zero_data_retention: bool,
    /// (trace id, parent span id) from a client `traceparent`.
    pub trace: Option<(String, String)>,
}

struct Inner {
    app: Arc<AppState>,
    request_id: String,
    start: Instant,
    start_ms: u64,
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
            start_ms: crate::proxy::unix_millis(),
            state: Mutex::new(RequestState::default()),
            _permit: permit,
        }))
    }

    pub fn request_id(&self) -> &str {
        &self.0.request_id
    }

    pub fn app(&self) -> &Arc<AppState> {
        &self.0.app
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.0.start.elapsed().as_millis() as u64
    }

    pub fn update(&self, f: impl FnOnce(&mut RequestState)) {
        f(&mut self.0.state.lock());
    }

    /// Records time to first byte (first token for streams) once.
    pub fn mark_first_byte(&self) {
        let ms = self.elapsed_ms();
        self.update(|s| {
            if s.ttft_ms.is_none() {
                s.ttft_ms = Some(ms);
            }
        });
    }

    /// Whether prompts and responses are being captured for the log.
    pub fn capturing(&self) -> bool {
        let rt = self.0.app.rt();
        let l = &rt.cfg.logging;
        let st = self.0.state.lock();
        l.enabled
            && l.capture_bodies
            && !st.zero_data_retention
            && st.options.as_ref().is_none_or(|o| o.collect_log)
    }

    fn max_body(&self) -> usize {
        self.0.app.rt().cfg.logging.max_body_bytes
    }

    /// Stores text for the log, truncated to `logging.max_body_bytes`.
    pub fn capture_request(&self, text: &str) {
        if self.capturing() {
            let t = crate::logs::truncate_utf8(text, self.max_body());
            self.update(|s| s.request_body = Some(t));
        }
    }

    pub fn capture_response(&self, text: &str) {
        if self.capturing() {
            let t = crate::logs::truncate_utf8(text, self.max_body());
            self.update(|s| s.response_body = Some(t));
        }
    }

    /// Appends raw text (any stream format), up to the capture limit.
    pub fn capture_append(&self, text: &str) {
        if !self.capturing() {
            return;
        }
        let max = self.max_body();
        let mut st = self.0.state.lock();
        let buf = st.response_body.get_or_insert_with(String::new);
        if buf.len() < max {
            buf.push_str(&crate::logs::truncate_utf8(text, max - buf.len()));
        }
    }

    /// Appends the generated text of one OpenAI-format stream chunk.
    pub fn capture_stream_chunk(&self, payload: &str) {
        if !self.capturing() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else { return };
        let Some(choices) = v.get("choices").and_then(Value::as_array) else { return };
        let mut text = String::new();
        for c in choices {
            let d = &c["delta"];
            if let Some(t) = d.get("content").and_then(Value::as_str) {
                text.push_str(t);
            }
            if let Some(calls) = d.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    if let Some(t) = call["function"].get("arguments").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
        }
        if !text.is_empty() {
            self.capture_append(&text);
        }
    }
}

fn compute_cost(st: &RequestState, billable: bool) -> Option<f64> {
    if !billable {
        return None;
    }
    if let Some(c) = st.options.as_ref().and_then(|o| o.custom_cost) {
        return Some(match c {
            CustomCost::Total(t) => t,
            CustomCost::PerToken { input, output } => {
                st.usage.prompt_tokens as f64 * input + st.usage.completion_tokens as f64 * output
            }
        });
    }
    match (st.usage.cost_microusd, st.price_input_per_mtok, st.price_output_per_mtok) {
        (Some(micro), _, _) => Some(micro as f64 / 1_000_000.0),
        (None, Some(pi), Some(po)) => {
            Some((st.usage.prompt_tokens as f64 * pi + st.usage.completion_tokens as f64 * po) / 1_000_000.0)
        }
        _ => None,
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let st = std::mem::take(&mut *self.state.lock());
        let latency = self.start.elapsed().as_secs_f64();
        let latency_ms = (latency * 1000.0) as u64;
        let tenant = st.tenant.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| "-".into());
        let provider = st.provider.clone().unwrap_or_else(|| "-".into());
        let model = st.model.clone().unwrap_or_else(|| "-".into());
        let cache = if st.cache_hit { "hit" } else { "miss" };
        let billable = !st.cache_hit && st.usage.total() > 0;
        let cost_usd = compute_cost(&st, billable);
        let ok = st.status > 0 && st.status < 400 && st.error.is_none();

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
        if let Some(t) = st.ttft_ms.filter(|_| st.stream) {
            metrics::histogram!("gateway_time_to_first_token_seconds", "provider" => provider.clone())
                .record(t as f64 / 1000.0);
        }
        if billable {
            for (kind, n) in [("prompt", st.usage.prompt_tokens), ("completion", st.usage.completion_tokens)]
            {
                metrics::counter!(
                    "gateway_tokens_total",
                    "tenant" => tenant.clone(), "provider" => provider.clone(),
                    "model" => model.clone(), "type" => kind
                )
                .increment(n);
            }
        }
        if let Some(c) = cost_usd {
            // Recorded in micro-dollars because counters are integers.
            metrics::counter!("gateway_cost_microdollars_total", "tenant" => tenant.clone())
                .increment((c * 1_000_000.0).round() as u64);
        }

        let tps = match (st.stream, st.ttft_ms) {
            (true, Some(t)) if latency_ms > t && st.usage.completion_tokens > 0 => {
                Some(st.usage.completion_tokens as f64 / ((latency_ms - t) as f64 / 1000.0))
            }
            _ => None,
        };
        if !st.cache_hit {
            if let (Some(p), Some(m)) = (&st.provider, &st.model) {
                self.app.stats.record(p, m, ok, st.ttft_ms.or(Some(latency_ms)), tps);
            }
        }

        tracing::info!(
            request_id = %self.request_id,
            tenant = %tenant,
            endpoint = st.endpoint,
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
            latency_ms = latency_ms,
            ttft_ms = st.ttft_ms.unwrap_or(0),
            user = st.options.as_ref().and_then(|o| o.user.as_deref()).unwrap_or(""),
            error = st.error.as_deref().unwrap_or(""),
            "request completed"
        );

        // Everything below needs an authenticated tenant and a runtime.
        let Some(t) = st.tenant.clone() else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let rt = self.app.rt();
        let opts = st.options.clone().unwrap_or_default();
        let entry = rt.cfg.logging.enabled.then(|| LogEntry {
            id: self.request_id.clone(),
            ts_ms: self.start_ms,
            tenant: t.name.clone(),
            endpoint: if st.endpoint.is_empty() { "chat".into() } else { st.endpoint.into() },
            route: st.route.clone(),
            provider: st.provider.clone(),
            model: st.model.clone(),
            status: st.status,
            stream: st.stream,
            cache_hit: st.cache_hit,
            attempts_count: st.attempts,
            prompt_tokens: if st.cache_hit { 0 } else { st.usage.prompt_tokens },
            completion_tokens: if st.cache_hit { 0 } else { st.usage.completion_tokens },
            cached_tokens: if st.cache_hit { 0 } else { st.usage.cached_tokens },
            reasoning_tokens: if st.cache_hit { 0 } else { st.usage.reasoning_tokens },
            cost_usd,
            latency_ms,
            ttft_ms: st.ttft_ms,
            tokens_per_sec: tps,
            error: st.error.clone(),
            guardrails: st.guardrails.clone(),
            user: opts.user.clone(),
            tags: opts.tags.clone(),
            metadata: opts.metadata.clone(),
            event_id: opts.event_id.clone(),
            credential: st.credential.map(str::to_string),
            zero_data_retention: st.zero_data_retention,
            attempts: st.attempt_log.clone(),
            trace_id: st.trace.as_ref().map(|t| t.0.clone()),
            parent_span_id: st.trace.as_ref().map(|t| t.1.clone()),
            feedback: None,
            request_body: if st.zero_data_retention { None } else { st.request_body.clone() },
            response_body: if st.zero_data_retention { None } else { st.response_body.clone() },
        });
        let collect = opts.collect_log;
        let app = self.app.clone();
        let request_id = self.request_id.clone();
        let tokens = if billable { st.usage.total() } else { 0 };
        let cost_micro = cost_usd.map(|c| (c * 1_000_000.0).round() as u64).unwrap_or(0);
        let limits = st.limits.clone();
        // Counted so shutdown can wait for these writes instead of dropping
        // usage that was already consumed.
        app.pending_usage_writes.fetch_add(1, Ordering::SeqCst);
        handle.spawn(async move {
            if tokens > 0 {
                let key = crate::proxy::budget_key(&t.name);
                record_usage(&app, &key, &request_id, tokens, &t.name).await;
            }
            if !limits.is_empty() && (tokens > 0 || cost_micro > 0) {
                let changes =
                    crate::limits::charge(&app.store, &limits, &request_id, tokens, cost_micro).await;
                crate::alerts::limit_crossings(&app, &changes).await;
            }
            if let Some(entry) = entry {
                crate::logs::record(&app, entry, collect).await;
            }
            app.pending_usage_writes.fetch_sub(1, Ordering::SeqCst);
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_every_usage_shape() {
        let openai = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5,
            "prompt_tokens_details": {"cached_tokens": 4}, "completion_tokens_details": {"reasoning_tokens": 2}}});
        let u = Usage::sniff(&openai).unwrap();
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.cached_tokens, u.reasoning_tokens),
            (10, 5, 4, 2)
        );

        let responses = json!({"type": "response.completed", "response": {"usage": {"input_tokens": 7, "output_tokens": 3,
            "input_tokens_details": {"cached_tokens": 1}, "output_tokens_details": {"reasoning_tokens": 1}}}});
        let u = Usage::sniff(&responses).unwrap();
        assert_eq!((u.prompt_tokens, u.completion_tokens, u.cached_tokens), (7, 3, 1));

        let anthropic_start = json!({"type": "message_start", "message": {"usage": {"input_tokens": 12,
            "cache_read_input_tokens": 100, "output_tokens": 1}}});
        let mut u = Usage::sniff(&anthropic_start).unwrap();
        assert_eq!((u.prompt_tokens, u.cached_tokens), (112, 100));
        u.merge(Usage::sniff(&json!({"type": "message_delta", "usage": {"output_tokens": 40}})).unwrap());
        assert_eq!(u.completion_tokens, 40);
        assert_eq!(u.prompt_tokens, 112);
        assert!(Usage::sniff(&json!({"choices": []})).is_none());
    }
}
