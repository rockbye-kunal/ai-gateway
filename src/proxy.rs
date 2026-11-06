//! The request pipeline:
//! authenticate → validate → resolve route → rate limit → budget → cache →
//! try targets in order (retries with backoff, circuit breakers, fallback) →
//! translate → respond.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    app::AppState,
    auth::Tenant,
    config::{valid_model, FailMode, TargetConfig},
    error::ApiError,
    providers::Provider,
    sse,
    store::StoreError,
    usage::{RequestGuard, Usage},
};

pub fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Store key for a tenant's token usage today (UTC). The `{tenant}` hash tag
/// keeps all of a tenant's keys in one Redis Cluster slot, which the
/// idempotent usage script needs.
pub fn budget_key(tenant: &str) -> String {
    format!("budget:{{{}}}:{}", tenant, unix_secs() / 86_400)
}

#[derive(Serialize, Deserialize)]
struct CachedResponse {
    provider: String,
    model: String,
    body: String,
}

pub async fn chat_completions(State(app): State<Arc<AppState>>, req: Request) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut resp = match app.limiter.clone().try_acquire_owned() {
        Err(_) => {
            metrics::counter!("gateway_rejected_total", "reason" => "overloaded").increment(1);
            ApiError::overloaded().into_response()
        }
        Ok(permit) => {
            let guard = RequestGuard::new(app.clone(), request_id.clone(), permit);
            let deadline = Duration::from_secs(app.cfg.server.request_timeout_secs);
            let result = match tokio::time::timeout(deadline, handle(&app, req, &guard)).await {
                Ok(r) => r,
                Err(_) => Err(ApiError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "server_error",
                    "gateway_timeout",
                    "the request did not complete within the gateway's time limit",
                )),
            };
            match result {
                Ok(r) => {
                    let status = r.status().as_u16();
                    guard.update(|s| s.status = status);
                    r
                }
                Err(e) => {
                    guard.update(|s| {
                        s.status = e.status.as_u16();
                        if s.error.is_none() {
                            s.error = Some(e.message.clone());
                        }
                    });
                    e.into_response()
                }
            }
        }
    };
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

pub async fn list_models(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let tenant = app.keys.authenticate(&headers)?;
    let mut ids: Vec<&String> = app.routes.keys().filter(|r| tenant.allows_route(r)).collect();
    ids.sort();
    let data: Vec<Value> = ids
        .into_iter()
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "gateway"}))
        .collect();
    Ok(Json(json!({"object": "list", "data": data})))
}

async fn handle(app: &Arc<AppState>, req: Request, guard: &RequestGuard) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let headers = &parts.headers;

    // Authenticate and claim the tenant's concurrency slot before reading the
    // body, so unauthenticated clients cannot make the gateway buffer data.
    let tenant = app.keys.authenticate(headers)?;
    guard.update(|s| s.tenant = Some(tenant.clone()));
    if let Some(sem) = &tenant.concurrency {
        let permit = sem.clone().try_acquire_owned().map_err(|_| {
            metrics::counter!("gateway_rejected_total", "reason" => "tenant_concurrency").increment(1);
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "concurrency_limit_exceeded",
                "too many concurrent requests for this API key",
            )
            .with_retry_after(1)
        })?;
        guard.update(|s| s.tenant_permit = Some(permit));
    }

    let body = axum::body::to_bytes(body, app.cfg.server.max_body_bytes).await.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request_too_large",
            format!("request body exceeds {} bytes or could not be read", app.cfg.server.max_body_bytes),
        )
    })?;

    let req: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("request body is not valid JSON: {e}")))?;
    let obj = req.as_object().ok_or_else(|| ApiError::bad_request("request body must be a JSON object"))?;
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("'model' is required"))?
        .to_string();
    if !valid_model(&model) {
        return Err(ApiError::bad_request("'model' contains invalid characters"));
    }
    match obj.get("messages") {
        Some(Value::Array(m)) if !m.is_empty() => {}
        _ => return Err(ApiError::bad_request("'messages' must be a non-empty array")),
    }
    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let prompt_estimate = sse::estimate_prompt_tokens(&req);
    let client_wants_usage = obj
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    guard.update(|s| {
        s.route = Some(model.clone());
        s.stream = stream;
    });

    let targets = resolve_targets(app, &tenant, &model)?;
    enforce_limits(app, &tenant).await?;

    let cache_key =
        (app.cfg.cache.enabled && !stream && !cache_bypassed(headers)).then(|| cache_key(&tenant.name, &req));
    if let Some(key) = &cache_key {
        match app.store.cache_get(key).await {
            Ok(Some(raw)) => {
                if let Ok(hit) = serde_json::from_slice::<CachedResponse>(&raw) {
                    metrics::counter!("gateway_cache_total", "result" => "hit").increment(1);
                    guard.update(|s| {
                        s.cache_hit = true;
                        s.provider = Some(hit.provider.clone());
                        s.model = Some(hit.model.clone());
                    });
                    return Ok(json_response(hit.body.into_bytes(), &hit.provider, &hit.model, Some("HIT")));
                }
            }
            Ok(None) => {
                metrics::counter!("gateway_cache_total", "result" => "miss").increment(1);
            }
            Err(e) => {
                metrics::counter!("gateway_store_errors_total", "op" => "cache_get").increment(1);
                tracing::warn!(error = %e, "cache lookup failed; continuing without cache");
            }
        }
    }

    let ctx = AttemptCtx {
        app: app.as_ref(),
        req: &req,
        stream,
        client_wants_usage,
        guard,
        cache_key: cache_key.as_deref(),
        prompt_estimate,
    };
    let mut last_error = ApiError::unavailable("no upstream provider is available for this model");
    for target in &targets {
        let Some(provider) = app.providers.get(&target.provider) else { continue };
        for attempt in 0..=provider.cfg.max_retries {
            if attempt > 0 {
                tokio::time::sleep(backoff(attempt)).await;
            }
            if !provider.breaker.allow() {
                metrics::counter!("gateway_upstream_skipped_total", "provider" => provider.name.clone())
                    .increment(1);
                last_error =
                    ApiError::unavailable(format!("provider '{}' is temporarily unavailable", provider.name));
                break;
            }
            guard.update(|s| s.attempts += 1);
            match try_target(&ctx, provider, target).await {
                Attempt::Done(resp) => return Ok(resp),
                Attempt::Retry(e) => last_error = e,
                Attempt::NextTarget(e) => {
                    last_error = e;
                    break;
                }
            }
        }
    }
    Err(last_error)
}

fn resolve_targets(app: &AppState, tenant: &Tenant, model: &str) -> Result<Vec<TargetConfig>, ApiError> {
    if let Some(targets) = app.routes.get(model) {
        if !tenant.allows_route(model) {
            return Err(ApiError::forbidden(format!("this API key may not use model '{model}'")));
        }
        return Ok(targets.clone());
    }
    if let Some((provider, upstream_model)) = model.split_once('/') {
        if let Some(p) = app.providers.get(provider) {
            if !tenant.allows_direct(provider, upstream_model) {
                return Err(ApiError::forbidden(format!("this API key may not use model '{model}'")));
            }
            if p.cfg.kind == crate::config::ProviderKind::Azure && upstream_model.contains('/') {
                return Err(ApiError::bad_request("azure deployment names cannot contain '/'"));
            }
            return Ok(vec![TargetConfig {
                provider: provider.to_string(),
                model: upstream_model.to_string(),
                price_input_per_mtok: None,
                price_output_per_mtok: None,
            }]);
        }
    }
    Err(ApiError::model_not_found(model))
}

async fn enforce_limits(app: &AppState, tenant: &Tenant) -> Result<(), ApiError> {
    let now = unix_secs();
    if let Some(rpm) = tenant.requests_per_minute {
        let key = format!("rl:{}:{}", tenant.name, now / 60);
        match app.store.incr(&key, 1, 120).await {
            Ok(n) if n > rpm => {
                metrics::counter!("gateway_rejected_total", "reason" => "rate_limit").increment(1);
                return Err(ApiError::rate_limited(60 - now % 60));
            }
            Ok(_) => {}
            Err(e) => on_store_error(app, "rate_limit", e)?,
        }
    }
    if let Some(budget) = tenant.daily_token_budget {
        match app.store.get_counter(&budget_key(&tenant.name)).await {
            Ok(used) if used >= budget => {
                metrics::counter!("gateway_rejected_total", "reason" => "budget").increment(1);
                return Err(ApiError::budget_exceeded(86_400 - now % 86_400));
            }
            Ok(_) => {}
            Err(e) => on_store_error(app, "budget", e)?,
        }
    }
    Ok(())
}

fn on_store_error(app: &AppState, op: &'static str, e: StoreError) -> Result<(), ApiError> {
    metrics::counter!("gateway_store_errors_total", "op" => op).increment(1);
    tracing::error!(error = %e, op, "store unavailable");
    match app.cfg.store.on_error {
        FailMode::Open => Ok(()),
        FailMode::Closed => {
            Err(ApiError::unavailable("rate limiting backend is unavailable, retry shortly")
                .with_retry_after(2))
        }
    }
}

fn cache_bypassed(headers: &HeaderMap) -> bool {
    headers
        .get("x-gateway-cache")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("skip"))
}

/// Cache entries are scoped per tenant so one client can never read another
/// client's cached completion.
fn cache_key(tenant: &str, req: &Value) -> String {
    let mut h = Sha256::new();
    h.update(tenant.as_bytes());
    h.update([0u8]);
    h.update(serde_json::to_vec(req).unwrap_or_default());
    format!("cache:{}", hex::encode(h.finalize()))
}

/// Exponential backoff with jitter: ~250ms, 500ms, 1s, ... capped at 4s.
fn backoff(attempt: u32) -> Duration {
    let base = 250u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(4)).min(4_000);
    let jittered = rand::thread_rng().gen_range(base / 2..=base);
    Duration::from_millis(jittered)
}

fn json_response(body: Vec<u8>, provider: &str, model: &str, cache: Option<&str>) -> Response {
    let mut r = Response::new(Body::from(body));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    set_gateway_headers(h, provider, model);
    if let Some(c) = cache.and_then(|c| HeaderValue::from_str(c).ok()) {
        h.insert("x-gateway-cache", c);
    }
    r
}

fn set_gateway_headers(h: &mut HeaderMap, provider: &str, model: &str) {
    if let Ok(v) = HeaderValue::from_str(provider) {
        h.insert("x-gateway-provider", v);
    }
    if let Ok(v) = HeaderValue::from_str(model) {
        h.insert("x-gateway-model", v);
    }
}

struct AttemptCtx<'a> {
    app: &'a AppState,
    req: &'a Value,
    stream: bool,
    client_wants_usage: bool,
    guard: &'a RequestGuard,
    cache_key: Option<&'a str>,
    prompt_estimate: u64,
}

enum Attempt {
    /// A response to return to the client (success or a client error).
    Done(Response),
    /// Transient failure; retry this provider if attempts remain.
    Retry(ApiError),
    /// This provider cannot serve the request; move to the next target.
    NextTarget(ApiError),
}

async fn try_target(ctx: &AttemptCtx<'_>, provider: &Provider, target: &TargetConfig) -> Attempt {
    let pname = provider.name.clone();
    let rb = match provider.build(ctx.req, &target.model, ctx.stream) {
        Ok(rb) => rb,
        // Translation errors are the client's request being unsupported.
        // Report a neutral outcome so a half-open probe slot is released.
        Err(e) => {
            provider.breaker.record_neutral();
            return Attempt::Done(e.into_response());
        }
    };

    let started = Instant::now();
    let resp = match tokio::time::timeout(provider.timeout(), rb.send()).await {
        Err(_) => {
            provider.breaker.record_failure();
            upstream_error_metric(&pname, "timeout");
            tracing::warn!(request_id = ctx.guard.request_id(), provider = %pname, "upstream timed out");
            return Attempt::Retry(ApiError::upstream_timeout());
        }
        Ok(Err(e)) => {
            provider.breaker.record_failure();
            upstream_error_metric(&pname, "transport");
            tracing::warn!(request_id = ctx.guard.request_id(), provider = %pname, error = %e,
                "upstream request failed");
            return Attempt::Retry(ApiError::upstream("upstream provider request failed"));
        }
        Ok(Ok(r)) => r,
    };

    let status = resp.status();
    if status.is_success() {
        if ctx.stream {
            provider.breaker.record_success();
            ctx.guard.update(|s| {
                s.provider = Some(pname.clone());
                s.model = Some(target.model.clone());
                s.price_input_per_mtok = target.price_input_per_mtok;
                s.price_output_per_mtok = target.price_output_per_mtok;
                // The upstream has accepted the prompt. Bill it even if the
                // client disconnects before the first chunk arrives.
                s.usage = Usage { prompt_tokens: ctx.prompt_estimate, completion_tokens: 0 };
            });
            let translator = provider.stream_translator(&target.model, ctx.client_wants_usage);
            let body = Body::from_stream(sse::event_stream(
                resp,
                translator,
                provider.idle_timeout(),
                ctx.prompt_estimate,
                ctx.app.stream_cancel.subscribe(),
                ctx.guard.clone(),
            ));
            let mut r = Response::new(body);
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            h.insert("x-accel-buffering", HeaderValue::from_static("no"));
            set_gateway_headers(h, &pname, &target.model);
            return Attempt::Done(r);
        }

        let remaining = provider.timeout().saturating_sub(started.elapsed());
        let bytes = match tokio::time::timeout(remaining, resp.bytes()).await {
            Err(_) => {
                provider.breaker.record_failure();
                upstream_error_metric(&pname, "timeout");
                return Attempt::Retry(ApiError::upstream_timeout());
            }
            Ok(Err(e)) => {
                provider.breaker.record_failure();
                upstream_error_metric(&pname, "transport");
                tracing::warn!(request_id = ctx.guard.request_id(), provider = %pname, error = %e,
                    "reading upstream body failed");
                return Attempt::Retry(ApiError::upstream("upstream provider request failed"));
            }
            Ok(Ok(b)) => b,
        };
        let (out, usage) = match provider.translate_response(&bytes, &target.model) {
            Ok(x) => x,
            Err(e) => {
                provider.breaker.record_failure();
                upstream_error_metric(&pname, "invalid_response");
                return Attempt::Retry(e);
            }
        };
        provider.breaker.record_success();
        ctx.guard.update(|s| {
            s.provider = Some(pname.clone());
            s.model = Some(target.model.clone());
            s.usage = usage;
            s.price_input_per_mtok = target.price_input_per_mtok;
            s.price_output_per_mtok = target.price_output_per_mtok;
        });

        if let Some(key) = ctx.cache_key {
            store_in_cache(ctx.app, key, &out, &pname, &target.model).await;
        }
        return Attempt::Done(json_response(out, &pname, &target.model, ctx.cache_key.map(|_| "MISS")));
    }

    // Upstream returned an error status.
    let code = status.as_u16();
    let body = match tokio::time::timeout(Duration::from_secs(10), resp.bytes()).await {
        Ok(Ok(b)) => b,
        _ => Bytes::new(),
    };
    upstream_error_metric(&pname, &code.to_string());
    tracing::warn!(request_id = ctx.guard.request_id(), provider = %pname, status = code,
        "upstream returned an error status");

    match code {
        // The request itself is bad; another provider would reject it too.
        // Neutral, not success: steady client errors must not hide a
        // provider that is otherwise failing.
        400 | 413 | 422 => {
            provider.breaker.record_neutral();
            let mut r = (status, Json(provider.translate_error(code, &body))).into_response();
            set_gateway_headers(r.headers_mut(), &pname, &target.model);
            Attempt::Done(r)
        }
        // Our credentials are wrong for this provider. Never expose upstream
        // auth details; try the next target.
        401 | 403 => {
            provider.breaker.record_neutral();
            tracing::error!(provider = %pname, status = code,
                "provider rejected the gateway's credentials; check its api_key");
            Attempt::NextTarget(ApiError::upstream("upstream provider rejected the gateway's credentials"))
        }
        // Redirects are never followed (they would carry provider keys).
        // A redirect means base_url is wrong; retrying cannot help.
        300..=399 => {
            provider.breaker.record_neutral();
            tracing::error!(provider = %pname, status = code,
                "provider redirected the request; fix its base_url");
            Attempt::NextTarget(ApiError::upstream("upstream provider is misconfigured"))
        }
        // Usually a wrong deployment name or base_url for this provider, not
        // the client's fault. Try the next target without exposing details.
        404 => {
            provider.breaker.record_neutral();
            tracing::error!(provider = %pname, model = %target.model,
                "provider returned 404; check the model/deployment name and base_url");
            Attempt::NextTarget(ApiError::upstream("upstream provider could not find the requested model"))
        }
        429 => {
            provider.breaker.record_neutral();
            Attempt::Retry(
                ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limit_error",
                    "upstream_rate_limited",
                    "upstream provider is rate limiting requests",
                )
                .with_retry_after(5),
            )
        }
        500..=599 => {
            provider.breaker.record_failure();
            Attempt::Retry(ApiError::upstream(format!("upstream provider returned {code}")))
        }
        _ => {
            provider.breaker.record_neutral();
            Attempt::Retry(ApiError::upstream(format!("upstream provider returned {code}")))
        }
    }
}

async fn store_in_cache(app: &AppState, key: &str, body: &[u8], provider: &str, model: &str) {
    if body.len() > app.cfg.cache.max_entry_bytes {
        return;
    }
    let Ok(body) = String::from_utf8(body.to_vec()) else { return };
    let entry = CachedResponse { provider: provider.to_string(), model: model.to_string(), body };
    let Ok(raw) = serde_json::to_vec(&entry) else { return };
    if let Err(e) = app.store.cache_set(key, &raw, app.cfg.cache.ttl_secs).await {
        metrics::counter!("gateway_store_errors_total", "op" => "cache_set").increment(1);
        tracing::warn!(error = %e, "failed to write response cache");
    }
}

fn upstream_error_metric(provider: &str, kind: &str) {
    metrics::counter!(
        "gateway_upstream_errors_total",
        "provider" => provider.to_string(),
        "kind" => kind.to_string()
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        for _ in 0..50 {
            let b1 = backoff(1).as_millis();
            assert!((125..=250).contains(&b1));
            let b10 = backoff(10).as_millis();
            assert!((2000..=4000).contains(&b10));
        }
    }

    #[test]
    fn cache_keys_are_tenant_scoped() {
        let req = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        assert_ne!(cache_key("a", &req), cache_key("b", &req));
        assert_eq!(cache_key("a", &req), cache_key("a", &req.clone()));
    }
}
