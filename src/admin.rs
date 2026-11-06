//! Admin API and dashboard, served on `server.admin_listen` only, plus the
//! client-facing usage endpoints (`/v1/report`, `/v1/generation`,
//! `/v1/feedback`) that a client key can call for its own requests.
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
    config::{valid_ident, valid_model, RuleAction, Secret},
    error::ApiError,
    logs::{self, Feedback, LogQuery, ReportQuery},
    proxy::{budget_key, unix_millis, unix_secs},
    rules::{Rule, STORED_RULES},
    store::StoreError,
};

const DASHBOARD_HTML: &str = include_str!("../dashboard/index.html");
const DASHBOARD_JS: &str = include_str!("../dashboard/app.js");
const PROVIDER_KEYS: &str = "provider_keys";
const AUDIT_LIST: &str = "audit";

fn authorize(app: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = &app.admin_token_sha256 else {
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

fn not_found(what: &str) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "invalid_request_error", "not_found", format!("no such {what}"))
}

/// Records an admin action in the audit log (best effort).
async fn audit(app: &AppState, action: &str, target: &str, detail: Value) {
    let entry = json!({"ts_ms": unix_millis(), "action": action, "target": target, "detail": detail});
    if let Err(e) = app.store.list_push_capped(AUDIT_LIST, &entry.to_string(), 1000).await {
        tracing::warn!(error = %e, "failed to write audit log");
    }
    tracing::info!(action, target, "admin action");
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
    key: Option<String>,
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
    let v = logs::overview(&app, range, unix_secs(), q.key.as_deref()).await.map_err(store_unavailable)?;
    Ok(Json(v))
}

pub async fn list_logs(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let (data, total) = logs::list(&app, &q).await.map_err(store_unavailable)?;
    Ok(Json(json!({ "data": data, "total": total, "capture_bodies": app.rt().cfg.logging.capture_bodies })))
}

pub async fn delete_logs(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let n = logs::delete(&app, &q).await.map_err(store_unavailable)?;
    audit(&app, "logs.delete", "logs", json!({"deleted": n, "filtered": !q.is_empty()})).await;
    Ok(Json(json!({ "deleted": n })))
}

pub async fn get_log(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    match logs::get(&app, &id).await.map_err(store_unavailable)? {
        Some(e) => Ok(Json(json!(e))),
        None => Err(not_found("request")),
    }
}

#[derive(Debug, Deserialize)]
pub struct FeedbackBody {
    #[serde(default)]
    request_id: Option<String>,
    /// 1 (thumbs up), -1 (thumbs down) or 0.
    #[serde(default)]
    rating: Option<i8>,
    #[serde(default)]
    score: Option<u8>,
    #[serde(default)]
    comment: Option<String>,
}

fn to_feedback(b: &FeedbackBody) -> Result<Feedback, ApiError> {
    if b.rating.is_none() && b.score.is_none() {
        return Err(ApiError::bad_request("send rating (1, 0 or -1) and/or score (0-100)"));
    }
    if b.rating.is_some_and(|r| !(-1..=1).contains(&r)) || b.score.is_some_and(|s| s > 100) {
        return Err(ApiError::bad_request("rating must be -1, 0 or 1 and score 0-100"));
    }
    if b.comment.as_ref().is_some_and(|c| c.len() > 2000) {
        return Err(ApiError::bad_request("comment must be at most 2000 characters"));
    }
    Ok(Feedback {
        rating: b.rating.unwrap_or(0),
        score: b.score,
        comment: b.comment.clone(),
        ts: unix_secs(),
    })
}

pub async fn set_feedback(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<FeedbackBody>,
) -> Result<StatusCode, ApiError> {
    authorize(&app, &headers)?;
    let fb = to_feedback(&b)?;
    let entry = logs::get(&app, &id).await.map_err(store_unavailable)?.ok_or_else(|| not_found("request"))?;
    logs::set_feedback(&app, &entry, &fb).await.map_err(store_unavailable)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/feedback` with a client key: feedback on the key's own request.
pub async fn client_feedback(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(b): Json<FeedbackBody>,
) -> Result<StatusCode, ApiError> {
    let tenant = app.keys.authenticate(&headers)?;
    let fb = to_feedback(&b)?;
    let id = b.request_id.as_deref().ok_or_else(|| ApiError::bad_request("request_id is required"))?;
    let entry = logs::get(&app, id).await.map_err(store_unavailable)?;
    let entry = entry.filter(|e| e.tenant == tenant.name).ok_or_else(|| not_found("request"))?;
    logs::set_feedback(&app, &entry, &fb).await.map_err(store_unavailable)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct IdQuery {
    id: String,
}

/// `GET /v1/generation?id=...` with a client key: one of its own requests,
/// without prompts or responses.
pub async fn client_generation(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<IdQuery>,
) -> Result<Json<Value>, ApiError> {
    let tenant = app.keys.authenticate(&headers)?;
    let e = logs::get(&app, &q.id).await.map_err(store_unavailable)?;
    let mut e = e.filter(|e| e.tenant == tenant.name).ok_or_else(|| not_found("generation"))?;
    e.request_body = None;
    e.response_body = None;
    Ok(Json(json!({ "data": e })))
}

pub async fn report(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ReportQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    match logs::report(&app, &q, None).await.map_err(store_unavailable)? {
        Ok(v) => Ok(Json(v)),
        Err(msg) => Err(ApiError::bad_request(msg)),
    }
}

/// `GET /v1/report` with a client key: the key's own usage only.
pub async fn client_report(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ReportQuery>,
) -> Result<Json<Value>, ApiError> {
    let tenant = app.keys.authenticate(&headers)?;
    match logs::report(&app, &q, Some(&tenant.name)).await.map_err(store_unavailable)? {
        Ok(v) => Ok(Json(v)),
        Err(msg) => Err(ApiError::bad_request(msg)),
    }
}

/// Reloads keys, routing rules and provider keys created through the admin
/// API from the store.
pub async fn refresh_dynamic(app: &AppState) -> Result<(), StoreError> {
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

    let rules: Vec<Rule> = app
        .store
        .hgetall(STORED_RULES)
        .await?
        .values()
        .filter_map(|v| serde_json::from_str(v).ok())
        .collect();
    app.rules.set_stored(rules);

    if let Some(cipher) = &app.cipher {
        let mut keys = HashMap::new();
        for (provider, enc) in app.store.hgetall(PROVIDER_KEYS).await? {
            match cipher.decrypt(&enc) {
                Ok(k) => {
                    keys.insert(provider, Secret::new(k));
                }
                Err(e) => {
                    tracing::warn!(error = %e, %provider, "ignoring a stored provider key that cannot be decrypted")
                }
            }
        }
        *app.stored_provider_keys.write() = keys;
    }
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
                "zero_data_retention": t.zero_data_retention,
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
    #[serde(default)]
    zero_data_retention: bool,
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

fn conflict(code: &'static str, message: String) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "invalid_request_error", code, message)
}

pub async fn create_key(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateKey>,
) -> Result<Response, ApiError> {
    authorize(&app, &headers)?;
    req.validate()?;
    let taken = || conflict("name_taken", format!("a key named '{}' already exists", req.name));
    if app.keys.is_config_name(&req.name) {
        return Err(taken());
    }
    // The store is authoritative: another replica may have created this name.
    let existing = app.store.hgetall(STORED_KEYS).await.map_err(store_unavailable)?;
    if existing.values().filter_map(|v| serde_json::from_str::<StoredKey>(v).ok()).any(|k| k.name == req.name)
    {
        return Err(taken());
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
        zero_data_retention: req.zero_data_retention,
    };
    let raw = serde_json::to_string(&rec).map_err(|_| ApiError::internal_config("could not encode key"))?;
    app.store.hset(STORED_KEYS, &hash, &raw).await.map_err(store_unavailable)?;
    app.keys.upsert_dynamic(hash, rec.clone());
    audit(&app, "key.create", &rec.name, json!({"allowed_models": rec.allowed_models})).await;

    let body = json!({
        "api_key": key,
        "name": rec.name,
        "key_hint": rec.key_hint,
        "created_at": rec.created_at,
        "requests_per_minute": rec.requests_per_minute,
        "daily_token_budget": rec.daily_token_budget,
        "max_concurrent_requests": rec.max_concurrent_requests,
        "allowed_models": rec.allowed_models,
        "zero_data_retention": rec.zero_data_retention,
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
        return Err(conflict(
            "defined_in_config",
            format!("key '{name}' is defined in the config file; remove it there and reload"),
        ));
    }
    let existing = app.store.hgetall(STORED_KEYS).await.map_err(store_unavailable)?;
    let hashes: Vec<String> = existing
        .into_iter()
        .filter(|(_, v)| serde_json::from_str::<StoredKey>(v).is_ok_and(|k| k.name == name))
        .map(|(h, _)| h)
        .collect();
    if hashes.is_empty() {
        return Err(not_found("key"));
    }
    for h in &hashes {
        app.store.hdel(STORED_KEYS, h).await.map_err(store_unavailable)?;
        app.keys.remove_dynamic(h);
    }
    audit(&app, "key.revoke", &name, json!({})).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_rules(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    Ok(Json(json!({ "data": app.rules.all() })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleBody {
    source: Option<String>,
    action: Option<RuleAction>,
    #[serde(default)]
    destination: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

fn validate_rule(r: &Rule) -> Result<(), ApiError> {
    let src_ok = r.source == "*" || valid_model(r.source.strip_suffix("/*").unwrap_or(&r.source));
    if !src_ok {
        return Err(ApiError::bad_request("source must be a model, provider/*, or *"));
    }
    if r.action == RuleAction::Rewrite && !r.destination.as_deref().is_some_and(valid_model) {
        return Err(ApiError::bad_request("a rewrite rule needs a valid destination model"));
    }
    if r.reason.as_ref().is_some_and(|x| x.len() > 500) {
        return Err(ApiError::bad_request("reason must be at most 500 characters"));
    }
    Ok(())
}

pub async fn create_rule(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(b): Json<RuleBody>,
) -> Result<Response, ApiError> {
    authorize(&app, &headers)?;
    let rule = Rule {
        id: format!("rule_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
        source: b.source.ok_or_else(|| ApiError::bad_request("source is required"))?,
        action: b.action.ok_or_else(|| ApiError::bad_request("action is required (rewrite or deny)"))?,
        destination: b.destination,
        reason: b.reason,
        enabled: b.enabled.unwrap_or(true),
        origin: "admin".into(),
        created_at: unix_millis(),
    };
    validate_rule(&rule)?;
    let raw = serde_json::to_string(&rule).unwrap_or_default();
    app.store.hset(STORED_RULES, &rule.id, &raw).await.map_err(store_unavailable)?;
    app.rules.upsert_stored(rule.clone());
    audit(&app, "rule.create", &rule.id, json!(rule)).await;
    Ok((StatusCode::CREATED, Json(json!(rule))).into_response())
}

pub async fn update_rule(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<RuleBody>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let stored = app.store.hgetall(STORED_RULES).await.map_err(store_unavailable)?;
    let mut rule: Rule = stored
        .get(&id)
        .and_then(|v| serde_json::from_str(v).ok())
        .ok_or_else(|| not_found("rule (rules from the config file are edited there)"))?;
    if let Some(s) = b.source {
        rule.source = s;
    }
    if let Some(a) = b.action {
        rule.action = a;
    }
    if b.destination.is_some() {
        rule.destination = b.destination;
    }
    if b.reason.is_some() {
        rule.reason = b.reason;
    }
    if let Some(e) = b.enabled {
        rule.enabled = e;
    }
    validate_rule(&rule)?;
    app.store
        .hset(STORED_RULES, &id, &serde_json::to_string(&rule).unwrap_or_default())
        .await
        .map_err(store_unavailable)?;
    app.rules.upsert_stored(rule.clone());
    audit(&app, "rule.update", &id, json!(rule)).await;
    Ok(Json(json!(rule)))
}

pub async fn delete_rule(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&app, &headers)?;
    if !app.store.hdel(STORED_RULES, &id).await.map_err(store_unavailable)? {
        return Err(not_found("rule (rules from the config file are removed there)"));
    }
    app.rules.remove_stored(&id);
    audit(&app, "rule.delete", &id, json!({})).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_provider_keys(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let stored = app.stored_provider_keys.read();
    let rt = app.rt();
    let mut data: Vec<Value> = rt
        .providers
        .values()
        .map(|p| {
            let k = stored.get(&p.name);
            json!({
                "provider": p.name,
                "source": if k.is_some() { "stored" } else { "config" },
                "hint": k.map(|k| hint(k.expose())).unwrap_or_else(|| hint(p.cfg.api_key.expose())),
                "allow_client_keys": p.cfg.allow_client_keys,
            })
        })
        .collect();
    data.sort_by(|a, b| a["provider"].as_str().cmp(&b["provider"].as_str()));
    Ok(Json(json!({ "data": data, "storage_enabled": app.cipher.is_some() })))
}

fn hint(k: &str) -> String {
    let n = k.chars().count();
    if n <= 8 {
        return "••••".into();
    }
    let tail: String = k.chars().skip(n - 4).collect();
    format!("{}…{tail}", k.chars().take(3).collect::<String>())
}

#[derive(Deserialize)]
pub struct ProviderKeyBody {
    api_key: String,
}

pub async fn put_provider_key(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    Json(b): Json<ProviderKeyBody>,
) -> Result<StatusCode, ApiError> {
    authorize(&app, &headers)?;
    let cipher = app.cipher.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "invalid_request_error",
            "storage_disabled",
            "set admin.encryption_key to store provider keys",
        )
    })?;
    if !app.rt().providers.contains_key(&provider) {
        return Err(not_found("provider"));
    }
    let key = b.api_key.trim();
    if key.is_empty() || key.len() > 1024 || axum::http::HeaderValue::from_str(key).is_err() {
        return Err(ApiError::bad_request("api_key must be 1-1024 printable characters"));
    }
    let enc = cipher.encrypt(key).map_err(|_| ApiError::internal_config("encryption failed"))?;
    app.store.hset(PROVIDER_KEYS, &provider, &enc).await.map_err(store_unavailable)?;
    app.stored_provider_keys.write().insert(provider.clone(), Secret::new(key));
    audit(&app, "provider_key.set", &provider, json!({"hint": hint(key)})).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_provider_key(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&app, &headers)?;
    if !app.store.hdel(PROVIDER_KEYS, &provider).await.map_err(store_unavailable)? {
        return Err(not_found("stored key for that provider"));
    }
    app.stored_provider_keys.write().remove(&provider);
    audit(&app, "provider_key.delete", &provider, json!({})).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_limits(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let rt = app.rt();
    let tenants: Vec<String> = app.keys.tenants().into_iter().map(|(t, _)| t.name.clone()).collect();
    let rows = crate::limits::snapshot(&app.store, &rt.cfg.limits, &tenants, unix_secs()).await;
    Ok(Json(json!({ "data": rows })))
}

pub async fn audit_log(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let raw = app.store.list_range(AUDIT_LIST, 1000).await.map_err(store_unavailable)?;
    let data: Vec<Value> = raw.iter().filter_map(|r| serde_json::from_str(r).ok()).collect();
    Ok(Json(json!({ "data": data })))
}

pub async fn reload(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    match app.reload() {
        Ok(notes) => {
            audit(&app, "config.reload", "config", json!({"notes": notes})).await;
            Ok(Json(json!({ "reloaded": true, "notes": notes })))
        }
        Err(e) => Err(ApiError::bad_request(format!("config not reloaded: {e:#}"))),
    }
}

pub async fn get_config(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    let rt = app.rt();
    let cfg = &rt.cfg;
    let stats = app.stats.snapshot();
    let mut providers: Vec<Value> = rt
        .providers
        .values()
        .map(|p| {
            let models: Vec<Value> = stats
                .iter()
                .filter(|(k, _)| k.split_once('/').is_some_and(|(pp, _)| pp == p.name))
                .map(|(k, s)| json!({"target": k, "ttft_ms": s.ttft_ms.round(), "tokens_per_sec": (s.tokens_per_sec * 10.0).round() / 10.0, "success_rate": s.success_rate, "samples": s.samples}))
                .collect();
            use crate::proxy::Endpoint::*;
            let endpoints: Vec<&str> = [Chat, Embeddings, Responses, Messages, Images, Moderations, Rerank, Speech]
                .into_iter()
                .filter(|e| p.supports(*e))
                .map(|e| e.as_str())
                .collect();
            json!({
                "name": p.name,
                "kind": p.cfg.kind,
                "base_url": p.cfg.base_url,
                "circuit_open": p.breaker.is_open(),
                "timeout_secs": p.cfg.timeout_secs,
                "max_retries": p.cfg.max_retries,
                "zero_data_retention": p.cfg.zero_data_retention,
                "no_prompt_training": p.cfg.no_prompt_training,
                "auto_prompt_cache": p.cfg.auto_prompt_cache,
                "endpoints": endpoints,
                "targets": models,
            })
        })
        .collect();
    providers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut routes: Vec<Value> = rt
        .routes
        .values()
        .map(|r| {
            json!({
                "name": r.name,
                "strategy": r.strategy,
                "targets": r.targets.iter().map(|t| json!({"provider": t.provider, "model": t.model, "weight": t.weight})).collect::<Vec<_>>(),
                "branches": r.branches.iter().map(|b| json!({
                    "when": b.when.keys().cloned().collect::<Vec<_>>(),
                    "strategy": b.strategy,
                    "targets": b.targets.iter().map(|t| format!("{}/{}", t.provider, t.model)).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    routes.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let guardrails: Vec<Value> = rt
        .guardrails
        .rule_summaries()
        .into_iter()
        .map(|(name, action, request, response)| json!({"name": name, "action": action, "request": request, "response": response}))
        .collect();
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "providers": providers,
        "routes": routes,
        "default_provider": cfg.default_provider,
        "guardrails": guardrails,
        "limits": cfg.limits.len(),
        "routing_rules": app.rules.all().len(),
        "cache": { "enabled": cfg.cache.enabled, "ttl_secs": cfg.cache.ttl_secs },
        "logging": {
            "enabled": cfg.logging.enabled,
            "capture_bodies": cfg.logging.capture_bodies,
            "max_entries": cfg.logging.max_entries,
            "export": cfg.logging.export.as_ref().map(|e| e.url.clone()),
            "otlp": cfg.logging.otlp.as_ref().map(|o| o.endpoint.clone()),
        },
        "alerts": cfg.alerts.webhook_url.is_some(),
        "byok_storage": app.cipher.is_some(),
        "reloadable": app.config_path.read().is_some(),
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

    #[test]
    fn key_hints_hide_secrets() {
        assert_eq!(hint("sk-1234567890abcd"), "sk-…abcd");
        assert_eq!(hint("short"), "••••");
    }
}
