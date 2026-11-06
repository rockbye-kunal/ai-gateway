//! Per-request gateway options, from headers and from the request body's
//! `providerOptions.gateway` object (the same shape Vercel AI Gateway uses).
//!
//! Headers are accepted as `x-gateway-<name>` and, for drop-in compatibility,
//! as Cloudflare's `cf-aig-<name>`:
//!
//! | header | meaning |
//! |---|---|
//! | `metadata` | JSON object of string/number/bool values attached to the log |
//! | `cache-ttl` | seconds to cache this response |
//! | `skip-cache` | `true` to bypass the cache |
//! | `cache-key` | custom cache key |
//! | `collect-log` | `false` to keep this request out of the request log |
//! | `request-timeout` | milliseconds for the whole request |
//! | `max-attempts` | attempts per target (1-10) |
//! | `retry-delay` | milliseconds before the first retry |
//! | `backoff` | `constant`, `linear` or `exponential` |
//! | `custom-cost` | `{"per_token_in": .., "per_token_out": ..}` or `{"total_cost": ..}` |
//! | `event-id` | your own id for the request, stored in the log |
//!
//! Also: `x-session-id`, `ai-reporting-user`, `ai-reporting-tags`.

use std::{collections::BTreeMap, time::Duration};

use axum::http::HeaderMap;
use serde_json::{Map, Value};

use crate::error::ApiError;

pub const MAX_METADATA_KEYS: usize = 20;
pub const MAX_TAGS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    Constant,
    Linear,
    Exponential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Cost,
    Latency,
    Throughput,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CustomCost {
    PerToken { input: f64, output: f64 },
    Total(f64),
}

#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub metadata: BTreeMap<String, String>,
    pub user: Option<String>,
    pub tags: Vec<String>,
    pub session_id: Option<String>,
    pub event_id: Option<String>,
    /// Providers to try first, in this order.
    pub order: Vec<String>,
    /// Only these providers may serve the request.
    pub only: Vec<String>,
    /// Fallback models, tried after the requested model.
    pub models: Vec<String>,
    pub sort: Option<Sort>,
    /// Request-scoped provider keys (provider name → key).
    pub byok: BTreeMap<String, String>,
    pub zero_data_retention: bool,
    pub disallow_prompt_training: bool,
    pub auto_cache: bool,
    pub cache_ttl: Option<u64>,
    pub skip_cache: bool,
    pub cache_key: Option<String>,
    pub collect_log: bool,
    pub request_timeout: Option<Duration>,
    pub max_attempts: Option<u32>,
    pub retry_delay: Option<Duration>,
    pub backoff: Option<Backoff>,
    pub custom_cost: Option<CustomCost>,
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(format!("x-gateway-{name}"))
        .or_else(|| h.get(format!("cf-aig-{name}")))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn parse_bool(name: &str, v: &str) -> Result<bool, ApiError> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(ApiError::bad_request(format!("header {name} must be true or false"))),
    }
}

fn parse_u64(name: &str, v: &str, max: u64) -> Result<u64, ApiError> {
    v.parse::<u64>()
        .ok()
        .filter(|n| *n <= max)
        .ok_or_else(|| ApiError::bad_request(format!("header {name} must be a whole number up to {max}")))
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn add_metadata(out: &mut BTreeMap<String, String>, obj: &Map<String, Value>) -> Result<(), ApiError> {
    for (k, v) in obj {
        if k.is_empty() || k.len() > 64 {
            return Err(ApiError::bad_request("metadata keys must be 1-64 characters"));
        }
        let Some(v) = scalar(v) else {
            return Err(ApiError::bad_request(format!("metadata '{k}' must be a string, number or boolean")));
        };
        if v.len() > 256 {
            return Err(ApiError::bad_request(format!("metadata '{k}' is longer than 256 characters")));
        }
        out.insert(k.clone(), v);
    }
    if out.len() > MAX_METADATA_KEYS {
        return Err(ApiError::bad_request(format!("at most {MAX_METADATA_KEYS} metadata keys are allowed")));
    }
    Ok(())
}

fn string_list(v: Option<&Value>, field: &str) -> Result<Vec<String>, ApiError> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str().map(str::to_string).ok_or_else(|| {
                    ApiError::bad_request(format!("providerOptions.gateway.{field} must list strings"))
                })
            })
            .collect(),
        Some(_) => Err(ApiError::bad_request(format!("providerOptions.gateway.{field} must be an array"))),
    }
}

fn push_tags(tags: &mut Vec<String>, new: impl IntoIterator<Item = String>) -> Result<(), ApiError> {
    for t in new {
        let t = t.trim().to_string();
        if t.is_empty() {
            continue;
        }
        if t.len() > 64 {
            return Err(ApiError::bad_request("tags must be 1-64 characters"));
        }
        if !tags.contains(&t) {
            tags.push(t);
        }
    }
    if tags.len() > MAX_TAGS {
        return Err(ApiError::bad_request(format!("at most {MAX_TAGS} tags are allowed")));
    }
    Ok(())
}

impl RequestOptions {
    /// Reads options from headers and from `body` (when it is a JSON object),
    /// removing `providerOptions` from the body so it never reaches a provider.
    pub fn parse(headers: &HeaderMap, body: Option<&mut Value>) -> Result<Self, ApiError> {
        let mut o = RequestOptions { collect_log: true, ..Default::default() };

        if let Some(raw) = header(headers, "metadata") {
            let v: Value = serde_json::from_str(raw)
                .map_err(|_| ApiError::bad_request("metadata header must be a JSON object"))?;
            let obj = v
                .as_object()
                .ok_or_else(|| ApiError::bad_request("metadata header must be a JSON object"))?;
            add_metadata(&mut o.metadata, obj)?;
        }
        if let Some(v) = header(headers, "cache-ttl") {
            o.cache_ttl = Some(parse_u64("cache-ttl", v, 30 * 86_400)?.max(1));
        }
        if let Some(v) = header(headers, "skip-cache") {
            o.skip_cache = parse_bool("skip-cache", v)?;
        }
        // Older header kept working: x-gateway-cache: skip
        if headers
            .get("x-gateway-cache")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("skip"))
        {
            o.skip_cache = true;
        }
        if let Some(v) = header(headers, "cache-key") {
            if v.len() > 512 {
                return Err(ApiError::bad_request("cache-key must be at most 512 characters"));
            }
            o.cache_key = Some(v.to_string());
        }
        if let Some(v) = header(headers, "collect-log") {
            o.collect_log = parse_bool("collect-log", v)?;
        }
        if let Some(v) = header(headers, "request-timeout") {
            o.request_timeout =
                Some(Duration::from_millis(parse_u64("request-timeout", v, 3_600_000)?.max(1)));
        }
        if let Some(v) = header(headers, "max-attempts") {
            let n = parse_u64("max-attempts", v, 10)?;
            if n == 0 {
                return Err(ApiError::bad_request("max-attempts must be between 1 and 10"));
            }
            o.max_attempts = Some(n as u32);
        }
        if let Some(v) = header(headers, "retry-delay") {
            o.retry_delay = Some(Duration::from_millis(parse_u64("retry-delay", v, 60_000)?));
        }
        if let Some(v) = header(headers, "backoff") {
            o.backoff = Some(match v.to_ascii_lowercase().as_str() {
                "constant" => Backoff::Constant,
                "linear" => Backoff::Linear,
                "exponential" => Backoff::Exponential,
                _ => return Err(ApiError::bad_request("backoff must be constant, linear or exponential")),
            });
        }
        if let Some(v) = header(headers, "custom-cost") {
            o.custom_cost = Some(parse_custom_cost(v)?);
        }
        if let Some(v) = header(headers, "event-id") {
            if v.len() > 128 {
                return Err(ApiError::bad_request("event-id must be at most 128 characters"));
            }
            o.event_id = Some(v.to_string());
        }
        if let Some(v) = headers.get("x-session-id").and_then(|v| v.to_str().ok()).map(str::trim) {
            if !v.is_empty() && v.len() <= 256 {
                o.session_id = Some(v.to_string());
            }
        }

        if let Some(obj) = body.and_then(Value::as_object_mut) {
            // OpenAI's standard `user` field doubles as the reporting user.
            if let Some(u) = obj.get("user").and_then(Value::as_str) {
                o.user = Some(u.to_string());
            }
            if let Some(po) = obj.remove("providerOptions") {
                if let Some(g) = po.get("gateway") {
                    o.apply_gateway(g)?;
                }
            }
        }

        // Headers win over the body for user; tags are merged.
        if let Some(u) = headers.get("ai-reporting-user").and_then(|v| v.to_str().ok()).map(str::trim) {
            if !u.is_empty() {
                o.user = Some(u.to_string());
            }
        }
        if let Some(t) = headers.get("ai-reporting-tags").and_then(|v| v.to_str().ok()) {
            let mut tags = std::mem::take(&mut o.tags);
            push_tags(&mut tags, t.split(',').map(str::to_string))?;
            o.tags = tags;
        }
        if o.user.as_ref().is_some_and(|u| u.len() > 256) {
            return Err(ApiError::bad_request("user must be at most 256 characters"));
        }
        Ok(o)
    }

    fn apply_gateway(&mut self, g: &Value) -> Result<(), ApiError> {
        let Some(g) = g.as_object() else {
            return Err(ApiError::bad_request("providerOptions.gateway must be an object"));
        };
        self.order = string_list(g.get("order"), "order")?;
        self.only = string_list(g.get("only"), "only")?;
        self.models = string_list(g.get("models"), "models")?;
        if self.models.len() > 10 {
            return Err(ApiError::bad_request("providerOptions.gateway.models allows at most 10 fallbacks"));
        }
        if let Some(u) = g.get("user").and_then(Value::as_str) {
            self.user = Some(u.to_string());
        }
        let tags = string_list(g.get("tags"), "tags")?;
        push_tags(&mut self.tags, tags)?;
        if let Some(m) = g.get("metadata") {
            let obj = m
                .as_object()
                .ok_or_else(|| ApiError::bad_request("providerOptions.gateway.metadata must be an object"))?;
            add_metadata(&mut self.metadata, obj)?;
        }
        if let Some(s) = g.get("sort").and_then(Value::as_str) {
            self.sort = Some(match s {
                "cost" => Sort::Cost,
                "latency" | "ttft" => Sort::Latency,
                "throughput" | "tps" => Sort::Throughput,
                _ => {
                    return Err(ApiError::bad_request(
                        "providerOptions.gateway.sort must be cost, ttft or tps",
                    ))
                }
            });
        }
        if let Some(s) = g.get("sessionId").and_then(Value::as_str) {
            self.session_id = Some(s.chars().take(256).collect());
        }
        if g.get("sessionAffinity").and_then(Value::as_bool) == Some(false) {
            self.session_id = None;
        }
        if let Some(c) = g.get("caching").and_then(Value::as_str) {
            self.auto_cache = c == "auto";
        }
        if g.get("zeroDataRetention").and_then(Value::as_bool) == Some(true) {
            self.zero_data_retention = true;
        }
        if g.get("disallowPromptTraining").and_then(Value::as_bool) == Some(true) {
            self.disallow_prompt_training = true;
        }
        if let Some(b) = g.get("byok") {
            let obj = b
                .as_object()
                .ok_or_else(|| ApiError::bad_request("providerOptions.gateway.byok must be an object"))?;
            for (provider, creds) in obj {
                // Accept [{ "apiKey": "..." }], { "apiKey": "..." } or "...".
                let key = match creds {
                    Value::String(s) => Some(s.as_str()),
                    Value::Array(a) => a.first().and_then(|c| c.get("apiKey")).and_then(Value::as_str),
                    Value::Object(_) => creds.get("apiKey").and_then(Value::as_str),
                    _ => None,
                };
                let Some(key) = key.filter(|k| !k.is_empty() && k.len() <= 1024) else {
                    return Err(ApiError::bad_request(format!(
                        "providerOptions.gateway.byok.{provider} must contain an apiKey"
                    )));
                };
                self.byok.insert(provider.clone(), key.to_string());
            }
        }
        Ok(())
    }
}

fn parse_custom_cost(v: &str) -> Result<CustomCost, ApiError> {
    let bad = || {
        ApiError::bad_request(
            "custom-cost must be {\"per_token_in\": n, \"per_token_out\": n} or {\"total_cost\": n}",
        )
    };
    let j: Value = serde_json::from_str(v).map_err(|_| bad())?;
    let num = |k: &str| j.get(k).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0);
    if let Some(t) = num("total_cost") {
        return Ok(CustomCost::Total(t));
    }
    match (num("per_token_in"), num("per_token_out")) {
        (None, None) => Err(bad()),
        (i, o) => Ok(CustomCost::PerToken { input: i.unwrap_or(0.0), output: o.unwrap_or(0.0) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn reads_headers_in_both_namings() {
        let o = RequestOptions::parse(
            &h(&[
                ("cf-aig-metadata", r#"{"plan":"free","team":7,"beta":true}"#),
                ("x-gateway-cache-ttl", "120"),
                ("cf-aig-skip-cache", "true"),
                ("x-gateway-collect-log", "false"),
                ("cf-aig-max-attempts", "3"),
                ("cf-aig-backoff", "linear"),
                ("x-gateway-custom-cost", r#"{"per_token_in":0.000001,"per_token_out":0.000002}"#),
                ("x-session-id", "s-1"),
                ("ai-reporting-tags", "team:a, feature:chat"),
                ("ai-reporting-user", "u-9"),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(o.metadata["plan"], "free");
        assert_eq!(o.metadata["team"], "7");
        assert_eq!(o.cache_ttl, Some(120));
        assert!(o.skip_cache && !o.collect_log);
        assert_eq!(o.max_attempts, Some(3));
        assert_eq!(o.backoff, Some(Backoff::Linear));
        assert!(matches!(o.custom_cost, Some(CustomCost::PerToken { .. })));
        assert_eq!(o.session_id.as_deref(), Some("s-1"));
        assert_eq!(o.tags, vec!["team:a", "feature:chat"]);
        assert_eq!(o.user.as_deref(), Some("u-9"));
    }

    #[test]
    fn reads_and_strips_provider_options() {
        let mut body = json!({
            "model": "m", "user": "body-user",
            "providerOptions": {"gateway": {
                "order": ["a", "b"], "only": ["a"], "models": ["x/y"], "tags": ["t1"],
                "byok": {"openai": [{"apiKey": "sk-1"}]}, "zeroDataRetention": true, "sort": "ttft",
                "caching": "auto", "metadata": {"k": "v"}
            }, "anthropic": {"thinking": {}}}
        });
        let o = RequestOptions::parse(&HeaderMap::new(), Some(&mut body)).unwrap();
        assert!(body.get("providerOptions").is_none());
        assert_eq!(o.order, vec!["a", "b"]);
        assert_eq!(o.only, vec!["a"]);
        assert_eq!(o.models, vec!["x/y"]);
        assert_eq!(o.byok["openai"], "sk-1");
        assert!(o.zero_data_retention && o.auto_cache);
        assert_eq!(o.sort, Some(Sort::Latency));
        assert_eq!(o.user.as_deref(), Some("body-user"));
        assert_eq!(o.metadata["k"], "v");
    }

    #[test]
    fn rejects_bad_values() {
        assert!(RequestOptions::parse(&h(&[("x-gateway-metadata", "[1]")]), None).is_err());
        assert!(RequestOptions::parse(&h(&[("x-gateway-max-attempts", "0")]), None).is_err());
        assert!(RequestOptions::parse(&h(&[("x-gateway-backoff", "random")]), None).is_err());
        assert!(RequestOptions::parse(&h(&[("cf-aig-cache-ttl", "-1")]), None).is_err());
        let mut body = json!({"providerOptions": {"gateway": {"byok": {"openai": [{}]}}}});
        assert!(RequestOptions::parse(&HeaderMap::new(), Some(&mut body)).is_err());
    }
}
