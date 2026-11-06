//! Request logs, analytics and usage reports.
//!
//! Every finished request (except unauthenticated ones, which would let
//! anyone flush the log, and ones sent with `collect-log: false`) is written:
//! - as a summary at the front of a capped list, `logs`, shared by replicas;
//! - as a full record, `logdetail:<id>`, kept `logging.detail_ttl_secs`;
//! - into per-minute (`stats:m:<minute>`, 2 days) and per-hour
//!   (`stats:h:<hour>`, 35 days) counters for the dashboard charts;
//! - into hourly usage-report counters (`rep:<hour>`), split by key, user,
//!   model, provider, tags, credential type and ZDR, for the report API.
//!
//! Writes happen after the response, off the request path.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{app::AppState, store::StoreError};

const LOG_LIST: &str = "logs";
const MINUTE_TTL_SECS: u64 = 2 * 86_400;
const HOUR_TTL_SECS: u64 = 35 * 86_400;
const MAX_MINUTE_BUCKETS: u64 = 6 * 60;
const SEP: char = '\u{1f}';
const METRIC_SEP: char = '\u{1e}';

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AttemptLog {
    pub provider: String,
    pub model: String,
    /// HTTP status from the provider, or the gateway's error status (0 when
    /// the provider could not be reached at all).
    pub status: u16,
    /// Milliseconds after the request arrived that this attempt started.
    pub start_ms: u64,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Feedback {
    /// 1 (thumbs up), -1 (thumbs down) or 0.
    #[serde(default)]
    pub rating: i8,
    /// Optional 0-100 score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub ts: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    /// Unix time in milliseconds when the request arrived.
    pub ts_ms: u64,
    pub tenant: String,
    /// `chat`, `embeddings`, `responses`, `messages`, `images`,
    /// `moderations`, `rerank`, `speech`, `native` or `realtime`.
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
    pub attempts_count: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    pub latency_ms: u64,
    /// Time to first byte of the response body (first token for streams).
    #[serde(default)]
    pub ttft_ms: Option<u64>,
    /// Output tokens per second after the first token (streams only).
    #[serde(default)]
    pub tokens_per_sec: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guardrails: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// `config`, `stored` or `request` (BYOK) for the key used upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default)]
    pub zero_data_retention: bool,
    /// Every upstream attempt, in order, including failed ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<AttemptLog>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<Feedback>,
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

fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(256).collect()
}

/// Writes one finished request. Failures are counted and logged, never fatal.
pub async fn record(app: &AppState, entry: LogEntry, collect: bool) {
    let rt = app.rt();
    let cfg = &rt.cfg.logging;
    let writes = async {
        if collect {
            let raw = serde_json::to_string(&entry.summary()).unwrap_or_default();
            app.store.list_push_capped(LOG_LIST, &raw, cfg.max_entries).await?;
            let full = serde_json::to_string(&entry).unwrap_or_default();
            app.store.kv_set(&format!("logdetail:{}", entry.id), &full, cfg.detail_ttl_secs).await?;
        }
        let fields = stat_fields(&entry);
        let secs = entry.ts_ms / 1000;
        app.store.hincr_many(&format!("stats:m:{}", secs / 60), &fields, MINUTE_TTL_SECS).await?;
        app.store.hincr_many(&format!("stats:h:{}", secs / 3600), &fields, HOUR_TTL_SECS).await?;
        let report = report_fields(&entry);
        app.store
            .hincr_many(&format!("rep:{}", secs / 3600), &report, cfg.report_retention_days * 86_400 + 3600)
            .await?;
        Ok::<(), StoreError>(())
    };
    if let Err(e) = writes.await {
        metrics::counter!("gateway_store_errors_total", "op" => "request_log").increment(1);
        tracing::warn!(error = %e, request_id = %entry.id, "failed to write request log");
    }
    if collect {
        app.exporter.send(&entry);
    }
}

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
        ("cached".to_string(), e.cached_tokens as i64),
        ("cost".to_string(), cost_micro),
        ("lat".to_string(), e.latency_ms as i64),
        (format!("s:{}", e.status), 1),
        (format!("t:{}", e.tenant), 1),
        (format!("tt:{}", e.tenant), tokens),
        (format!("tc:{}", e.tenant), cost_micro),
        (format!("e:{}", e.endpoint), 1),
    ];
    if let Some(t) = e.ttft_ms {
        f.push(("ttft".to_string(), t as i64));
        f.push(("ttftn".to_string(), 1));
    }
    if !e.guardrails.is_empty() {
        f.push(("guard".to_string(), 1));
    }
    if let Some(m) = &e.model {
        f.push((format!("m:{m}"), 1));
        f.push((format!("mt:{m}"), tokens));
        f.push((format!("mc:{m}"), cost_micro));
        f.push((format!("me:{m}"), err));
        f.push((format!("ml:{m}"), e.latency_ms as i64));
    }
    if let Some(p) = &e.provider {
        f.push((format!("p:{p}"), 1));
        f.push((format!("pe:{p}"), err));
        f.push((format!("pl:{p}"), e.latency_ms as i64));
    }
    if let Some(u) = &e.user {
        let u = clean(u);
        f.push((format!("u:{u}"), 1));
        f.push((format!("ut:{u}"), tokens));
        f.push((format!("uc:{u}"), cost_micro));
    }
    for t in &e.tags {
        let t = clean(t);
        f.push((format!("g:{t}"), 1));
        f.push((format!("gc:{t}"), cost_micro));
    }
    f.retain(|(_, v)| *v != 0);
    f
}

fn report_fields(e: &LogEntry) -> Vec<(String, i64)> {
    let mut tags: Vec<String> = e.tags.iter().map(|t| clean(t).replace(',', ";")).collect();
    tags.sort();
    let dims = [
        format!("k={}", clean(&e.tenant)),
        format!("u={}", e.user.as_deref().map(clean).unwrap_or_default()),
        format!("m={}", e.model.as_deref().map(clean).unwrap_or_default()),
        format!("p={}", e.provider.as_deref().map(clean).unwrap_or_default()),
        format!("t={}", tags.join(",")),
        format!("c={}", e.credential.as_deref().unwrap_or("config")),
        format!("z={}", e.zero_data_retention),
    ]
    .join(&SEP.to_string());
    let cost = e.cost_usd.map(|c| (c * 1_000_000.0).round() as i64).unwrap_or(0);
    [
        ("req", 1),
        ("err", i64::from(e.is_error())),
        ("in", e.prompt_tokens as i64),
        ("out", e.completion_tokens as i64),
        ("cached", e.cached_tokens as i64),
        ("reason", e.reasoning_tokens as i64),
        ("cost", cost),
    ]
    .into_iter()
    .filter(|(_, v)| *v != 0)
    .map(|(k, v)| (format!("{dims}{METRIC_SEP}{k}"), v))
    .collect()
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
    /// `ok`, `error`, `cached`, `flagged`, or an exact status code.
    pub status: Option<String>,
    pub user: Option<String>,
    pub tag: Option<String>,
    pub event_id: Option<String>,
    pub endpoint: Option<String>,
    /// `metadata.<key>=<value>` pairs, comma separated.
    pub metadata: Option<String>,
    /// Free text: request id, error, model, route, key, user or guardrail.
    pub q: Option<String>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
}

fn nonempty(o: &Option<String>) -> Option<String> {
    o.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

impl LogQuery {
    pub fn is_empty(&self) -> bool {
        [
            &self.tenant,
            &self.provider,
            &self.model,
            &self.status,
            &self.user,
            &self.tag,
            &self.event_id,
            &self.endpoint,
            &self.metadata,
            &self.q,
        ]
        .iter()
        .all(|f| nonempty(f).is_none())
            && self.since_ms.is_none()
            && self.until_ms.is_none()
    }

    pub fn matches(&self, e: &LogEntry) -> bool {
        if nonempty(&self.tenant).is_some_and(|t| e.tenant != t) {
            return false;
        }
        if nonempty(&self.provider).is_some_and(|p| e.provider.as_deref() != Some(p.as_str())) {
            return false;
        }
        if nonempty(&self.endpoint).is_some_and(|p| e.endpoint != p) {
            return false;
        }
        if nonempty(&self.user).is_some_and(|u| e.user.as_deref() != Some(u.as_str())) {
            return false;
        }
        if nonempty(&self.tag).is_some_and(|t| !e.tags.contains(&t)) {
            return false;
        }
        if nonempty(&self.event_id).is_some_and(|t| e.event_id.as_deref() != Some(t.as_str())) {
            return false;
        }
        if let Some(m) = nonempty(&self.model) {
            let m = m.to_lowercase();
            if ![&e.model, &e.route]
                .iter()
                .any(|v| v.as_deref().is_some_and(|x| x.to_lowercase().contains(&m)))
            {
                return false;
            }
        }
        if let Some(s) = nonempty(&self.status) {
            let ok = match s.as_str() {
                "ok" => !e.is_error(),
                "error" => e.is_error(),
                "cached" => e.cache_hit,
                "flagged" => !e.guardrails.is_empty(),
                code => code.parse::<u16>().is_ok_and(|c| c == e.status),
            };
            if !ok {
                return false;
            }
        }
        if let Some(md) = nonempty(&self.metadata) {
            for pair in md.split(',') {
                let Some((k, v)) = pair.split_once('=') else { continue };
                let k = k.trim().trim_start_matches("metadata.");
                if e.metadata.get(k).map(String::as_str) != Some(v.trim()) {
                    return false;
                }
            }
        }
        if self.since_ms.is_some_and(|s| e.ts_ms < s) || self.until_ms.is_some_and(|u| e.ts_ms > u) {
            return false;
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
                e.user.as_deref(),
                e.event_id.as_deref(),
            ];
            let hit = fields.iter().flatten().any(|f| f.to_lowercase().contains(&q))
                || e.guardrails.iter().chain(&e.tags).any(|g| g.to_lowercase().contains(&q))
                || e.metadata.values().any(|v| v.to_lowercase().contains(&q));
            if !hit {
                return false;
            }
        }
        true
    }
}

async fn recent(app: &AppState) -> Result<Vec<LogEntry>, StoreError> {
    let raw = app.store.list_range(LOG_LIST, app.rt().cfg.logging.max_entries).await?;
    Ok(raw.iter().filter_map(|r| serde_json::from_str(r).ok()).collect())
}

async fn attach_feedback(app: &AppState, entries: &mut [LogEntry]) -> Result<(), StoreError> {
    if entries.is_empty() {
        return Ok(());
    }
    let keys: Vec<String> = entries.iter().map(|e| format!("feedback:{}", e.id)).collect();
    let values = app.store.kv_get_many(&keys).await?;
    for (e, v) in entries.iter_mut().zip(values) {
        if let Some(fb) = v.and_then(|v| serde_json::from_str(&v).ok()) {
            e.feedback = Some(fb);
        }
    }
    Ok(())
}

/// Matching log summaries, newest first, and the total number that matched.
pub async fn list(app: &AppState, q: &LogQuery) -> Result<(Vec<LogEntry>, usize), StoreError> {
    let all = recent(app).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0);
    let matched: Vec<LogEntry> = all.into_iter().filter(|e| q.matches(e)).collect();
    let total = matched.len();
    let mut page: Vec<LogEntry> = matched.into_iter().skip(offset).take(limit).collect();
    attach_feedback(app, &mut page).await?;
    Ok((page, total))
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

pub async fn get(app: &AppState, id: &str) -> Result<Option<LogEntry>, StoreError> {
    if !valid_id(id) {
        return Ok(None);
    }
    let mut found = match app.store.kv_get(&format!("logdetail:{id}")).await? {
        Some(raw) => serde_json::from_str::<LogEntry>(&raw).ok(),
        // Detail expired: fall back to the summary if it is still in the list.
        None => recent(app).await?.into_iter().find(|e| e.id == id),
    };
    if let Some(e) = found.as_mut() {
        attach_feedback(app, std::slice::from_mut(e)).await?;
    }
    Ok(found)
}

/// Stores feedback for a request and counts it in analytics.
pub async fn set_feedback(app: &AppState, entry: &LogEntry, fb: &Feedback) -> Result<(), StoreError> {
    let ttl = app.rt().cfg.logging.detail_ttl_secs.max(30 * 86_400);
    app.store
        .kv_set(&format!("feedback:{}", entry.id), &serde_json::to_string(fb).unwrap_or_default(), ttl)
        .await?;
    let secs = entry.ts_ms / 1000;
    let mut fields = vec![("fb".to_string(), 1)];
    if let Some(m) = &entry.model {
        let key = match fb.rating.signum() {
            1 => "fbu",
            -1 => "fbd",
            _ => "fbn",
        };
        fields.push((format!("{key}:{m}"), 1));
        if let Some(s) = fb.score {
            fields.push((format!("fbs:{m}"), i64::from(s)));
            fields.push((format!("fbsn:{m}"), 1));
        }
    }
    app.store.hincr_many(&format!("stats:h:{}", secs / 3600), &fields, HOUR_TTL_SECS).await?;
    app.store.hincr_many(&format!("stats:m:{}", secs / 60), &fields, MINUTE_TTL_SECS).await?;
    Ok(())
}

/// Deletes matching entries from the searchable log (all of them when the
/// query has no filters). Returns how many were removed.
pub async fn delete(app: &AppState, q: &LogQuery) -> Result<usize, StoreError> {
    let all = recent(app).await?;
    let (gone, keep): (Vec<LogEntry>, Vec<LogEntry>) =
        if q.is_empty() { (all, Vec::new()) } else { all.into_iter().partition(|e| q.matches(e)) };
    let kept: Vec<String> = keep.iter().filter_map(|e| serde_json::to_string(e).ok()).collect();
    app.store.list_replace(LOG_LIST, &kept).await?;
    for e in &gone {
        app.store.del(&format!("logdetail:{}", e.id)).await?;
    }
    Ok(gone.len())
}

/// Dashboard overview for the last `range_secs` seconds.
pub async fn overview(
    app: &AppState,
    range_secs: u64,
    now_secs: u64,
    tenant: Option<&str>,
) -> Result<Value, StoreError> {
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
            "avg_ttft_ms": if g("ttftn") > 0 { g("ttft") / g("ttftn") } else { 0 },
        }));
        for k in
            ["req", "err", "cache", "ptok", "ctok", "cached", "cost", "lat", "guard", "ttft", "ttftn", "fb"]
        {
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
    let recent_ok: Vec<LogEntry> = recent(app)
        .await?
        .into_iter()
        .filter(|e| e.ts_ms >= since_ms && !e.cache_hit && !e.is_error())
        .filter(|e| tenant.is_none_or(|t| e.tenant == t))
        .collect();
    let mut latencies: Vec<u64> = recent_ok.iter().map(|e| e.latency_ms).collect();
    latencies.sort_unstable();
    let mut ttfts: Vec<u64> = recent_ok.iter().filter_map(|e| e.ttft_ms).collect();
    ttfts.sort_unstable();
    let pct = |v: &[u64], p: f64| -> Value {
        if v.is_empty() {
            return Value::Null;
        }
        let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
        json!(v[idx.min(v.len() - 1)])
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
            "cached_tokens": total("cached"),
            "cost_usd": total("cost") as f64 / 1_000_000.0,
            "avg_latency_ms": if req > 0 { total("lat") / req } else { 0 },
            "p50_latency_ms": pct(&latencies, 0.50),
            "p95_latency_ms": pct(&latencies, 0.95),
            "p99_latency_ms": pct(&latencies, 0.99),
            "avg_ttft_ms": if total("ttftn") > 0 { total("ttft") / total("ttftn") } else { 0 },
            "p50_ttft_ms": pct(&ttfts, 0.50),
            "p95_ttft_ms": pct(&ttfts, 0.95),
            "guardrail_hits": total("guard"),
            "feedback": total("fb"),
        },
        "series": series,
        "models": breakdown("m", &[
            ("tokens", "mt"), ("cost_usd", "mc"), ("errors", "me"), ("latency_sum_ms", "ml"),
            ("thumbs_up", "fbu"), ("thumbs_down", "fbd"), ("score_sum", "fbs"), ("score_n", "fbsn"),
        ]),
        "providers": breakdown("p", &[("errors", "pe"), ("latency_sum_ms", "pl")]),
        "keys": breakdown("t", &[("tokens", "tt"), ("cost_usd", "tc")]),
        "users": breakdown("u", &[("tokens", "ut"), ("cost_usd", "uc")]),
        "tags": breakdown("g", &[("cost_usd", "gc")]),
        "endpoints": breakdown("e", &[]),
        "statuses": breakdown("s", &[]),
    }))
}

/// Query for the usage report (same shape as Vercel's custom reporting).
#[derive(Debug, Default, Deserialize)]
pub struct ReportQuery {
    pub start_date: String,
    pub end_date: String,
    /// `day` (default), `hour`, `model`, `provider`, `user`, `tag`, `key`,
    /// `credential_type`, `zero_data_retention`.
    pub group_by: Option<String>,
    pub date_part: Option<String>,
    pub key: Option<String>,
    pub user_id: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub credential_type: Option<String>,
    pub zero_data_retention: Option<String>,
    pub tags: Option<String>,
    pub tags_match: Option<String>,
}

fn parse_date(s: &str) -> Option<u64> {
    let mut p = s.trim().split('-');
    let y: i64 = p.next()?.parse().ok()?;
    let m: u32 = p.next()?.parse().ok()?;
    let d: i64 = p.next()?.parse().ok()?;
    if p.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from civil (see limits.rs), plus the day of month.
    let (y2, m2) = if m <= 2 { (y - 1, i64::from(m) + 9) } else { (y, i64::from(m) - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days >= 0).then_some(days as u64 * 86_400)
}

fn date_string(secs: u64, hour: bool) -> String {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    if hour {
        format!("{y:04}-{m:02}-{d:02}T{:02}", (secs % 86_400) / 3600)
    } else {
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// Usage and spend grouped by one dimension, from hourly report counters.
/// `force_key` restricts the report to one API key (client-facing report).
pub async fn report(
    app: &AppState,
    q: &ReportQuery,
    force_key: Option<&str>,
) -> Result<Result<Value, String>, StoreError> {
    let (Some(start), Some(end)) = (parse_date(&q.start_date), parse_date(&q.end_date)) else {
        return Ok(Err("start_date and end_date must be YYYY-MM-DD".into()));
    };
    if end < start {
        return Ok(Err("end_date is before start_date".into()));
    }
    let days = (end - start) / 86_400 + 1;
    if days > 400 {
        return Ok(Err("a report can cover at most 400 days".into()));
    }
    let group_by = q.group_by.as_deref().unwrap_or("day");
    let valid = [
        "day",
        "model",
        "provider",
        "user",
        "tag",
        "key",
        "api_key_name",
        "credential_type",
        "zero_data_retention",
    ];
    if !valid.contains(&group_by) {
        return Ok(Err(format!("group_by must be one of {}", valid.join(", "))));
    }
    let hourly = q.date_part.as_deref() == Some("hour");
    let hours: Vec<u64> = (start / 3600..(end + 86_400) / 3600).collect();
    let keys: Vec<String> = hours.iter().map(|h| format!("rep:{h}")).collect();
    let mut maps = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(500) {
        maps.extend(app.store.hgetall_many(chunk).await?);
    }

    let want_tags: Vec<String> = q
        .tags
        .as_deref()
        .map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let tags_all = q.tags_match.as_deref() == Some("all");
    let key_filter = force_key.map(str::to_string).or_else(|| nonempty(&q.key));

    let mut rows: BTreeMap<String, HashMap<&'static str, i64>> = BTreeMap::new();
    for (hour, m) in hours.iter().zip(&maps) {
        for (field, v) in m {
            let Some((dims, metric)) = field.split_once(METRIC_SEP) else { continue };
            let dims: HashMap<&str, &str> = dims.split(SEP).filter_map(|kv| kv.split_once('=')).collect();
            let get = |k: &str| dims.get(k).copied().unwrap_or("");
            let tags: Vec<&str> = get("t").split(',').filter(|s| !s.is_empty()).collect();
            if key_filter.as_deref().is_some_and(|k| get("k") != k)
                || nonempty(&q.user_id).is_some_and(|u| get("u") != u)
                || nonempty(&q.model).is_some_and(|x| get("m") != x)
                || nonempty(&q.provider).is_some_and(|x| get("p") != x)
                || nonempty(&q.credential_type).is_some_and(|x| get("c") != x)
                || nonempty(&q.zero_data_retention).is_some_and(|x| get("z") != x)
            {
                continue;
            }
            if !want_tags.is_empty() {
                let has = |t: &String| tags.contains(&t.as_str());
                let ok = if tags_all { want_tags.iter().all(has) } else { want_tags.iter().any(has) };
                if !ok {
                    continue;
                }
            }
            let metric: &'static str = match metric {
                "req" => "request_count",
                "err" => "error_count",
                "in" => "input_tokens",
                "out" => "output_tokens",
                "cached" => "cached_input_tokens",
                "reason" => "reasoning_tokens",
                "cost" => "cost",
                _ => continue,
            };
            let groups: Vec<String> = match group_by {
                "day" => vec![date_string(hour * 3600, hourly)],
                "model" => vec![get("m").to_string()],
                "provider" => vec![get("p").to_string()],
                "user" => vec![get("u").to_string()],
                "key" | "api_key_name" => vec![get("k").to_string()],
                "credential_type" => vec![get("c").to_string()],
                "zero_data_retention" => vec![get("z").to_string()],
                _ => tags.iter().map(|t| t.to_string()).collect(),
            };
            for g in groups {
                *rows.entry(g).or_default().entry(metric).or_insert(0) += v;
            }
        }
    }
    let label = match group_by {
        "day" if hourly => "hour",
        "key" => "api_key_name",
        other => other,
    };
    let results: Vec<Value> = rows
        .into_iter()
        .map(|(g, m)| {
            let n = |k: &str| m.get(k).copied().unwrap_or(0);
            json!({
                label: g,
                "total_cost": n("cost") as f64 / 1_000_000.0,
                "input_tokens": n("input_tokens"),
                "output_tokens": n("output_tokens"),
                "cached_input_tokens": n("cached_input_tokens"),
                "reasoning_tokens": n("reasoning_tokens"),
                "request_count": n("request_count"),
                "error_count": n("error_count"),
            })
        })
        .collect();
    Ok(Ok(json!({ "results": results })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(parse_date("2026-11-06"), Some(1_793_923_200));
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("nope"), None);
        assert_eq!(date_string(1_793_923_200, false), "2026-11-06");
        assert_eq!(date_string(1_793_923_200 + 5 * 3600, true), "2026-11-06T05");
        assert_eq!(date_string(951_782_400, false), "2000-02-29");
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
        let t = truncate_utf8("héllo", 2);
        assert!(t.starts_with('h') && t.ends_with("[truncated]"));
    }

    #[test]
    fn stat_and_report_fields() {
        let e = LogEntry {
            tenant: "acme".into(),
            status: 200,
            model: Some("gpt".into()),
            provider: Some("openai".into()),
            user: Some("u1".into()),
            tags: vec!["b".into(), "a".into()],
            prompt_tokens: 3,
            completion_tokens: 4,
            latency_ms: 10,
            ..Default::default()
        };
        let f: HashMap<String, i64> = stat_fields(&e).into_iter().collect();
        assert_eq!(f["req"], 1);
        assert_eq!(f["mt:gpt"], 7);
        assert_eq!(f["u:u1"], 1);
        assert_eq!(f["g:a"], 1);
        assert!(!f.contains_key("err"));
        let r = report_fields(&e);
        assert!(r.iter().any(|(k, v)| k.ends_with("\u{1e}in") && *v == 3));
        assert!(r[0].0.contains("t=a,b"));
    }

    #[test]
    fn query_filters() {
        let mut e = LogEntry {
            id: "abc-123".into(),
            tenant: "acme".into(),
            status: 502,
            model: Some("openai/gpt-4o".into()),
            error: Some("upstream timed out".into()),
            user: Some("u1".into()),
            tags: vec!["prod".into()],
            ..Default::default()
        };
        e.metadata.insert("plan".into(), "free".into());
        let q = |s: &str| -> LogQuery { serde_json::from_str(s).unwrap() };
        assert!(q(r#"{}"#).matches(&e));
        assert!(q(r#"{}"#).is_empty());
        assert!(q(r#"{"status":"error"}"#).matches(&e));
        assert!(!q(r#"{"status":"ok"}"#).matches(&e));
        assert!(q(r#"{"status":"502"}"#).matches(&e));
        assert!(q(r#"{"model":"GPT-4"}"#).matches(&e));
        assert!(q(r#"{"q":"timed"}"#).matches(&e));
        assert!(q(r#"{"q":"free"}"#).matches(&e));
        assert!(!q(r#"{"tenant":"other"}"#).matches(&e));
        assert!(q(r#"{"tenant":"  "}"#).matches(&e));
        assert!(q(r#"{"user":"u1","tag":"prod","metadata":"plan=free"}"#).matches(&e));
        assert!(!q(r#"{"metadata":"plan=pro"}"#).matches(&e));
    }
}
