//! Admin API and dashboard, served on `server.admin_listen` only.
//!
//! Every `/api/*` call needs `Authorization: Bearer <admin token>`, whose
//! SHA-256 is `admin.token_sha256`. Without that setting the API answers 404
//! and only the dashboard shell, metrics and health checks are served.

use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    app::AppState,
    auth::{generate_key, hash_key, StoredKey, STORED_KEYS},
    config::{valid_ident, valid_model, ProviderKind, RouteStrategy},
    error::ApiError,
    logs::{self, LogQuery},
    proxy::{budget_key, unix_secs},
    store::StoreError,
};

const DASHBOARD_HTML: &str = include_str!("../dashboard/index.html");
const DASHBOARD_JS: &str = include_str!("../dashboard/app.js");

fn authorize(app: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = &app.cfg.admin.token_sha256 else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "admin_disabled",
            "the admin API is disabled; set admin.token_sha256 in the gateway config",
        ));
    };
    let token =
        crate::auth::extract_key(headers).ok_or_else(|| ApiError::unauthorized("missing admin token"))?;
    if hash_key(token) != *expected {
        metrics::counter!("gateway_rejected_total", "reason" => "admin_auth").increment(1);
        return Err(ApiError::unauthorized("invalid admin token"));
    }
    Ok(())
}

fn store_unavailable(e: StoreError) -> ApiError {
    metrics::counter!("gateway_store_errors_total", "op" => "admin").increment(1);
    tracing::error!(error = %e, "admin API: store unavailable");
    ApiError::unavailable("the gateway's store is unavailable, retry shortly")
}

fn with_security_headers(mut r: Response, content_type: &'static str) -> Response {
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; \
             frame-ancestors 'none'",
        ),
    );
    r
}

pub async fn dashboard() -> Response {
    with_security_headers(DASHBOARD_HTML.into_response(), "text/html; charset=utf-8")
}

pub async fn dashboard_js() -> Response {
    with_security_headers(DASHBOARD_JS.into_response(), "text/javascript; charset=utf-8")
}

#[derive(Deserialize)]
pub struct RangeQuery {
    range: Option<String>,
}

/// `15m`, `1h`, `6h`, `24h`, `7d`, `30d`, or any `<n>m|h|d`.
fn parse_range(s: Option<&str>) -> Result<u64, ApiError> {
    let s = s.unwrap_or("24h").trim();
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num.parse().map_err(|_| ApiError::bad_request("range must look like 15m, 24h or 7d"))?;
    let secs = match unit {
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return Err(ApiError::bad_request("range must look like 15m, 24h or 7d")),
    };
    if !(60..=35 * 86_400).contains(&secs) {
        return Err(ApiError::bad_request("range must be between 1m and 35d"));
    }
    Ok(secs)
}

pub async fn overview(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<RangeQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let range = parse_range(q.range.as_deref())?;
    let v = logs::overview(&app, range, unix_secs()).await.map_err(store_unavailable)?;
    Ok(Json(v))
}

pub async fn list_logs(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let (data, total) = logs::list(&app, &q).await.map_err(store_unavailable)?;
    Ok(Json(json!({ "data": data, "total": total, "capture_bodies": app.cfg.logging.capture_bodies })))
}

pub async fn get_log(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    match logs::get(&app, &id).await.map_err(store_unavailable)? {
        Some(e) => Ok(Json(json!(e))),
        None => {
            Err(ApiError::new(StatusCode::NOT_FOUND, "invalid_request_error", "not_found", "no such request"))
        }
    }
}

/// Reloads keys created through the admin API from the store.
pub async fn refresh_keys(app: &AppState) -> Result<(), StoreError> {
    let seen = app.keys.generation();
    let raw = app.store.hgetall(STORED_KEYS).await?;
    let mut records = HashMap::with_capacity(raw.len());
    for (hash, value) in raw {
        if !crate::config::is_sha256_hex(&hash) {
            continue;
        }
        match serde_json::from_str::<StoredKey>(&value) {
            Ok(rec) => {
                records.insert(hash, rec);
            }
            Err(e) => tracing::warn!(error = %e, "ignoring unreadable stored API key"),
        }
    }
    app.keys.replace_dynamic_if_unchanged(seen, records);
    Ok(())
}

pub async fn list_keys(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let tenants = app.keys.tenants();
    let store = &app.store;
    let usage = futures::future::join_all(
        tenants.iter().map(|(t, _)| async move { store.get_counter(&budget_key(&t.name)).await.ok() }),
    )
    .await;
    let data: Vec<Value> = tenants
        .iter()
        .zip(usage)
        .map(|((t, rec), used)| {
            json!({
                "name": t.name,
                "source": t.source,
                "key_hint": rec.as_ref().map(|r| r.key_hint.clone()),
                "created_at": rec.as_ref().map(|r| r.created_at),
                "requests_per_minute": t.requests_per_minute,
                "daily_token_budget": t.daily_token_budget,
                "max_concurrent_requests": t.max_concurrent_requests,
                "allowed_models": t.allowed_models(),
                "tokens_used_today": used,
            })
        })
        .collect();
    Ok(Json(json!({ "data": data })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateKey {
    name: String,
    #[serde(default)]
    requests_per_minute: Option<u64>,
    #[serde(default)]
    daily_token_budget: Option<u64>,
    #[serde(default)]
    max_concurrent_requests: Option<usize>,
    allowed_models: Vec<String>,
}

fn valid_allowed_entry(p: &str) -> bool {
    p == "*" || valid_model(p.strip_suffix("/*").unwrap_or(p))
}

impl CreateKey {
    fn validate(&self) -> Result<(), ApiError> {
        if !valid_ident(&self.name) {
            return Err(ApiError::bad_request(
                "name must be 1-64 characters: letters, digits, '-', '_' and '.'",
            ));
        }
        if self.allowed_models.is_empty() || self.allowed_models.len() > 200 {
            return Err(ApiError::bad_request("allowed_models must list between 1 and 200 entries"));
        }
        if let Some(bad) = self.allowed_models.iter().find(|p| !valid_allowed_entry(p)) {
            return Err(ApiError::bad_request(format!(
                "allowed_models entry '{bad}' is not a route, provider/model, provider/* or *"
            )));
        }
        if self.requests_per_minute == Some(0)
            || self.daily_token_budget == Some(0)
            || self.max_concurrent_requests == Some(0)
        {
            return Err(ApiError::bad_request("limits must be greater than 0 (leave them out for no limit)"));
        }
        Ok(())
    }
}

fn name_taken(name: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "invalid_request_error",
        "name_taken",
        format!("a key named '{name}' already exists"),
    )
}

pub async fn create_key(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateKey>,
) -> Result<Response, ApiError> {
    authorize(&app, &headers)?;
    req.validate()?;
    if app.keys.is_config_name(&req.name) {
        return Err(name_taken(&req.name));
    }
    // The store is authoritative: another replica may have created this name.
    let existing = app.store.hgetall(STORED_KEYS).await.map_err(store_unavailable)?;
    if existing.values().filter_map(|v| serde_json::from_str::<StoredKey>(v).ok()).any(|k| k.name == req.name)
    {
        return Err(name_taken(&req.name));
    }

    let key = generate_key();
    let hash = hash_key(&key);
    let rec = StoredKey {
        name: req.name,
        key_hint: format!("{}…", &key[..10]),
        created_at: unix_secs(),
        requests_per_minute: req.requests_per_minute,
        daily_token_budget: req.daily_token_budget,
        max_concurrent_requests: req.max_concurrent_requests,
        allowed_models: req.allowed_models,
    };
    let raw = serde_json::to_string(&rec).map_err(|_| ApiError::internal_config("could not encode key"))?;
    app.store.hset(STORED_KEYS, &hash, &raw).await.map_err(store_unavailable)?;
    app.keys.upsert_dynamic(hash, rec.clone());
    tracing::info!(name = %rec.name, "API key created through the admin API");

    let body = json!({
        "api_key": key,
        "name": rec.name,
        "key_hint": rec.key_hint,
        "created_at": rec.created_at,
        "requests_per_minute": rec.requests_per_minute,
        "daily_token_budget": rec.daily_token_budget,
        "max_concurrent_requests": rec.max_concurrent_requests,
        "allowed_models": rec.allowed_models,
        "note": "Store this key now. The gateway keeps only its hash and cannot show it again.",
    });
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

pub async fn delete_key(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&app, &headers)?;
    if app.keys.is_config_name(&name) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "invalid_request_error",
            "defined_in_config",
            format!("key '{name}' is defined in the config file; remove it there and restart"),
        ));
    }
    let existing = app.store.hgetall(STORED_KEYS).await.map_err(store_unavailable)?;
    let hashes: Vec<String> = existing
        .into_iter()
        .filter(|(_, v)| serde_json::from_str::<StoredKey>(v).is_ok_and(|k| k.name == name))
        .map(|(h, _)| h)
        .collect();
    if hashes.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "not_found",
            format!("no key named '{name}'"),
        ));
    }
    for h in &hashes {
        app.store.hdel(STORED_KEYS, h).await.map_err(store_unavailable)?;
        app.keys.remove_dynamic(h);
    }
    tracing::info!(%name, "API key revoked through the admin API");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_config(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let cfg = &app.cfg;
    let mut providers: Vec<Value> = app
        .providers
        .values()
        .map(|p| {
            json!({
                "name": p.name,
                "kind": match p.cfg.kind {
                    ProviderKind::Openai => "openai",
                    ProviderKind::Azure => "azure",
                    ProviderKind::Anthropic => "anthropic",
                },
                "base_url": p.cfg.base_url,
                "circuit_open": p.breaker.is_open(),
                "timeout_secs": p.cfg.timeout_secs,
                "max_retries": p.cfg.max_retries,
                "embeddings": p.supports_embeddings(),
            })
        })
        .collect();
    providers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut routes: Vec<Value> = app
        .routes
        .values()
        .map(|r| {
            json!({
                "name": r.name,
                "strategy": if r.strategy == RouteStrategy::Weighted { "weighted" } else { "fallback" },
                "targets": r.targets.iter().map(|t| json!({
                    "provider": t.provider,
                    "model": t.model,
                    "weight": t.weight,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    routes.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let guardrails: Vec<Value> = app
        .guardrails
        .rule_summaries()
        .into_iter()
        .map(|(name, action, request, response)| {
            json!({"name": name, "action": action, "request": request, "response": response})
        })
        .collect();
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "providers": providers,
        "routes": routes,
        "default_provider": cfg.default_provider,
        "guardrails": guardrails,
        "cache": { "enabled": cfg.cache.enabled, "ttl_secs": cfg.cache.ttl_secs },
        "logging": {
            "enabled": cfg.logging.enabled,
            "capture_bodies": cfg.logging.capture_bodies,
            "max_entries": cfg.logging.max_entries,
        },
        "store": format!("{:?}", cfg.store.backend).to_lowercase(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None).unwrap(), 86_400);
        assert_eq!(parse_range(Some("15m")).unwrap(), 900);
        assert_eq!(parse_range(Some("7d")).unwrap(), 7 * 86_400);
        assert!(parse_range(Some("0m")).is_err());
        assert!(parse_range(Some("40d")).is_err());
        assert!(parse_range(Some("abc")).is_err());
        assert!(parse_range(Some("")).is_err());
    }

    #[test]
    fn allowed_entries() {
        assert!(valid_allowed_entry("*"));
        assert!(valid_allowed_entry("smart"));
        assert!(valid_allowed_entry("openrouter/*"));
        assert!(valid_allowed_entry("openai/gpt-4o"));
        assert!(!valid_allowed_entry("bad model"));
        assert!(!valid_allowed_entry(""));
    }
}
