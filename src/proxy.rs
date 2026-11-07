//! The request pipeline for the unified APIs:
//! authenticate → read options → validate → routing rules → resolve targets
//! (routes, branches, fallback models, order/only/sort, ZDR) → limits →
//! guardrails → cache → try targets (retries with backoff, circuit breakers,
//! per-provider limits, BYOK, fallback) → translate → respond.

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
    app::{AppState, Runtime},
    auth::Tenant,
    config::{valid_model, Condition, FailMode, ProviderKind, RouteConfig, RouteStrategy, TargetConfig},
    error::ApiError,
    limits::{self, CheckError, Dims},
    logs::AttemptLog,
    options::{Backoff, RequestOptions, Sort},
    providers::Provider,
    rules::Outcome,
    sse::{self, CacheSink},
    store::StoreError,
    usage::{RequestGuard, Usage},
};

pub fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
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
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    content_type: Option<String>,
}

/// The API surface a request came in on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Chat,
    Embeddings,
    Responses,
    Messages,
    Images,
    Moderations,
    Rerank,
    Speech,
}

impl Endpoint {
    pub fn as_str(self) -> &'static str {
        match self {
            Endpoint::Chat => "chat",
            Endpoint::Embeddings => "embeddings",
            Endpoint::Responses => "responses",
            Endpoint::Messages => "messages",
            Endpoint::Images => "images",
            Endpoint::Moderations => "moderations",
            Endpoint::Rerank => "rerank",
            Endpoint::Speech => "speech",
        }
    }

    /// Upstream path for OpenAI-style providers.
    fn path(self) -> &'static str {
        match self {
            Endpoint::Chat => "chat/completions",
            Endpoint::Embeddings => "embeddings",
            Endpoint::Responses => "responses",
            Endpoint::Messages => "messages",
            Endpoint::Images => "images/generations",
            Endpoint::Moderations => "moderations",
            Endpoint::Rerank => "rerank",
            Endpoint::Speech => "audio/speech",
        }
    }

    pub fn supports(self, kind: ProviderKind) -> bool {
        use ProviderKind::*;
        match self {
            Endpoint::Chat => true,
            Endpoint::Embeddings | Endpoint::Images => matches!(kind, Openai | Azure),
            Endpoint::Responses | Endpoint::Moderations | Endpoint::Rerank | Endpoint::Speech => {
                kind == Openai
            }
            // Vertex serves Messages for Claude models only; others get a 400.
            Endpoint::Messages => matches!(kind, Anthropic | Vertex),
        }
    }

    fn required_field(self) -> &'static str {
        match self {
            Endpoint::Chat | Endpoint::Messages => "messages",
            Endpoint::Embeddings | Endpoint::Responses | Endpoint::Moderations | Endpoint::Speech => "input",
            Endpoint::Images => "prompt",
            Endpoint::Rerank => "query",
        }
    }

    fn streams(self) -> bool {
        matches!(self, Endpoint::Chat | Endpoint::Responses | Endpoint::Messages)
    }

    fn cacheable(self) -> bool {
        !matches!(self, Endpoint::Images | Endpoint::Speech)
    }
}

macro_rules! endpoint_handler {
    ($name:ident, $ep:expr) => {
        pub async fn $name(State(app): State<Arc<AppState>>, req: Request) -> Response {
            serve(app, req, $ep).await
        }
    };
}

endpoint_handler!(chat_completions, Endpoint::Chat);
endpoint_handler!(embeddings, Endpoint::Embeddings);
endpoint_handler!(responses, Endpoint::Responses);
endpoint_handler!(messages, Endpoint::Messages);
endpoint_handler!(images, Endpoint::Images);
endpoint_handler!(moderations, Endpoint::Moderations);
endpoint_handler!(rerank, Endpoint::Rerank);
endpoint_handler!(speech, Endpoint::Speech);

/// Overall deadline for a request: the server's limit, or a shorter one the
/// client asked for with `request-timeout`.
pub fn request_deadline(rt: &Runtime, headers: &HeaderMap) -> Duration {
    let server = Duration::from_secs(rt.cfg.server.request_timeout_secs);
    let asked = ["x-gateway-request-timeout", "cf-aig-request-timeout"]
        .iter()
        .find_map(|h| headers.get(*h))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis);
    asked.map_or(server, |a| a.min(server).max(Duration::from_millis(1)))
}

pub async fn serve(app: Arc<AppState>, req: Request, endpoint: Endpoint) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut resp = match app.limiter.clone().try_acquire_owned() {
        Err(_) => {
            metrics::counter!("gateway_rejected_total", "reason" => "overloaded").increment(1);
            ApiError::overloaded().into_response()
        }
        Ok(permit) => {
            let guard = RequestGuard::new(app.clone(), request_id.clone(), permit);
            guard.update(|s| s.endpoint = endpoint.as_str());
            let deadline = request_deadline(&app.rt(), req.headers());
            let result = match tokio::time::timeout(deadline, handle(&app, req, &guard, endpoint)).await {
                Ok(r) => r,
                Err(_) => Err(ApiError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "server_error",
                    "gateway_timeout",
                    "the request did not complete within the gateway's time limit",
                )),
            };
            let mut r = finish(&guard, result);
            add_guardrail_header(&guard, r.headers_mut());
            r
        }
    };
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert("x-request-id", v.clone());
        resp.headers_mut().insert("x-gateway-generation-id", v);
    }
    resp
}

/// Records the outcome on the guard and turns errors into responses.
pub fn finish(guard: &RequestGuard, result: Result<Response, ApiError>) -> Response {
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

fn add_guardrail_header(guard: &RequestGuard, h: &mut HeaderMap) {
    let mut names = Vec::new();
    guard.update(|s| names = s.guardrails.clone());
    if !names.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&names.join(",")) {
            h.insert("x-gateway-guardrails", v);
        }
    }
}

/// `GET /v1/models`: routes this key may use, with prices when configured.
#[derive(serde::Deserialize, Default)]
pub struct ModelsQuery {
    /// Only this provider's models.
    pub provider: Option<String>,
}

/// One catalog model as an OpenAI `model` object, with its price.
pub fn catalog_entry(app: &AppState, rt: &Runtime, provider: &str, m: &crate::catalog::ModelInfo) -> Value {
    let found = rt
        .providers
        .get(provider)
        .and_then(|p| app.pricing.lookup(&rt.cfg.pricing, provider, &p.cfg, &direct_target(provider, &m.id)));
    let (input, output, cached, source) = match (found, m.input_per_mtok, m.output_per_mtok) {
        (Some((p, src)), _, _) => {
            (Some(p.input_per_mtok), Some(p.output_per_mtok), p.cached_input_per_mtok, Some(src))
        }
        (None, Some(i), Some(o)) => (Some(i), Some(o), None, Some("provider")),
        _ => (None, None, None, None),
    };
    let mut v = json!({
        "id": format!("{provider}/{}", m.id),
        "object": "model",
        "created": m.created.unwrap_or(0),
        "owned_by": provider,
        "pricing": {"input_per_mtok": input, "output_per_mtok": output, "cached_input_per_mtok": cached},
    });
    if let Some(n) = &m.name {
        v["name"] = json!(n);
    }
    if let Some(c) = m.context_length {
        v["context_length"] = json!(c);
    }
    if let Some(s) = source {
        v["pricing"]["source"] = json!(s);
    }
    v
}

/// `GET /v1/models`: the routes this key may use, then every model of every
/// provider that has a key, as `provider/model`, from the providers' own
/// model lists (cached for an hour).
pub async fn list_models(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<ModelsQuery>,
) -> Result<Json<Value>, ApiError> {
    let tenant = app.keys.authenticate(&headers)?;
    let rt = app.rt();
    let mut data: Vec<Value> = Vec::new();
    if q.provider.is_none() {
        let mut ids: Vec<&String> = rt.routes.keys().filter(|r| tenant.allows_route(r)).collect();
        ids.sort();
        for id in ids {
            let r = &rt.routes[id];
            let price = target_price(&app, &rt, &r.targets[0]);
            data.push(json!({
                "id": id, "object": "model", "created": 0, "owned_by": "gateway",
                "targets": r.targets.iter().map(|t| format!("{}/{}", t.provider, t.model)).collect::<Vec<_>>(),
                "pricing": {
                    "input_per_mtok": price.map(|p| p.input_per_mtok),
                    "output_per_mtok": price.map(|p| p.output_per_mtok),
                    "cached_input_per_mtok": price.and_then(|p| p.cached_input_per_mtok),
                },
            }));
        }
    }
    for list in crate::catalog::Catalog::lists(&app, false).await {
        if q.provider.as_deref().is_some_and(|p| p != list.provider) {
            continue;
        }
        for m in &list.models {
            if tenant.allows_direct(&list.provider, &m.id) {
                data.push(catalog_entry(&app, &rt, &list.provider, m));
            }
        }
    }
    Ok(Json(json!({"object": "list", "data": data})))
}

/// Authentication and the per-key concurrency slot, before any body is read,
/// so unauthenticated clients cannot make the gateway buffer data.
pub fn claim(app: &AppState, headers: &HeaderMap, guard: &RequestGuard) -> Result<Arc<Tenant>, ApiError> {
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
    Ok(tenant)
}

/// Gateway options and tracing context for a request. Returns the options
/// and whether zero data retention applies.
pub fn apply_options(
    headers: &HeaderMap,
    body: Option<&mut Value>,
    tenant: &Tenant,
    guard: &RequestGuard,
) -> Result<(Arc<RequestOptions>, bool), ApiError> {
    let opts = Arc::new(RequestOptions::parse(headers, body)?);
    let zdr = tenant.zero_data_retention || opts.zero_data_retention;
    let trace =
        headers.get("traceparent").and_then(|v| v.to_str().ok()).and_then(crate::export::parse_traceparent);
    guard.update(|s| {
        s.options = Some(opts.clone());
        s.zero_data_retention = zdr;
        s.trace = trace;
    });
    Ok((opts, zdr))
}

async fn read_json(body: Body, max: usize) -> Result<Value, ApiError> {
    let bytes = axum::body::to_bytes(body, max).await.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request_too_large",
            format!("request body exceeds {max} bytes or could not be read"),
        )
    })?;
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::bad_request(format!("request body is not valid JSON: {e}")))?;
    if !v.is_object() {
        return Err(ApiError::bad_request("request body must be a JSON object"));
    }
    Ok(v)
}

async fn handle(
    app: &Arc<AppState>,
    req: Request,
    guard: &RequestGuard,
    endpoint: Endpoint,
) -> Result<Response, ApiError> {
    let rt = app.rt();
    let (parts, body) = req.into_parts();
    let headers = &parts.headers;

    let tenant = claim(app, headers, guard)?;
    let mut req = read_json(body, rt.cfg.server.max_body_bytes).await?;
    let (opts, zdr) = apply_options(headers, Some(&mut req), &tenant, guard)?;

    let model = req
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("'model' is required"))?
        .to_string();
    if !valid_model(&model) {
        return Err(ApiError::bad_request("'model' contains invalid characters"));
    }
    let field = endpoint.required_field();
    let has_field = match req.get(field) {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    };
    if !has_field {
        return Err(ApiError::bad_request(format!("'{field}' is required and must not be empty")));
    }
    let stream = endpoint.streams() && req.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let prompt_estimate = sse::estimate_prompt_tokens(&req);
    let client_wants_usage = req
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    guard.update(|s| {
        s.route = Some(model.clone());
        s.stream = stream;
    });

    let targets = plan_targets(app, &rt, &tenant, &model, &opts, endpoint, zdr)?;

    enforce_key_limits(app, &rt, &tenant).await?;
    let dims =
        Dims { tenant: &tenant.name, user: opts.user.as_deref(), model: &model, metadata: &opts.metadata };
    match limits::check(&app.store, &rt.cfg.limits, &dims, None, unix_secs()).await {
        Ok(applied) => guard.update(|s| s.limits = applied),
        Err(CheckError::Exceeded(x)) => return Err(limit_exceeded(&x.name, x.retry_after)),
        Err(CheckError::Store(e)) => on_store_error(&rt, "limits", e)?,
    }

    if !rt.guardrails.is_empty() {
        let outcome = rt.guardrails.check_request(&mut req);
        record_guardrail_hits(guard, &outcome.hits, "request");
        if let Some(rule) = outcome.blocked_by {
            return Err(content_blocked(format!("request blocked by guardrail '{rule}'")));
        }
        if rt.guardrails.has_moderation(true) {
            let outcome =
                rt.guardrails.moderate(app, &rt, &crate::guardrails::request_text(&req), true).await;
            record_guardrail_hits(guard, &outcome.hits, "request");
            if let Some(rule) = outcome.blocked_by {
                return Err(content_blocked(format!("request blocked by guardrail '{rule}'")));
            }
        }
    }
    if guard.capturing() {
        guard.capture_request(&serde_json::to_string(&req).unwrap_or_default());
    }

    let cache_key = (rt.cfg.cache.enabled && endpoint.cacheable() && !zdr && !opts.skip_cache).then(|| {
        match &opts.cache_key {
            Some(k) => custom_cache_key(&tenant.name, k),
            None => cache_key(&tenant.name, endpoint, &req, &targets, &opts),
        }
    });
    let cache_ttl = opts.cache_ttl.unwrap_or(rt.cfg.cache.ttl_secs);
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
                    guard.mark_first_byte();
                    guard.capture_response(&hit.body);
                    let ct = if hit.stream {
                        "text/event-stream".to_string()
                    } else {
                        hit.content_type.clone().unwrap_or_else(|| "application/json".into())
                    };
                    return Ok(body_response(
                        hit.body.into_bytes(),
                        &ct,
                        &hit.provider,
                        &hit.model,
                        Some("HIT"),
                    ));
                }
            }
            Ok(None) => metrics::counter!("gateway_cache_total", "result" => "miss").increment(1),
            Err(e) => {
                metrics::counter!("gateway_store_errors_total", "op" => "cache_get").increment(1);
                tracing::warn!(error = %e, "cache lookup failed; continuing without cache");
            }
        }
    }

    let ctx = AttemptCtx {
        app: app.as_ref(),
        rt: &rt,
        endpoint,
        req: &req,
        headers,
        stream,
        client_wants_usage,
        guard,
        cache_key: cache_key.as_deref(),
        cache_ttl,
        prompt_estimate,
        opts: &opts,
    };
    run_targets(&ctx, &tenant, &model, &targets).await
}

/// Tries each target in order with retries, per-provider limits, circuit
/// breakers and fallback, returning the first response to send.
async fn run_targets(
    ctx: &AttemptCtx<'_>,
    tenant: &Tenant,
    model: &str,
    targets: &[TargetConfig],
) -> Result<Response, ApiError> {
    let opts = ctx.opts;
    let mut last_error = ApiError::unavailable("no upstream provider is available for this model");
    for target in targets {
        let Some(provider) = ctx.rt.providers.get(&target.provider) else { continue };
        if !provider_ready(ctx.app, provider, Some(opts)) {
            last_error = ApiError::unavailable(format!(
                "provider '{}' has no API key; add one in the dashboard or its environment variable",
                provider.name
            ));
            continue;
        }
        if ctx.rt.cfg.limits.iter().any(|l| l.per == "provider") {
            let dims =
                Dims { tenant: &tenant.name, user: opts.user.as_deref(), model, metadata: &opts.metadata };
            match limits::check(&ctx.app.store, &ctx.rt.cfg.limits, &dims, Some(&provider.name), unix_secs())
                .await
            {
                Ok(applied) => ctx.guard.update(|s| s.limits.extend(applied)),
                Err(CheckError::Exceeded(x)) => {
                    last_error = limit_exceeded(&x.name, x.retry_after);
                    continue;
                }
                Err(CheckError::Store(e)) => on_store_error(ctx.rt, "limits", e)?,
            }
        }
        let attempts = opts.max_attempts.unwrap_or(provider.cfg.max_retries + 1).max(1);
        for attempt in 0..attempts {
            if attempt > 0 {
                tokio::time::sleep(retry_delay(attempt, opts.retry_delay, opts.backoff)).await;
            }
            if !provider.breaker.allow() {
                metrics::counter!("gateway_upstream_skipped_total", "provider" => provider.name.clone())
                    .increment(1);
                last_error =
                    ApiError::unavailable(format!("provider '{}' is temporarily unavailable", provider.name));
                break;
            }
            ctx.guard.update(|s| s.attempts += 1);
            let started = ctx.guard.elapsed_ms();
            let t0 = Instant::now();
            let outcome = try_target(ctx, provider, target).await;
            let (status, error) = match &outcome {
                Attempt::Done(r) => (r.status().as_u16(), None),
                Attempt::Retry(e) | Attempt::NextTarget(e) => (e.status.as_u16(), Some(e.message.clone())),
            };
            ctx.guard.update(|s| {
                s.attempt_log.push(AttemptLog {
                    provider: provider.name.clone(),
                    model: target.model.clone(),
                    status,
                    start_ms: started,
                    latency_ms: t0.elapsed().as_millis() as u64,
                    error,
                })
            });
            match outcome {
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

pub fn limit_exceeded(name: &str, retry_after: u64) -> ApiError {
    metrics::counter!("gateway_rejected_total", "reason" => "limit").increment(1);
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limit_error",
        "limit_exceeded",
        format!("limit '{name}' reached for this window"),
    )
    .with_retry_after(retry_after)
}

pub fn record_guardrail_hits(guard: &RequestGuard, hits: &[String], stage: &'static str) {
    if hits.is_empty() {
        return;
    }
    for h in hits {
        metrics::counter!("gateway_guardrail_hits_total", "rule" => h.clone(), "stage" => stage).increment(1);
    }
    guard.update(|s| {
        for h in hits {
            if !s.guardrails.contains(h) {
                s.guardrails.push(h.clone());
            }
        }
    });
}

pub fn content_blocked(message: String) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid_request_error", "content_blocked", message)
}

fn matches_condition(c: &Condition, value: Option<&str>, list: Option<&[String]>) -> bool {
    let any = |pred: &dyn Fn(&str) -> bool| match list {
        Some(items) => items.iter().any(|i| pred(i)),
        None => value.is_some_and(pred),
    };
    match c {
        Condition::Equals(e) => any(&|v| v == e),
        Condition::Op(op) => {
            if let Some(want) = op.exists {
                let present = list.map_or(value.is_some(), |l| !l.is_empty());
                if present != want {
                    return false;
                }
            }
            if let Some(e) = &op.eq {
                if !any(&|v| v == e) {
                    return false;
                }
            }
            if let Some(n) = &op.ne {
                if any(&|v| v == n) {
                    return false;
                }
            }
            if let Some(set) = &op.one_of {
                if !any(&|v| set.iter().any(|s| s == v)) {
                    return false;
                }
            }
            if let Some(set) = &op.not_in {
                if any(&|v| set.iter().any(|s| s == v)) {
                    return false;
                }
            }
            true
        }
    }
}

/// Picks the targets for a route: the first branch whose conditions all
/// match, else the route's own targets.
fn route_targets<'a>(
    route: &'a RouteConfig,
    tenant: &Tenant,
    opts: &RequestOptions,
) -> (RouteStrategy, &'a [TargetConfig]) {
    for b in &route.branches {
        let ok = b.when.iter().all(|(field, cond)| match field.as_str() {
            "key" => matches_condition(cond, Some(&tenant.name), None),
            "user" => matches_condition(cond, opts.user.as_deref(), None),
            "tags" => matches_condition(cond, None, Some(&opts.tags)),
            f => {
                let k = f.trim_start_matches("metadata.");
                matches_condition(cond, opts.metadata.get(k).map(String::as_str), None)
            }
        });
        if ok {
            return (b.strategy, &b.targets);
        }
    }
    (route.strategy, &route.targets)
}

/// The resolved price of a target (route prices, overrides, the price list).
pub fn target_price(app: &AppState, rt: &Runtime, t: &TargetConfig) -> Option<crate::pricing::Price> {
    let p = rt.providers.get(&t.provider)?;
    app.pricing.lookup(&rt.cfg.pricing, &t.provider, &p.cfg, t).map(|(p, _)| p)
}

/// Sort key for cheapest-first ordering; unknown prices sort last.
fn price(app: Option<&AppState>, t: &TargetConfig) -> f64 {
    let known = match (t.price_input_per_mtok, t.price_output_per_mtok) {
        (Some(i), Some(o)) => Some(i + o),
        _ => app.and_then(|a| target_price(a, &a.rt(), t)).map(|p| p.input_per_mtok + p.output_per_mtok),
    };
    known.unwrap_or(f64::MAX / 2.0)
}

/// Target order for a route. `fallback` keeps the configured order;
/// `weighted` picks the first target by weight (sticky per session id);
/// `latency` and `cost` sort by recent time to first token or price.
pub fn order_targets(
    app: Option<&AppState>,
    strategy: RouteStrategy,
    targets: &[TargetConfig],
    session: Option<&str>,
) -> Vec<TargetConfig> {
    let mut out = targets.to_vec();
    match strategy {
        RouteStrategy::Fallback => {}
        RouteStrategy::Cost => out.sort_by(|a, b| price(app, a).total_cmp(&price(app, b))),
        RouteStrategy::Latency => {
            if let Some(app) = app {
                // Targets with no measurement yet go first so they get one.
                let ttft = |t: &TargetConfig| {
                    app.stats
                        .get(&t.provider, &t.model)
                        .filter(|s| s.ttft_ms > 0.0)
                        .map_or(0.0, |s| s.ttft_ms)
                };
                out.sort_by(|a, b| ttft(a).total_cmp(&ttft(b)));
                explore(&mut out);
            }
        }
        RouteStrategy::Weighted => {
            if targets.len() < 2 {
                return out;
            }
            let weight = |t: &TargetConfig| u64::from(t.weight.unwrap_or(1));
            let total: u64 = targets.iter().map(weight).sum();
            if total == 0 {
                return out;
            }
            let mut pick = match session {
                Some(s) => {
                    let h = Sha256::digest(s.as_bytes());
                    u64::from_be_bytes(h[..8].try_into().unwrap_or([0; 8])) % total
                }
                None => rand::thread_rng().gen_range(0..total),
            };
            let mut first = 0;
            for (i, t) in targets.iter().enumerate() {
                let w = weight(t);
                if pick < w {
                    first = i;
                    break;
                }
                pick -= w;
            }
            out = Vec::with_capacity(targets.len());
            out.push(targets[first].clone());
            out.extend(targets.iter().enumerate().filter(|(i, _)| *i != first).map(|(_, t)| t.clone()));
        }
    }
    out
}

/// Performance-sorted orders occasionally (5%) try the runner-up first, so
/// measurements of slower targets stay fresh and a recovered target is noticed.
fn explore(targets: &mut [TargetConfig]) {
    if targets.len() > 1 && rand::thread_rng().gen_bool(0.05) {
        targets.swap(0, 1);
    }
}

fn direct_target(provider: &str, model: &str) -> TargetConfig {
    TargetConfig {
        provider: provider.to_string(),
        model: model.to_string(),
        price_input_per_mtok: None,
        price_output_per_mtok: None,
        weight: None,
    }
}

/// Resolves one requested model (after routing rules) to its targets.
fn resolve_model(
    app: &AppState,
    rt: &Runtime,
    tenant: &Tenant,
    model: &str,
    opts: &RequestOptions,
) -> Result<Vec<TargetConfig>, ApiError> {
    if let Some(route) = rt.routes.get(model) {
        if !tenant.allows_route(model) {
            return Err(ApiError::forbidden(format!("this API key may not use model '{model}'")));
        }
        let (strategy, targets) = route_targets(route, tenant, opts);
        return Ok(order_targets(Some(app), strategy, targets, opts.session_id.as_deref()));
    }
    if let Some((provider, upstream_model)) = model.split_once('/') {
        if let Some(p) = rt.providers.get(provider).filter(|p| provider_ready(app, p, Some(opts))) {
            if !tenant.allows_direct(provider, upstream_model) {
                return Err(ApiError::forbidden(format!("this API key may not use model '{model}'")));
            }
            if p.cfg.kind == ProviderKind::Azure && upstream_model.contains('/') {
                return Err(ApiError::bad_request("azure deployment names cannot contain '/'"));
            }
            return Ok(vec![direct_target(provider, upstream_model)]);
        }
    }
    // Anything else goes to the default provider (e.g. OpenRouter) with the
    // model name unchanged, so clients can pick any model it offers.
    if let Some(default) = &rt.cfg.default_provider {
        if !tenant.allows_direct(default, model) {
            return Err(ApiError::forbidden(format!("this API key may not use model '{model}'")));
        }
        return Ok(vec![direct_target(default, model)]);
    }
    Err(ApiError::model_not_found(model))
}

fn apply_rules(app: &AppState, model: &str) -> Result<String, ApiError> {
    match app.rules.apply(model) {
        Outcome::Model(m) => Ok(m),
        Outcome::Denied { rule_id, reason } => {
            metrics::counter!("gateway_rejected_total", "reason" => "routing_rule").increment(1);
            let msg = match reason {
                Some(r) => format!("request denied by a routing rule ({rule_id}): {r}"),
                None => format!("request denied by a routing rule ({rule_id})"),
            };
            Err(ApiError::new(StatusCode::FORBIDDEN, "permission_error", "model_denied", msg))
        }
        Outcome::Loop => Err(ApiError::bad_request(format!("routing rules for '{model}' form a loop"))),
    }
}

/// All targets for a request, in the order they will be tried: the
/// requested model's targets, then each fallback model's, filtered by what
/// the endpoint supports, `only`, ZDR and prompt-training requirements, and
/// reordered by `order` and `sort`.
pub fn plan_targets(
    app: &AppState,
    rt: &Runtime,
    tenant: &Tenant,
    model: &str,
    opts: &RequestOptions,
    endpoint: Endpoint,
    zdr: bool,
) -> Result<Vec<TargetConfig>, ApiError> {
    let primary = apply_rules(app, model)?;
    if primary != model {
        tracing::debug!(from = model, to = %primary, "model rewritten by a routing rule");
    }
    let mut targets = resolve_model(app, rt, tenant, &primary, opts)?;
    let mut fallback_error = None;
    for m in &opts.models {
        if !valid_model(m) {
            return Err(ApiError::bad_request(format!("fallback model '{m}' contains invalid characters")));
        }
        // A denied or unavailable fallback is skipped, not fatal.
        match apply_rules(app, m).and_then(|m| resolve_model(app, rt, tenant, &m, opts)) {
            Ok(t) => targets.extend(t),
            Err(e) => fallback_error = Some(e),
        }
    }
    let before = targets.len();
    targets.retain(|t| {
        let Some(p) = rt.providers.get(&t.provider) else { return false };
        p.supports(endpoint)
            && (opts.only.is_empty() || opts.only.contains(&t.provider))
            && (!zdr || p.cfg.zero_data_retention)
            && (!opts.disallow_prompt_training || p.cfg.no_prompt_training)
    });
    if targets.is_empty() {
        let why = if zdr {
            "no provider for this model has zero data retention enabled"
        } else if opts.disallow_prompt_training {
            "no provider for this model is marked as not training on prompts"
        } else if !opts.only.is_empty() {
            "none of the providers in 'only' serve this model"
        } else if before > 0 {
            "this model is not served by a provider that supports this endpoint"
        } else {
            return Err(fallback_error.unwrap_or_else(|| ApiError::model_not_found(model)));
        };
        return Err(ApiError::bad_request(format!("{why} ({} API)", endpoint.as_str())));
    }
    if !opts.order.is_empty() {
        let rank = |t: &TargetConfig| opts.order.iter().position(|p| p == &t.provider).unwrap_or(usize::MAX);
        targets.sort_by_key(rank);
    }
    if let Some(sort) = opts.sort {
        let metric = |t: &TargetConfig| -> f64 {
            let s = app.stats.get(&t.provider, &t.model);
            match sort {
                Sort::Cost => price(Some(app), t),
                // Unmeasured targets first, so they get measured.
                Sort::Latency => s.filter(|s| s.ttft_ms > 0.0).map_or(0.0, |s| s.ttft_ms),
                Sort::Throughput => {
                    s.filter(|s| s.tokens_per_sec > 0.0).map_or(f64::MIN, |s| -s.tokens_per_sec)
                }
            }
        };
        targets.sort_by(|a, b| metric(a).total_cmp(&metric(b)));
        if sort != Sort::Cost {
            explore(&mut targets);
        }
    }
    let mut seen = std::collections::HashSet::new();
    targets.retain(|t| seen.insert((t.provider.clone(), t.model.clone())));
    Ok(targets)
}

/// The per-key requests/minute and daily token budget from the key config.
pub async fn enforce_key_limits(app: &AppState, rt: &Runtime, tenant: &Tenant) -> Result<(), ApiError> {
    let now = unix_secs();
    if let Some(rpm) = tenant.requests_per_minute {
        let key = format!("rl:{}:{}", tenant.name, now / 60);
        match app.store.incr(&key, 1, 120).await {
            Ok(n) if n > rpm => {
                metrics::counter!("gateway_rejected_total", "reason" => "rate_limit").increment(1);
                return Err(ApiError::rate_limited(60 - now % 60));
            }
            Ok(_) => {}
            Err(e) => on_store_error(rt, "rate_limit", e)?,
        }
    }
    if let Some(budget) = tenant.daily_token_budget {
        match app.store.get_counter(&budget_key(&tenant.name)).await {
            Ok(used) if used >= budget => {
                metrics::counter!("gateway_rejected_total", "reason" => "budget").increment(1);
                return Err(ApiError::budget_exceeded(86_400 - now % 86_400));
            }
            Ok(_) => {}
            Err(e) => on_store_error(rt, "budget", e)?,
        }
    }
    Ok(())
}

pub fn on_store_error(rt: &Runtime, op: &'static str, e: StoreError) -> Result<(), ApiError> {
    metrics::counter!("gateway_store_errors_total", "op" => op).increment(1);
    tracing::error!(error = %e, op, "store unavailable");
    match rt.cfg.store.on_error {
        FailMode::Open => Ok(()),
        FailMode::Closed => {
            Err(ApiError::unavailable("rate limiting backend is unavailable, retry shortly")
                .with_retry_after(2))
        }
    }
}

/// Cache entries are scoped per tenant so one client can never read another
/// client's cached completion.
/// The key covers everything that changes the answer: the body, the
/// targets the request would be routed to (which depend on metadata,
/// `order`, `only`, fallbacks and rules) and any client-supplied provider key.
fn cache_key(
    tenant: &str,
    endpoint: Endpoint,
    req: &Value,
    targets: &[TargetConfig],
    opts: &RequestOptions,
) -> String {
    let mut h = Sha256::new();
    h.update(tenant.as_bytes());
    h.update([0u8]);
    h.update(endpoint.as_str().as_bytes());
    h.update([0u8]);
    h.update(serde_json::to_vec(req).unwrap_or_default());
    for t in targets {
        h.update([0u8]);
        h.update(t.provider.as_bytes());
        h.update(b"/");
        h.update(t.model.as_bytes());
    }
    for (p, k) in &opts.byok {
        h.update([1u8]);
        h.update(p.as_bytes());
        h.update(Sha256::digest(k.as_bytes()));
    }
    format!("cache:{}", hex::encode(h.finalize()))
}

fn custom_cache_key(tenant: &str, key: &str) -> String {
    let mut h = Sha256::new();
    h.update(tenant.as_bytes());
    h.update([0u8]);
    h.update(key.as_bytes());
    format!("cache:custom:{}", hex::encode(h.finalize()))
}

/// Delay before retry number `attempt` (1-based). Default: exponential
/// with jitter, ~250ms, 500ms, 1s, ... capped at 4s.
pub fn retry_delay(attempt: u32, base: Option<Duration>, backoff: Option<Backoff>) -> Duration {
    let base_ms = base.map(|d| d.as_millis() as u64).unwrap_or(250);
    let n = u64::from(attempt.max(1));
    let ms = match backoff {
        Some(Backoff::Constant) => base_ms,
        Some(Backoff::Linear) => base_ms.saturating_mul(n),
        Some(Backoff::Exponential) | None => base_ms.saturating_mul(1u64 << (n - 1).min(10)),
    };
    let cap = if base.is_some() || backoff.is_some() { 60_000 } else { 4_000 };
    let ms = ms.min(cap);
    if backoff.is_none() && base.is_none() {
        Duration::from_millis(rand::thread_rng().gen_range(ms / 2..=ms))
    } else {
        Duration::from_millis(ms)
    }
}

fn body_response(
    body: Vec<u8>,
    content_type: &str,
    provider: &str,
    model: &str,
    cache: Option<&str>,
) -> Response {
    let mut r = Response::new(Body::from(body));
    let h = r.headers_mut();
    if let Ok(ct) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, ct);
    }
    if content_type.starts_with("text/event-stream") {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    set_gateway_headers(h, provider, model);
    if let Some(c) = cache.and_then(|c| HeaderValue::from_str(c).ok()) {
        h.insert("x-gateway-cache", c.clone());
        h.insert("cf-aig-cache-status", c);
    }
    r
}

pub fn set_gateway_headers(h: &mut HeaderMap, provider: &str, model: &str) {
    if let Ok(v) = HeaderValue::from_str(provider) {
        h.insert("x-gateway-provider", v);
    }
    if let Ok(v) = HeaderValue::from_str(model) {
        h.insert("x-gateway-model", v);
    }
}

pub struct AttemptCtx<'a> {
    pub app: &'a AppState,
    pub rt: &'a Runtime,
    pub endpoint: Endpoint,
    pub req: &'a Value,
    pub headers: &'a HeaderMap,
    pub stream: bool,
    pub client_wants_usage: bool,
    pub guard: &'a RequestGuard,
    pub cache_key: Option<&'a str>,
    pub cache_ttl: u64,
    pub prompt_estimate: u64,
    pub opts: &'a RequestOptions,
}

pub enum Attempt {
    /// A response to return to the client (success or a client error).
    Done(Response),
    /// Transient failure; retry this provider if attempts remain.
    Retry(ApiError),
    /// This provider cannot serve the request; move to the next target.
    NextTarget(ApiError),
}

/// The provider key to use: a key the client sent for this provider
/// (request-scoped BYOK), a key stored through the admin API, or the key in
/// the config file.
/// Whether a provider can be called: it has a key in the config or the
/// environment, uses cloud credentials (Bedrock, Vertex), has a key stored in
/// the dashboard, or the request brings one. Built-in providers without any
/// of these are skipped, and their models go to `default_provider`.
pub fn provider_ready(app: &AppState, p: &Provider, opts: Option<&RequestOptions>) -> bool {
    !p.cfg.api_key.expose().is_empty()
        || matches!(p.cfg.kind, ProviderKind::Bedrock | ProviderKind::Vertex)
        || app.stored_provider_keys.read().contains_key(&p.name)
        || opts.is_some_and(|o| p.cfg.allow_client_keys && o.byok.contains_key(&p.name))
}

pub fn credentials(app: &AppState, provider: &Provider, opts: &RequestOptions) -> (String, &'static str) {
    if provider.cfg.allow_client_keys {
        if let Some(k) = opts.byok.get(&provider.name) {
            return (k.clone(), "request");
        }
    }
    if let Some(k) = app.stored_provider_keys.read().get(&provider.name) {
        return (k.expose().to_string(), "stored");
    }
    (provider.cfg.api_key.expose().to_string(), "config")
}

/// Marks a provider failure, alerting when it opens the circuit.
pub fn provider_failed(app: &AppState, provider: &Provider) {
    let was_open = provider.breaker.is_open();
    provider.breaker.record_failure();
    if !was_open && provider.breaker.is_open() {
        crate::alerts::circuit_opened(app, &provider.name);
    }
}

pub fn upstream_error_metric(provider: &str, kind: &str) {
    metrics::counter!(
        "gateway_upstream_errors_total",
        "provider" => provider.to_string(),
        "kind" => kind.to_string()
    )
    .increment(1);
}

async fn try_target(ctx: &AttemptCtx<'_>, provider: &Provider, target: &TargetConfig) -> Attempt {
    let pname = provider.name.clone();
    let (key, credential) = credentials(ctx.app, provider, ctx.opts);
    let auto_cache = ctx.opts.auto_cache;
    let built = match ctx.endpoint {
        Endpoint::Chat => provider.build(ctx.req, &target.model, ctx.stream, &key, auto_cache),
        Endpoint::Messages => provider.build_messages(ctx.req, &target.model, &key, ctx.headers, auto_cache),
        ep => provider.build_json(ep.path(), ctx.req, &target.model, &key),
    };
    let rb = match built {
        Ok(rb) => rb,
        // Translation errors are the client's request being unsupported.
        // Report a neutral outcome so a half-open probe slot is released.
        Err(e) => {
            provider.breaker.record_neutral();
            return Attempt::Done(e.into_response());
        }
    };

    let started = Instant::now();
    let resp = match tokio::time::timeout(provider.timeout(), provider.send(rb, &key)).await {
        Err(_) => {
            provider_failed(ctx.app, provider);
            upstream_error_metric(&pname, "timeout");
            tracing::warn!(request_id = ctx.guard.request_id(), provider = %pname, "upstream timed out");
            return Attempt::Retry(ApiError::upstream_timeout());
        }
        Ok(Err(e)) => {
            provider_failed(ctx.app, provider);
            upstream_error_metric(&pname, "transport");
            tracing::warn!(request_id = ctx.guard.request_id(), provider = %pname, error = %e,
                "upstream request failed");
            return Attempt::Retry(ApiError::upstream("upstream provider request failed"));
        }
        Ok(Ok(r)) => r,
    };

    let status = resp.status();
    if status.is_success() {
        provider.breaker.record_success();
        ctx.guard.update(|s| {
            s.provider = Some(pname.clone());
            s.model = Some(target.model.clone());
            s.price = target_price(ctx.app, &ctx.app.rt(), target);
            s.credential = Some(credential);
        });
        let sink = ctx.cache_key.map(|k| CacheSink {
            key: k.to_string(),
            ttl_secs: ctx.cache_ttl,
            max_bytes: ctx.rt.cfg.cache.max_entry_bytes,
            provider: pname.clone(),
            model: target.model.clone(),
        });
        if ctx.stream {
            ctx.guard.update(|s| {
                // The upstream has accepted the prompt. Bill it even if the
                // client disconnects before the first chunk arrives.
                s.usage = Usage { prompt_tokens: ctx.prompt_estimate, ..Default::default() };
            });
            let cancel = ctx.app.stream_cancel.subscribe();
            let body = if ctx.endpoint == Endpoint::Chat {
                let translator = provider.stream_translator(&target.model, ctx.client_wants_usage);
                Body::from_stream(sse::event_stream(
                    resp,
                    translator,
                    provider.idle_timeout(),
                    ctx.prompt_estimate,
                    cancel,
                    ctx.guard.clone(),
                    sink,
                ))
            } else {
                Body::from_stream(sse::raw_stream(
                    resp,
                    provider.idle_timeout(),
                    ctx.prompt_estimate,
                    cancel,
                    ctx.guard.clone(),
                    sink,
                ))
            };
            let mut r = Response::new(body);
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            h.insert("x-accel-buffering", HeaderValue::from_static("no"));
            set_gateway_headers(h, &pname, &target.model);
            return Attempt::Done(r);
        }

        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let remaining = provider.timeout().saturating_sub(started.elapsed());
        let bytes = match tokio::time::timeout(remaining, resp.bytes()).await {
            Err(_) => {
                provider_failed(ctx.app, provider);
                upstream_error_metric(&pname, "timeout");
                return Attempt::Retry(ApiError::upstream_timeout());
            }
            Ok(Err(e)) => {
                provider_failed(ctx.app, provider);
                upstream_error_metric(&pname, "transport");
                tracing::warn!(request_id = ctx.guard.request_id(), provider = %pname, error = %e,
                    "reading upstream body failed");
                return Attempt::Retry(ApiError::upstream("upstream provider request failed"));
            }
            Ok(Ok(b)) => b,
        };
        ctx.guard.mark_first_byte();
        let json_body = content_type.starts_with("application/json");
        let (out, usage) = if ctx.endpoint == Endpoint::Chat {
            match provider.translate_response(&bytes, &target.model) {
                Ok(x) => x,
                Err(e) => {
                    provider_failed(ctx.app, provider);
                    upstream_error_metric(&pname, "invalid_response");
                    return Attempt::Retry(e);
                }
            }
        } else {
            let usage = if json_body {
                serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|v| Usage::sniff(&v))
                    .unwrap_or_default()
            } else {
                Usage::default()
            };
            (bytes.to_vec(), usage)
        };
        ctx.guard.update(|s| s.usage = usage);

        let out = if ctx.rt.guardrails.has_response_rules() && json_body {
            match apply_response_guardrails(ctx, out).await {
                Ok(o) => o,
                Err(e) => {
                    let mut r = e.into_response();
                    set_gateway_headers(r.headers_mut(), &pname, &target.model);
                    return Attempt::Done(r);
                }
            }
        } else {
            out
        };
        if ctx.guard.capturing() && json_body {
            ctx.guard.capture_response(&String::from_utf8_lossy(&out));
        }
        if let Some(sink) = sink {
            store_in_cache(ctx.app, sink, &out, &content_type).await;
        }
        let ct = if ctx.endpoint == Endpoint::Chat { "application/json" } else { content_type.as_str() };
        return Attempt::Done(body_response(out, ct, &pname, &target.model, ctx.cache_key.map(|_| "MISS")));
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
    upstream_status_outcome(ctx.app, provider, code, &body, credential, &target.model)
}

/// What to do with an upstream error status: return it, retry, or fall back.
pub fn upstream_status_outcome(
    app: &AppState,
    provider: &Provider,
    code: u16,
    body: &[u8],
    credential: &str,
    model: &str,
) -> Attempt {
    let pname = &provider.name;
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
    match code {
        // The request itself is bad; another provider would reject it too.
        // Neutral, not success: steady client errors must not hide a
        // provider that is otherwise failing.
        400 | 413 | 422 => {
            provider.breaker.record_neutral();
            let mut r = (status, Json(provider.translate_error(code, body))).into_response();
            set_gateway_headers(r.headers_mut(), pname, model);
            Attempt::Done(r)
        }
        // A key the client brought is wrong: tell them, don't fall back.
        401 | 403 if credential == "request" => {
            provider.breaker.record_neutral();
            Attempt::Done(
                ApiError::new(
                    status,
                    "authentication_error",
                    "provider_key_rejected",
                    format!("provider '{pname}' rejected the API key sent in providerOptions.gateway.byok"),
                )
                .into_response(),
            )
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
        300..=399 => {
            provider.breaker.record_neutral();
            tracing::error!(provider = %pname, status = code, "provider redirected the request; fix its base_url");
            Attempt::NextTarget(ApiError::upstream("upstream provider is misconfigured"))
        }
        // Usually a wrong deployment name or base_url for this provider.
        404 => {
            provider.breaker.record_neutral();
            tracing::error!(provider = %pname, model, "provider returned 404; check the model name and base_url");
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
            provider_failed(app, provider);
            Attempt::Retry(ApiError::upstream(format!("upstream provider returned {code}")))
        }
        _ => {
            provider.breaker.record_neutral();
            Attempt::Retry(ApiError::upstream(format!("upstream provider returned {code}")))
        }
    }
}

/// Runs response guardrails on a non-streaming response. Tokens are already
/// billed; a blocked response is replaced with an error.
async fn apply_response_guardrails(ctx: &AttemptCtx<'_>, out: Vec<u8>) -> Result<Vec<u8>, ApiError> {
    let Ok(mut v) = serde_json::from_slice::<Value>(&out) else { return Ok(out) };
    let outcome = ctx.rt.guardrails.check_response(&mut v);
    record_guardrail_hits(ctx.guard, &outcome.hits, "response");
    if let Some(rule) = outcome.blocked_by {
        return Err(content_blocked(format!("response blocked by guardrail '{rule}'")));
    }
    if ctx.rt.guardrails.has_moderation(false) {
        let text = crate::guardrails::response_text(&v);
        let m = ctx.rt.guardrails.moderate(ctx.app, ctx.rt, &text, false).await;
        record_guardrail_hits(ctx.guard, &m.hits, "response");
        if let Some(rule) = m.blocked_by {
            return Err(content_blocked(format!("response blocked by guardrail '{rule}'")));
        }
    }
    if outcome.hits.is_empty() {
        return Ok(out);
    }
    Ok(serde_json::to_vec(&v).unwrap_or(out))
}

async fn store_in_cache(app: &AppState, sink: CacheSink, body: &[u8], content_type: &str) {
    if body.len() > sink.max_bytes {
        return;
    }
    let Ok(body) = String::from_utf8(body.to_vec()) else { return };
    let entry = CachedResponse {
        provider: sink.provider,
        model: sink.model,
        body,
        stream: false,
        content_type: Some(content_type.to_string()),
    };
    let Ok(raw) = serde_json::to_vec(&entry) else { return };
    if let Err(e) = app.store.cache_set(&sink.key, &raw, sink.ttl_secs).await {
        metrics::counter!("gateway_store_errors_total", "op" => "cache_set").increment(1);
        tracing::warn!(error = %e, "failed to write response cache");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConditionOp;

    #[test]
    fn retry_delays() {
        for _ in 0..50 {
            let b1 = retry_delay(1, None, None).as_millis();
            assert!((125..=250).contains(&b1));
            let b10 = retry_delay(10, None, None).as_millis();
            assert!((2000..=4000).contains(&b10));
        }
        let d = Some(Duration::from_millis(100));
        assert_eq!(retry_delay(3, d, Some(Backoff::Constant)).as_millis(), 100);
        assert_eq!(retry_delay(3, d, Some(Backoff::Linear)).as_millis(), 300);
        assert_eq!(retry_delay(3, d, Some(Backoff::Exponential)).as_millis(), 400);
    }

    #[test]
    fn cache_keys_are_tenant_scoped() {
        let req = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let t1 = vec![direct_target("p", "m")];
        let t2 = vec![direct_target("q", "m")];
        let o = RequestOptions::default();
        let k = |tenant: &str, ep: Endpoint, t: &[TargetConfig]| cache_key(tenant, ep, &req, t, &o);
        assert_ne!(k("a", Endpoint::Chat, &t1), k("b", Endpoint::Chat, &t1));
        assert_ne!(k("a", Endpoint::Chat, &t1), k("a", Endpoint::Embeddings, &t1));
        assert_ne!(k("a", Endpoint::Chat, &t1), k("a", Endpoint::Chat, &t2), "routing changes the key");
        assert_eq!(k("a", Endpoint::Chat, &t1), k("a", Endpoint::Chat, &t1));
        let mut byok = RequestOptions::default();
        byok.byok.insert("p".into(), "sk".into());
        assert_ne!(cache_key("a", Endpoint::Chat, &req, &t1, &byok), k("a", Endpoint::Chat, &t1));
        assert_ne!(custom_cache_key("a", "k"), custom_cache_key("b", "k"));
    }

    fn target(model: &str, weight: Option<u32>) -> TargetConfig {
        TargetConfig { weight, ..direct_target("p", model) }
    }

    #[test]
    fn weighted_routes_split_traffic_and_keep_fallbacks() {
        let targets = vec![target("a", Some(3)), target("b", Some(1)), target("c", Some(0))];
        let mut firsts = std::collections::HashMap::new();
        for _ in 0..4000 {
            let order = order_targets(None, RouteStrategy::Weighted, &targets, None);
            assert_eq!(order.len(), 3);
            *firsts.entry(order[0].model.clone()).or_insert(0) += 1;
            assert_eq!(order.last().unwrap().model, "c", "weight 0 is fallback only");
        }
        assert!(!firsts.contains_key("c"));
        let a = firsts["a"] as f64 / 4000.0;
        assert!((0.68..0.82).contains(&a), "a got {a}");

        // A session id always lands on the same first target.
        let s1 = order_targets(None, RouteStrategy::Weighted, &targets, Some("session-1"))[0].model.clone();
        for _ in 0..20 {
            assert_eq!(
                order_targets(None, RouteStrategy::Weighted, &targets, Some("session-1"))[0].model,
                s1
            );
        }

        let order: Vec<String> = order_targets(None, RouteStrategy::Fallback, &targets, None)
            .into_iter()
            .map(|t| t.model)
            .collect();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn cost_strategy_sorts_by_price() {
        let mut cheap = target("cheap", None);
        cheap.price_input_per_mtok = Some(0.1);
        cheap.price_output_per_mtok = Some(0.2);
        let mut pricey = target("pricey", None);
        pricey.price_input_per_mtok = Some(3.0);
        pricey.price_output_per_mtok = Some(15.0);
        let order = order_targets(None, RouteStrategy::Cost, &[target("unknown", None), pricey, cheap], None);
        assert_eq!(
            order.iter().map(|t| t.model.as_str()).collect::<Vec<_>>(),
            vec!["cheap", "pricey", "unknown"]
        );
    }

    #[test]
    fn conditions() {
        let eq = Condition::Equals("free".into());
        assert!(matches_condition(&eq, Some("free"), None));
        assert!(!matches_condition(&eq, Some("pro"), None));
        assert!(!matches_condition(&eq, None, None));
        let op =
            Condition::Op(ConditionOp { one_of: Some(vec!["a".into(), "b".into()]), ..Default::default() });
        assert!(matches_condition(&op, Some("b"), None));
        let tags = vec!["x".into(), "beta".into()];
        assert!(matches_condition(&Condition::Equals("beta".into()), None, Some(&tags)));
        let missing = Condition::Op(ConditionOp { exists: Some(false), ..Default::default() });
        assert!(matches_condition(&missing, None, None));
        assert!(!matches_condition(&missing, Some("v"), None));
    }
}
