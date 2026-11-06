//! Request, token and spend limits.
//!
//! Each limit counts one kind of thing (requests, tokens or dollars) per
//! calendar window (minute, hour, UTC day, ISO week, month), split by a
//! dimension: everything (`global`), API key, user, requested model, provider
//! or a metadata field. Requests are counted when they arrive; tokens and
//! cost are added when the request finishes, so a burst of concurrent
//! requests can overshoot a token or cost limit by what is in flight (the
//! same soft-cap behaviour as Cloudflare and Vercel).
//!
//! `per: provider` limits act on targets: a provider over its limit is
//! skipped and the next fallback is tried.

use std::collections::BTreeMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    config::{LimitConfig, LimitKind, LimitWindow},
    store::{Store, StoreError},
};

/// Everything a limit can be split or filtered by.
pub struct Dims<'a> {
    pub tenant: &'a str,
    pub user: Option<&'a str>,
    pub model: &'a str,
    pub metadata: &'a BTreeMap<String, String>,
}

/// A limit bucket a request was counted against.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub name: String,
    pub kind: LimitKind,
    pub window: LimitWindow,
    /// The dimension value this bucket is for (`*` for global).
    pub scope: String,
    #[serde(skip)]
    pub bucket: String,
    #[serde(skip)]
    pub marker_prefix: String,
    /// Limit in counter units (requests, tokens or micro-dollars).
    pub limit_units: u64,
    #[serde(skip)]
    pub ttl: u64,
}

/// A limit that rejected a request.
#[derive(Debug)]
pub struct Exceeded {
    pub name: String,
    pub retry_after: u64,
}

pub enum CheckError {
    Store(StoreError),
    Exceeded(Exceeded),
}

impl From<StoreError> for CheckError {
    fn from(e: StoreError) -> Self {
        CheckError::Store(e)
    }
}

fn units(kind: LimitKind, v: f64) -> u64 {
    match kind {
        LimitKind::Cost => (v * 1_000_000.0).round() as u64,
        _ => v.round().max(1.0) as u64,
    }
}

/// Days since 1970-01-01 → (year, month 1-12). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m)
}

fn days_from_civil(y: i64, m: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (window id, window start, window end) in unix seconds.
pub fn window_bounds(w: LimitWindow, now: u64) -> (u64, u64, u64) {
    match w {
        LimitWindow::Minute => (now / 60, now / 60 * 60, now / 60 * 60 + 60),
        LimitWindow::Hour => (now / 3600, now / 3600 * 3600, now / 3600 * 3600 + 3600),
        LimitWindow::Day => (now / 86_400, now / 86_400 * 86_400, now / 86_400 * 86_400 + 86_400),
        LimitWindow::Week => {
            // ISO weeks start on Monday; 1970-01-01 was a Thursday.
            let days = now / 86_400;
            let id = (days + 3) / 7;
            let start = (id * 7 - 3) * 86_400;
            (id, start, start + 7 * 86_400)
        }
        LimitWindow::Month => {
            let (y, m) = civil_from_days((now / 86_400) as i64);
            let start = days_from_civil(y, m) as u64 * 86_400;
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            let end = days_from_civil(ny, nm) as u64 * 86_400;
            ((y * 12 + i64::from(m)) as u64, start, end)
        }
    }
}

fn model_matches(patterns: &[String], model: &str) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|p| {
            p == "*"
                || p == model
                || p.strip_suffix("/*")
                    .is_some_and(|prefix| model.split_once('/').is_some_and(|(pp, _)| pp == prefix))
        })
}

fn dimension_value<'a>(l: &LimitConfig, d: &'a Dims<'_>, provider: Option<&'a str>) -> Option<String> {
    match l.per.as_str() {
        "global" => Some("*".into()),
        "key" => Some(d.tenant.to_string()),
        "user" => d.user.map(str::to_string),
        "model" => Some(d.model.to_string()),
        "provider" => provider.map(str::to_string),
        f => f.strip_prefix("metadata.").and_then(|k| d.metadata.get(k)).cloned(),
    }
}

fn applied(l: &LimitConfig, scope: String, now: u64) -> Applied {
    let (win, _, end) = window_bounds(l.window, now);
    let h = hex::encode(&Sha256::digest(scope.as_bytes())[..8]);
    // One hash tag per limit+scope keeps the counter and its idempotency
    // markers in the same Redis Cluster slot.
    let tag = format!("{{{}:{h}}}", l.name);
    Applied {
        name: l.name.clone(),
        kind: l.kind,
        window: l.window,
        scope,
        bucket: format!("lim:{tag}:{win}"),
        marker_prefix: format!("limdone:{tag}:{win}"),
        limit_units: units(l.kind, l.limit),
        ttl: (end.saturating_sub(now)).max(60) + 86_400,
    }
}

/// Limits that apply to a request, excluding `per: provider` limits.
fn applicable<'a>(
    limits: &'a [LimitConfig],
    d: &Dims<'_>,
    provider: Option<&str>,
) -> Vec<(&'a LimitConfig, String)> {
    limits
        .iter()
        .filter(|l| (l.per == "provider") == provider.is_some())
        .filter(|l| l.keys.is_empty() || l.keys.iter().any(|k| k == d.tenant))
        .filter(|l| {
            let served = provider.map(|p| format!("{p}/{}", d.model));
            model_matches(&l.models, d.model)
                || served.as_deref().is_some_and(|s| model_matches(&l.models, s))
        })
        .filter_map(|l| dimension_value(l, d, provider).map(|v| (l, v)))
        .collect()
}

/// Checks and counts a request against every applicable limit. Returns the
/// buckets to charge tokens and cost to when it finishes.
pub async fn check(
    store: &Store,
    limits: &[LimitConfig],
    d: &Dims<'_>,
    provider: Option<&str>,
    now: u64,
) -> Result<Vec<Applied>, CheckError> {
    let mut out = Vec::new();
    for (l, scope) in applicable(limits, d, provider) {
        let a = applied(l, scope, now);
        let (_, start, end) = window_bounds(l.window, now);
        let retry_after = end.saturating_sub(now).max(1);
        match l.kind {
            LimitKind::Requests => {
                let n = store.incr(&a.bucket, 1, a.ttl).await?;
                let mut effective = n as f64;
                if l.sliding {
                    let (prev_id, prev_start, _) = window_bounds(l.window, start.saturating_sub(1));
                    let prev_bucket = a.bucket.rsplit_once(':').map(|(head, _)| format!("{head}:{prev_id}"));
                    if let Some(pb) = prev_bucket {
                        let prev = store.get_counter(&pb).await? as f64;
                        let len = (start - prev_start).max(1) as f64;
                        let elapsed = (now - start) as f64 / len;
                        effective += prev * (1.0 - elapsed).max(0.0);
                    }
                }
                if effective > a.limit_units as f64 {
                    return Err(CheckError::Exceeded(Exceeded { name: l.name.clone(), retry_after }));
                }
            }
            LimitKind::Tokens | LimitKind::Cost => {
                if store.get_counter(&a.bucket).await? >= a.limit_units {
                    return Err(CheckError::Exceeded(Exceeded { name: l.name.clone(), retry_after }));
                }
            }
        }
        out.push(a);
    }
    Ok(out)
}

/// Adds a finished request's tokens and cost to its buckets (at most once
/// per request id). Returns (limit, value before, value after) for alerts.
pub async fn charge(
    store: &Store,
    applied: &[Applied],
    request_id: &str,
    tokens: u64,
    cost_micro: u64,
) -> Vec<(Applied, u64, u64)> {
    let mut out = Vec::new();
    for a in applied {
        let delta = match a.kind {
            LimitKind::Requests => continue,
            LimitKind::Tokens => tokens,
            LimitKind::Cost => cost_micro,
        };
        if delta == 0 {
            continue;
        }
        let marker = format!("{}:{request_id}", a.marker_prefix);
        let res = store.incr_once(&a.bucket, &marker, delta, a.ttl).await;
        match res {
            Ok(()) => {
                if let Ok(after) = store.get_counter(&a.bucket).await {
                    out.push((a.clone(), after.saturating_sub(delta), after));
                }
            }
            Err(e) => {
                metrics::counter!("gateway_store_errors_total", "op" => "limit_charge").increment(1);
                tracing::warn!(error = %e, limit = %a.name, "failed to charge limit");
            }
        }
    }
    out
}

/// Current usage of each global and per-key limit, for the dashboard.
pub async fn snapshot(
    store: &Store,
    limits: &[LimitConfig],
    tenants: &[String],
    now: u64,
) -> Vec<serde_json::Value> {
    let mut rows = Vec::new();
    for l in limits {
        let scopes: Vec<String> = match l.per.as_str() {
            "global" => vec!["*".into()],
            "key" => tenants.iter().filter(|t| l.keys.is_empty() || l.keys.contains(t)).cloned().collect(),
            _ => Vec::new(),
        };
        let (_, _, end) = window_bounds(l.window, now);
        if scopes.is_empty() {
            rows.push(serde_json::json!({
                "name": l.name, "kind": l.kind, "window": l.window, "per": l.per, "limit": l.limit,
                "scope": null, "used": null, "resets_at": end,
            }));
        }
        for scope in scopes {
            let a = applied(l, scope.clone(), now);
            let used = store.get_counter(&a.bucket).await.ok();
            let used_value =
                used.map(|u| if l.kind == LimitKind::Cost { u as f64 / 1_000_000.0 } else { u as f64 });
            rows.push(serde_json::json!({
                "name": l.name, "kind": l.kind, "window": l.window, "per": l.per, "limit": l.limit,
                "scope": scope, "used": used_value, "resets_at": end,
            }));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FailMode, StoreBackend, StoreConfig};

    #[test]
    fn calendar_windows() {
        // 2026-11-06T12:00:00Z (a Friday)
        let now = 1_793_966_400;
        let (_, s, e) = window_bounds(LimitWindow::Day, now);
        assert_eq!((s, e), (1_793_923_200, 1_794_009_600));
        let (_, s, e) = window_bounds(LimitWindow::Week, now);
        assert_eq!(s, 1_793_577_600, "Monday 2026-11-02");
        assert_eq!(e - s, 7 * 86_400);
        let (_, s, e) = window_bounds(LimitWindow::Month, now);
        assert_eq!(s, 1_793_491_200, "2026-11-01");
        assert_eq!(e, 1_796_083_200, "2026-12-01");
        let (_, s, e) = window_bounds(LimitWindow::Month, 1_798_761_600 + 5); // 2027-01-01
        assert_eq!(s, 1_798_761_600);
        assert_eq!(e, 1_798_761_600 + 31 * 86_400);
    }

    fn lim(name: &str, kind: LimitKind, limit: f64, per: &str) -> LimitConfig {
        LimitConfig {
            name: name.into(),
            kind,
            limit,
            window: LimitWindow::Day,
            per: per.into(),
            keys: vec![],
            models: vec![],
            sliding: false,
        }
    }

    async fn mem() -> Store {
        Store::connect(&StoreConfig {
            backend: StoreBackend::Memory,
            redis_url: None,
            key_prefix: "t:".into(),
            op_timeout_ms: 100,
            on_error: FailMode::Closed,
            memory_max_cache_entries: 10,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn counts_requests_and_charges_cost_per_user() {
        let store = mem().await;
        let limits =
            vec![lim("rpd", LimitKind::Requests, 2.0, "global"), lim("spend", LimitKind::Cost, 0.01, "user")];
        let md = BTreeMap::new();
        let d = Dims { tenant: "acme", user: Some("u1"), model: "m", metadata: &md };
        let now = 1_793_966_400;
        let a = check(&store, &limits, &d, None, now).await.ok().unwrap();
        assert_eq!(a.len(), 2);
        assert!(check(&store, &limits, &d, None, now).await.is_ok());
        assert!(matches!(check(&store, &limits, &d, None, now).await, Err(CheckError::Exceeded(_))));

        let only_cost = vec![lim("spend", LimitKind::Cost, 0.01, "user")];
        let a = check(&store, &only_cost, &d, None, now).await.ok().unwrap();
        let changes = charge(&store, &a, "r1", 10, 12_000).await;
        assert_eq!(changes[0].2, 12_000);
        // Charging the same request twice counts once.
        charge(&store, &a, "r1", 10, 12_000).await;
        assert!(matches!(check(&store, &only_cost, &d, None, now).await, Err(CheckError::Exceeded(_))));
        // Another user has their own budget; no user means the limit does not apply.
        let d2 = Dims { tenant: "acme", user: Some("u2"), model: "m", metadata: &md };
        assert!(check(&store, &only_cost, &d2, None, now).await.is_ok());
        let d3 = Dims { tenant: "acme", user: None, model: "m", metadata: &md };
        assert!(check(&store, &only_cost, &d3, None, now).await.ok().unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_limits_only_apply_per_target() {
        let store = mem().await;
        let limits = vec![lim("prov", LimitKind::Requests, 1.0, "provider")];
        let md = BTreeMap::new();
        let d = Dims { tenant: "acme", user: None, model: "m", metadata: &md };
        assert!(check(&store, &limits, &d, None, 0).await.ok().unwrap().is_empty());
        assert!(check(&store, &limits, &d, Some("a"), 0).await.is_ok());
        assert!(check(&store, &limits, &d, Some("a"), 0).await.is_err());
        assert!(check(&store, &limits, &d, Some("b"), 0).await.is_ok());
    }
}
