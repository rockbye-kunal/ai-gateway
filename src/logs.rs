//! Request logs and analytics.
//!
//! Every finished request (except unauthenticated ones, which would let
//! anyone flush the log) is written as:
//! - a summary at the front of a capped list, `logs`, shared by all replicas;
//! - a full detail record, `logdetail:<id>`, including prompt and response
//!   when `logging.capture_bodies` is on, kept `logging.detail_ttl_secs`;
//! - counters in per-minute (`stats:m:<minute>`, kept 2 days) and per-hour
//!   (`stats:h:<hour>`, kept 35 days) hashes, which the dashboard charts.
//!
//! Writes happen after the response, off the request path, so a slow or
//! unavailable store never delays clients.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app::AppState;

const LOG_LIST: &str = "logs";
const MINUTE_TTL_SECS: u64 = 2 * 86_400;
const HOUR_TTL_SECS: u64 = 35 * 86_400;
/// Up to this many minutes are charted at minute resolution; longer ranges use hours.
const MAX_MINUTE_BUCKETS: u64 = 6 * 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    /// Unix time in milliseconds when the request arrived.
    pub ts_ms: u64,
    pub tenant: String,
    /// `chat` or `embeddings`.
    pub endpoint: String,
    /// What the client sent as `model` (a route name or a model).
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub status: u16,
    pub stream: bool,
    pub cache_hit: bool,
    pub attempts: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    pub latency_ms: u64,
    #[serde(default)]
    pub error: Option<String>,
    /// Guardrail rules that matched this request or its response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guardrails: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
}

impl LogEntry {
    pub fn is_error(&self) -> bool {
        self.status >= 400 || self.status == 0
    }

    fn summary(&self) -> LogEntry {
        LogEntry { request_body: None, response_body: None, ..self.clone() }
    }
}

/// Cuts `s` to at most `max` bytes on a character boundary.
pub fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

/// Writes one finished request. Failures are counted and logged, never fatal.
pub async fn record(app: &AppState, entry: LogEntry) {
    let cfg = &app.cfg.logging;
    let summary = entry.summary();
    let writes = async {
        let raw = serde_json::to_string(&summary).unwrap_or_default();
        app.store.list_push_capped(LOG_LIST, &raw, cfg.max_entries).await?;
        let full = serde_json::to_string(&entry).unwrap_or_default();
        app.store.kv_set(&format!("logdetail:{}", entry.id), &full, cfg.detail_ttl_secs).await?;

        let fields = stat_fields(&entry);
        let secs = entry.ts_ms / 1000;
        app.store.hincr_many(&format!("stats:m:{}", secs / 60), &fields, MINUTE_TTL_SECS).await?;
        app.store.hincr_many(&format!("stats:h:{}", secs / 3600), &fields, HOUR_TTL_SECS).await?;
        Ok::<(), crate::store::StoreError>(())
    };
    if let Err(e) = writes.await {
        metrics::counter!("gateway_store_errors_total", "op" => "request_log").increment(1);
        tracing::warn!(error = %e, request_id = %entry.id, "failed to write request log");
    }
}

/// Counter fields for one request. Breakdown fields are `<kind>:<name>`.
fn stat_fields(e: &LogEntry) -> Vec<(String, i64)> {
    let tokens = (e.prompt_tokens + e.completion_tokens) as i64;
    let cost_micro = e.cost_usd.map(|c| (c * 1_000_000.0).round() as i64).unwrap_or(0);
    let err = i64::from(e.is_error());
    let mut f = vec![
        ("req".to_string(), 1),
        ("err".to_string(), err),
        ("cache".to_string(), i64::from(e.cache_hit)),
        ("ptok".to_string(), e.prompt_tokens as i64),
        ("ctok".to_string(), e.completion_tokens as i64),
        ("cost".to_string(), cost_micro),
        ("lat".to_string(), e.latency_ms as i64),
        (format!("s:{}", e.status), 1),
        (format!("t:{}", e.tenant), 1),
        (format!("tt:{}", e.tenant), tokens),
        (format!("tc:{}", e.tenant), cost_micro),
    ];
    if !e.guardrails.is_empty() {
        f.push(("guard".to_string(), 1));
    }
    if let Some(m) = &e.model {
        f.push((format!("m:{m}"), 1));
        f.push((format!("mt:{m}"), tokens));
        f.push((format!("mc:{m}"), cost_micro));
        f.push((format!("me:{m}"), err));
    }
    if let Some(p) = &e.provider {
        f.push((format!("p:{p}"), 1));
        f.push((format!("pe:{p}"), err));
        f.push((format!("pl:{p}"), e.latency_ms as i64));
    }
    f.retain(|(_, v)| *v != 0);
    f
}

/// Filters for the log list.
#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub tenant: Option<String>,
    pub provider: Option<String>,
    /// Substring of the model or route.
    pub model: Option<String>,
    /// `ok`, `error`, or an exact status code.
    pub status: Option<String>,
    /// Free text: request id, error, model, route, tenant or guardrail.
    pub q: Option<String>,
    pub since_ms: Option<u64>,
}

impl LogQuery {
    fn matches(&self, e: &LogEntry) -> bool {
        let nonempty =
            |o: &Option<String>| o.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        if let Some(t) = nonempty(&self.tenant) {
            if e.tenant != t {
                return false;
            }
        }
        if let Some(p) = nonempty(&self.provider) {
            if e.provider.as_deref() != Some(p.as_str()) {
                return false;
            }
        }
        if let Some(m) = nonempty(&self.model) {
            let m = m.to_lowercase();
            let hit = [&e.model, &e.route]
                .iter()
                .any(|v| v.as_deref().is_some_and(|x| x.to_lowercase().contains(&m)));
            if !hit {
                return false;
            }
        }
        if let Some(s) = nonempty(&self.status) {
            let ok = match s.as_str() {
                "ok" => !e.is_error(),
                "error" => e.is_error(),
                code => code.parse::<u16>().is_ok_and(|c| c == e.status),
            };
            if !ok {
                return false;
            }
        }
        if let Some(since) = self.since_ms {
            if e.ts_ms < since {
                return false;
            }
        }
        if let Some(q) = nonempty(&self.q) {
            let q = q.to_lowercase();
            let fields = [
                Some(e.id.as_str()),
                Some(e.tenant.as_str()),
                e.error.as_deref(),
                e.model.as_deref(),
                e.route.as_deref(),
                e.provider.as_deref(),
            ];
            let hit = fields.iter().flatten().any(|f| f.to_lowercase().contains(&q))
                || e.guardrails.iter().any(|g| g.to_lowercase().contains(&q));
            if !hit {
                return false;
            }
        }
        true
    }
}

async fn recent(app: &AppState) -> Result<Vec<LogEntry>, crate::store::StoreError> {
    let raw = app.store.list_range(LOG_LIST, app.cfg.logging.max_entries).await?;
    Ok(raw.iter().filter_map(|r| serde_json::from_str(r).ok()).collect())
}

/// Matching log summaries, newest first, and the total number that matched.
pub async fn list(app: &AppState, q: &LogQuery) -> Result<(Vec<LogEntry>, usize), crate::store::StoreError> {
    let all = recent(app).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0);
    let matched: Vec<LogEntry> = all.into_iter().filter(|e| q.matches(e)).collect();
    let total = matched.len();
    Ok((matched.into_iter().skip(offset).take(limit).collect(), total))
}

pub async fn get(app: &AppState, id: &str) -> Result<Option<LogEntry>, crate::store::StoreError> {
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Ok(None);
    }
    if let Some(raw) = app.store.kv_get(&format!("logdetail:{id}")).await? {
        return Ok(serde_json::from_str(&raw).ok());
    }
    // Detail expired: fall back to the summary if it is still in the list.
    Ok(recent(app).await?.into_iter().find(|e| e.id == id))
}

/// Dashboard overview for the last `range_secs` seconds: totals, a time
/// series, breakdowns by model / provider / key / status, and latency
/// percentiles from the recent log.
pub async fn overview(
    app: &AppState,
    range_secs: u64,
    now_secs: u64,
) -> Result<Value, crate::store::StoreError> {
    let range_secs = range_secs.clamp(60, 35 * 86_400);
    let (step, prefix) = if range_secs / 60 <= MAX_MINUTE_BUCKETS { (60, "m") } else { (3600, "h") };
    let last = now_secs / step;
    let first = now_secs.saturating_sub(range_secs) / step + 1;
    let buckets: Vec<u64> = (first..=last).collect();
    let keys: Vec<String> = buckets.iter().map(|b| format!("stats:{prefix}:{b}")).collect();
    let maps = app.store.hgetall_many(&keys).await?;

    let mut totals: HashMap<&'static str, i64> = HashMap::new();
    let mut groups: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut series = Vec::with_capacity(buckets.len());
    for (b, m) in buckets.iter().zip(&maps) {
        let g = |k: &str| m.get(k).copied().unwrap_or(0);
        let req = g("req");
        series.push(json!({
            "t": b * step,
            "req": req,
            "err": g("err"),
            "cache": g("cache"),
            "tokens": g("ptok") + g("ctok"),
            "cost_usd": g("cost") as f64 / 1_000_000.0,
            "avg_latency_ms": if req > 0 { g("lat") / req } else { 0 },
        }));
        for k in ["req", "err", "cache", "ptok", "ctok", "cost", "lat", "guard"] {
            *totals.entry(k).or_insert(0) += g(k);
        }
        for (field, v) in m {
            if let Some((kind, name)) = field.split_once(':') {
                *groups.entry(kind.to_string()).or_default().entry(name.to_string()).or_insert(0) += v;
            }
        }
    }

    let group = |count: &str| groups.get(count).cloned().unwrap_or_default();
    let breakdown = |count: &str, extras: &[(&str, &str)]| -> Vec<Value> {
        let counts = group(count);
        let extra_maps: Vec<(String, HashMap<String, i64>)> =
            extras.iter().map(|(label, kind)| (label.to_string(), group(kind))).collect();
        let mut rows: Vec<(String, i64)> = counts.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        rows.into_iter()
            .take(25)
            .map(|(name, n)| {
                let mut row = json!({"name": name, "requests": n});
                for (label, m) in &extra_maps {
                    let v = m.get(&name).copied().unwrap_or(0);
                    row[label.as_str()] =
                        if label == "cost_usd" { json!(v as f64 / 1_000_000.0) } else { json!(v) };
                }
                row
            })
            .collect()
    };

    let total = |k: &str| totals.get(k).copied().unwrap_or(0);
    let req = total("req");
    let since_ms = now_secs.saturating_sub(range_secs) * 1000;
    let mut latencies: Vec<u64> = recent(app)
        .await?
        .into_iter()
        .filter(|e| e.ts_ms >= since_ms && !e.cache_hit && !e.is_error())
        .map(|e| e.latency_ms)
        .collect();
    latencies.sort_unstable();
    let pct = |p: f64| -> Value {
        if latencies.is_empty() {
            return Value::Null;
        }
        let idx = ((latencies.len() as f64 - 1.0) * p).round() as usize;
        json!(latencies[idx.min(latencies.len() - 1)])
    };

    Ok(json!({
        "range_secs": range_secs,
        "step_secs": step,
        "totals": {
            "requests": req,
            "errors": total("err"),
            "error_rate": if req > 0 { total("err") as f64 / req as f64 } else { 0.0 },
            "cache_hits": total("cache"),
            "cache_hit_rate": if req > 0 { total("cache") as f64 / req as f64 } else { 0.0 },
            "prompt_tokens": total("ptok"),
            "completion_tokens": total("ctok"),
            "cost_usd": total("cost") as f64 / 1_000_000.0,
            "avg_latency_ms": if req > 0 { total("lat") / req } else { 0 },
            "p50_latency_ms": pct(0.50),
            "p95_latency_ms": pct(0.95),
            "p99_latency_ms": pct(0.99),
            "guardrail_hits": total("guard"),
        },
        "series": series,
        "models": breakdown("m", &[("tokens", "mt"), ("cost_usd", "mc"), ("errors", "me")]),
        "providers": breakdown("p", &[("errors", "pe"), ("latency_sum_ms", "pl")]),
        "keys": breakdown("t", &[("tokens", "tt"), ("cost_usd", "tc")]),
        "statuses": breakdown("s", &[]),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
        let t = truncate_utf8("héllo", 2);
        assert!(t.starts_with('h') && t.ends_with("[truncated]"));
    }

    #[test]
    fn stat_fields_skip_zeroes() {
        let e = LogEntry {
            tenant: "acme".into(),
            status: 200,
            model: Some("gpt".into()),
            provider: Some("openai".into()),
            prompt_tokens: 3,
            completion_tokens: 4,
            latency_ms: 10,
            ..Default::default()
        };
        let f: HashMap<String, i64> = stat_fields(&e).into_iter().collect();
        assert_eq!(f["req"], 1);
        assert_eq!(f["mt:gpt"], 7);
        assert_eq!(f["p:openai"], 1);
        assert!(!f.contains_key("err"));
        assert!(!f.contains_key("cost"));
    }

    #[test]
    fn query_filters() {
        let e = LogEntry {
            id: "abc-123".into(),
            tenant: "acme".into(),
            status: 502,
            model: Some("openai/gpt-4o".into()),
            error: Some("upstream timed out".into()),
            ..Default::default()
        };
        let q = |s: &str| -> LogQuery { serde_json::from_str(s).unwrap() };
        assert!(q(r#"{}"#).matches(&e));
        assert!(q(r#"{"status":"error"}"#).matches(&e));
        assert!(!q(r#"{"status":"ok"}"#).matches(&e));
        assert!(q(r#"{"status":"502"}"#).matches(&e));
        assert!(q(r#"{"model":"GPT-4"}"#).matches(&e));
        assert!(q(r#"{"q":"timed"}"#).matches(&e));
        assert!(!q(r#"{"tenant":"other"}"#).matches(&e));
        assert!(q(r#"{"tenant":"  "}"#).matches(&e));
    }
}
