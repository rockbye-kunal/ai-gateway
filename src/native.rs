//! Provider-native passthrough: `/native/<provider>/<any path>`.
//!
//! Sends the request to `<provider base_url>/<path>` in the provider's own
//! format (any method, any endpoint: Anthropic Messages with every beta,
//! OpenAI Batch or Files, Gemini, and so on), with the provider's key added by
//! the gateway. Gateway features still apply: API keys and permissions,
//! limits and budgets, routing deny rules, guardrails on JSON bodies, ZDR,
//! retries on the same provider, usage and cost tracking, and the request log.
//! Responses (including streams) come back unchanged.

use std::{sync::Arc, time::Instant};

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::{
    app::AppState,
    error::ApiError,
    limits::{self, CheckError, Dims},
    logs::AttemptLog,
    proxy::{
        apply_options, claim, content_blocked, credentials, enforce_key_limits, finish, limit_exceeded,
        on_store_error, provider_failed, record_guardrail_hits, request_deadline, retry_delay,
        set_gateway_headers, unix_secs, upstream_error_metric,
    },
    rules::Outcome,
    sse,
    usage::{RequestGuard, Usage},
};

pub async fn handler(
    State(app): State<Arc<AppState>>,
    Path((provider, path)): Path<(String, String)>,
    req: Request,
) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut resp = match app.limiter.clone().try_acquire_owned() {
        Err(_) => ApiError::overloaded().into_response(),
        Ok(permit) => {
            let guard = RequestGuard::new(app.clone(), request_id.clone(), permit);
            guard.update(|s| s.endpoint = "native");
            let deadline = request_deadline(&app.rt(), req.headers());
            let result =
                match tokio::time::timeout(deadline, handle(&app, &provider, &path, req, &guard)).await {
                    Ok(r) => r,
                    Err(_) => Err(ApiError::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "server_error",
                        "gateway_timeout",
                        "the request did not complete within the gateway's time limit",
                    )),
                };
            finish(&guard, result)
        }
    };
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

async fn handle(
    app: &Arc<AppState>,
    provider_name: &str,
    path: &str,
    req: Request,
    guard: &RequestGuard,
) -> Result<Response, ApiError> {
    let rt = app.rt();
    let (parts, body) = req.into_parts();
    let headers = &parts.headers;
    let tenant = claim(app, headers, guard)?;
    let provider = rt
        .providers
        .get(provider_name)
        .cloned()
        .ok_or_else(|| ApiError::model_not_found(&format!("{provider_name}/*")))?;
    if path.split('/').any(|seg| seg == "..") {
        return Err(ApiError::bad_request("path must not contain '..'"));
    }
    let mut bytes = axum::body::to_bytes(body, rt.cfg.server.max_body_bytes).await.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request_too_large",
            "request body too large",
        )
    })?;

    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/json"));
    let mut json: Option<Value> = if is_json && !bytes.is_empty() {
        Some(
            serde_json::from_slice(&bytes)
                .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?,
        )
    } else {
        None
    };
    let had_options = json.as_ref().is_some_and(|v| v.get("providerOptions").is_some());
    let (opts, zdr) = apply_options(headers, json.as_mut(), &tenant, guard)?;
    let model = json.as_ref().and_then(|v| v.get("model")).and_then(Value::as_str).map(str::to_string);

    let allowed = match &model {
        Some(m) => tenant.allows_direct(provider_name, m),
        None => tenant.allows_direct(provider_name, "*"),
    };
    if !allowed {
        return Err(ApiError::forbidden(format!(
            "this API key may not use provider '{provider_name}' directly{}",
            model.as_ref().map(|m| format!(" with model '{m}'")).unwrap_or_default()
        )));
    }
    if let Some(m) = &model {
        let full = format!("{provider_name}/{m}");
        match app.rules.apply(&full) {
            Outcome::Denied { rule_id, .. } => {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "permission_error",
                    "model_denied",
                    format!("request denied by a routing rule ({rule_id})"),
                ))
            }
            Outcome::Model(new) if new != full => {
                // Rewrites only apply within the same provider here: the body
                // is in this provider's own format.
                if let Some(rest) = new.strip_prefix(&format!("{provider_name}/")) {
                    if let Some(v) = json.as_mut() {
                        v["model"] = Value::String(rest.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if zdr && !provider.cfg.zero_data_retention {
        return Err(ApiError::bad_request(format!(
            "provider '{provider_name}' does not have zero data retention enabled"
        )));
    }
    let route_model = model.clone().unwrap_or_else(|| "-".into());
    guard.update(|s| {
        s.route = Some(format!("{provider_name}/{route_model}"));
        s.provider = Some(provider_name.to_string());
        s.model = model.clone();
    });

    enforce_key_limits(app, &rt, &tenant).await?;
    let dims = Dims {
        tenant: &tenant.name,
        user: opts.user.as_deref(),
        model: &route_model,
        metadata: &opts.metadata,
    };
    for p in [None, Some(provider_name)] {
        match limits::check(&app.store, &rt.cfg.limits, &dims, p, unix_secs()).await {
            Ok(applied) => guard.update(|s| s.limits.extend(applied)),
            Err(CheckError::Exceeded(x)) => return Err(limit_exceeded(&x.name, x.retry_after)),
            Err(CheckError::Store(e)) => on_store_error(&rt, "limits", e)?,
        }
    }

    let mut changed = had_options;
    if let Some(v) = json.as_mut() {
        if !rt.guardrails.is_empty() {
            let outcome = rt.guardrails.check_request(v);
            record_guardrail_hits(guard, &outcome.hits, "request");
            if let Some(rule) = outcome.blocked_by {
                return Err(content_blocked(format!("request blocked by guardrail '{rule}'")));
            }
            changed |= !outcome.hits.is_empty();
            if rt.guardrails.has_moderation(true) {
                let m = rt.guardrails.moderate(app, &rt, &crate::guardrails::request_text(v), true).await;
                record_guardrail_hits(guard, &m.hits, "request");
                if let Some(rule) = m.blocked_by {
                    return Err(content_blocked(format!("request blocked by guardrail '{rule}'")));
                }
            }
        }
        changed |= model.as_deref() != v.get("model").and_then(Value::as_str);
        if changed {
            bytes = serde_json::to_vec(v).unwrap_or_default().into();
        }
    }
    if guard.capturing() {
        guard.capture_request(&String::from_utf8_lossy(&bytes));
    }

    let stream = json.as_ref().and_then(|v| v.get("stream")).and_then(Value::as_bool).unwrap_or(false)
        || path.contains("stream")
        || headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("text/event-stream"));
    guard.update(|s| s.stream = stream);
    let prompt_estimate = json.as_ref().map(sse::estimate_prompt_tokens).unwrap_or(0);
    let path_and_query = match parts.uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };

    let (key, credential) = credentials(app, &provider, &opts);
    let attempts = opts.max_attempts.unwrap_or(provider.cfg.max_retries + 1).max(1);
    let mut last_error =
        ApiError::unavailable(format!("provider '{provider_name}' is temporarily unavailable"));
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(retry_delay(attempt, opts.retry_delay, opts.backoff)).await;
        }
        if !provider.breaker.allow() {
            break;
        }
        guard.update(|s| s.attempts += 1);
        let started_ms = guard.elapsed_ms();
        let t0 = Instant::now();
        let rb =
            provider.build_native(parts.method.clone(), &path_and_query, bytes.clone(), &key, headers)?;
        let log_attempt = |status: u16, error: Option<String>| {
            guard.update(|s| {
                s.attempt_log.push(AttemptLog {
                    provider: provider_name.to_string(),
                    model: route_model.clone(),
                    status,
                    start_ms: started_ms,
                    latency_ms: t0.elapsed().as_millis() as u64,
                    error,
                })
            })
        };
        let resp = match tokio::time::timeout(provider.timeout(), provider.send(rb, &key)).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) | Err(_) => {
                provider_failed(app, &provider);
                upstream_error_metric(provider_name, "transport");
                last_error = ApiError::upstream("upstream provider request failed");
                log_attempt(502, Some(last_error.message.clone()));
                continue;
            }
        };
        let status = resp.status();
        let code = status.as_u16();
        if code == 429 || status.is_server_error() {
            if status.is_server_error() {
                provider_failed(app, &provider);
            }
            upstream_error_metric(provider_name, &code.to_string());
            last_error = ApiError::upstream(format!("upstream provider returned {code}"));
            log_attempt(code, Some(last_error.message.clone()));
            continue;
        }
        if (code == 401 || code == 403) && credential != "request" {
            provider.breaker.record_neutral();
            log_attempt(code, Some("provider rejected the gateway's credentials".into()));
            tracing::error!(provider = %provider_name, status = code, "provider rejected the gateway's credentials");
            return Err(ApiError::upstream("upstream provider rejected the gateway's credentials"));
        }
        if status.is_success() {
            provider.breaker.record_success();
        } else {
            provider.breaker.record_neutral();
        }
        log_attempt(code, None);
        guard.update(|s| s.credential = Some(credential));

        let ct = resp.headers().get(header::CONTENT_TYPE).cloned();
        let event_stream =
            ct.as_ref().and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("text/event-stream"));
        let mut out = if event_stream && status.is_success() {
            guard.update(|s| s.usage = Usage { prompt_tokens: prompt_estimate, ..Default::default() });
            let body = sse::raw_stream(
                resp,
                provider.idle_timeout(),
                prompt_estimate,
                app.stream_cancel.subscribe(),
                guard.clone(),
                None,
            );
            let mut r = Response::new(Body::from_stream(body));
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            r.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
            r
        } else {
            let remaining = provider.timeout().saturating_sub(t0.elapsed());
            let body = match tokio::time::timeout(remaining, resp.bytes()).await {
                Ok(Ok(b)) => b,
                _ => {
                    provider_failed(app, &provider);
                    return Err(ApiError::upstream_timeout());
                }
            };
            guard.mark_first_byte();
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                if let Some(u) = Usage::sniff(&v) {
                    guard.update(|s| s.usage = u);
                }
                if guard.capturing() {
                    guard.capture_response(&String::from_utf8_lossy(&body));
                }
            }
            Response::new(Body::from(body))
        };
        *out.status_mut() = status;
        if let Some(ct) = ct {
            out.headers_mut().insert(header::CONTENT_TYPE, ct);
        }
        set_gateway_headers(out.headers_mut(), provider_name, &route_model);
        return Ok(out);
    }
    Err(last_error)
}
