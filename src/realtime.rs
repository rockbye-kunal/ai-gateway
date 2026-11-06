//! Realtime API over WebSockets: `GET /v1/realtime?model=<model>`.
//!
//! The gateway authenticates the client (an `Authorization` header, or the
//! `openai-insecure-api-key.<key>` subprotocol browsers use), resolves the
//! model like any other request (routes, rules, permissions, limits), opens a
//! WebSocket to the provider with the provider's key, and relays frames both
//! ways. Token usage from each `response.done` event is added up and billed
//! when the session ends; the whole session is one entry in the request log.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

use crate::{
    app::AppState,
    config::ProviderKind,
    error::ApiError,
    limits::{self, CheckError, Dims},
    proxy::{
        apply_options, claim, credentials, enforce_key_limits, finish, limit_exceeded, on_store_error,
        plan_targets, unix_secs, Endpoint,
    },
    usage::{RequestGuard, Usage},
};

const KEY_PROTOCOL: &str = "openai-insecure-api-key.";

#[derive(serde::Deserialize)]
pub struct RealtimeQuery {
    model: Option<String>,
}

/// Turns the browser subprotocol key into an Authorization header.
fn auth_headers(headers: &HeaderMap) -> HeaderMap {
    let mut h = headers.clone();
    if h.get("authorization").is_none() && h.get("x-api-key").is_none() {
        let key = headers
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .and_then(|p| p.split(',').map(str::trim).find_map(|p| p.strip_prefix(KEY_PROTOCOL)));
        if let Some(Ok(v)) = key.map(|k| HeaderValue::from_str(&format!("Bearer {k}"))) {
            h.insert("authorization", v);
        }
    }
    h
}

pub async fn handler(
    State(app): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
    Query(q): Query<RealtimeQuery>,
    headers: HeaderMap,
) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let Ok(permit) = app.limiter.clone().try_acquire_owned() else {
        return ApiError::overloaded().into_response();
    };
    let guard = RequestGuard::new(app.clone(), request_id.clone(), permit);
    guard.update(|s| {
        s.endpoint = "realtime";
        s.stream = true;
    });
    match connect(&app, &headers, q.model, &guard).await {
        Err(e) => finish(&guard, Err(e)),
        Ok(upstream) => {
            guard.update(|s| s.status = 101);
            let mut r = ws
                .protocols(["realtime"])
                .on_upgrade(move |socket| relay(socket, upstream, guard))
                .into_response();
            if let Ok(v) = HeaderValue::from_str(&request_id) {
                r.headers_mut().insert("x-request-id", v);
            }
            r
        }
    }
}

type Upstream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    app: &Arc<AppState>,
    headers: &HeaderMap,
    model: Option<String>,
    guard: &RequestGuard,
) -> Result<Upstream, ApiError> {
    let rt = app.rt();
    let headers = auth_headers(headers);
    let tenant = claim(app, &headers, guard)?;
    let (opts, zdr) = apply_options(&headers, None, &tenant, guard)?;
    let model = model
        .filter(|m| crate::config::valid_model(m))
        .ok_or_else(|| ApiError::bad_request("query parameter 'model' is required"))?;
    guard.update(|s| s.route = Some(model.clone()));
    let targets = plan_targets(app, &rt, &tenant, &model, &opts, Endpoint::Chat, zdr)?;
    let target = targets
        .iter()
        .find(|t| rt.providers.get(&t.provider).is_some_and(|p| p.cfg.kind == ProviderKind::Openai))
        .ok_or_else(|| {
            ApiError::bad_request(format!("model '{model}' is not served by an OpenAI-compatible provider"))
        })?;
    let provider = rt.providers[&target.provider].clone();

    enforce_key_limits(app, &rt, &tenant).await?;
    let dims =
        Dims { tenant: &tenant.name, user: opts.user.as_deref(), model: &model, metadata: &opts.metadata };
    for p in [None, Some(provider.name.as_str())] {
        match limits::check(&app.store, &rt.cfg.limits, &dims, p, unix_secs()).await {
            Ok(applied) => guard.update(|s| s.limits.extend(applied)),
            Err(CheckError::Exceeded(x)) => return Err(limit_exceeded(&x.name, x.retry_after)),
            Err(CheckError::Store(e)) => on_store_error(&rt, "limits", e)?,
        }
    }

    let base = provider.cfg.base_url.trim_end_matches('/');
    let ws_base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let url = format!("{ws_base}/realtime?model={}", target.model);
    let mut req =
        url.into_client_request().map_err(|_| ApiError::internal_config("invalid provider base_url"))?;
    let (key, credential) = credentials(app, &provider, &opts);
    let auth = HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| ApiError::internal_config("invalid provider key"))?;
    req.headers_mut().insert("authorization", auth);
    req.headers_mut().insert("openai-beta", HeaderValue::from_static("realtime=v1"));
    guard.update(|s| s.attempts += 1);
    let connected = tokio::time::timeout(provider.timeout(), tokio_tungstenite::connect_async(req)).await;
    match connected {
        Ok(Ok((stream, _))) => {
            provider.breaker.record_success();
            guard.update(|s| {
                s.provider = Some(provider.name.clone());
                s.model = Some(target.model.clone());
                s.credential = Some(credential);
                s.price_input_per_mtok = target.price_input_per_mtok;
                s.price_output_per_mtok = target.price_output_per_mtok;
            });
            Ok(stream)
        }
        Ok(Err(tungstenite::Error::Http(r))) if matches!(r.status().as_u16(), 401 | 403) => {
            Err(ApiError::upstream("upstream provider rejected the credentials"))
        }
        _ => {
            crate::proxy::provider_failed(app, &provider);
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "upstream_error",
                "could not connect to the provider",
            ))
        }
    }
}

fn to_upstream(m: Message) -> Option<tungstenite::Message> {
    Some(match m {
        Message::Text(t) => tungstenite::Message::Text(t),
        Message::Binary(b) => tungstenite::Message::Binary(b),
        Message::Ping(p) => tungstenite::Message::Ping(p),
        Message::Pong(p) => tungstenite::Message::Pong(p),
        Message::Close(_) => return None,
    })
}

fn to_client(m: tungstenite::Message) -> Option<Message> {
    Some(match m {
        tungstenite::Message::Text(t) => Message::Text(t),
        tungstenite::Message::Binary(b) => Message::Binary(b),
        tungstenite::Message::Ping(p) => Message::Ping(p),
        tungstenite::Message::Pong(p) => Message::Pong(p),
        tungstenite::Message::Close(_) | tungstenite::Message::Frame(_) => return None,
    })
}

async fn relay(client: WebSocket, upstream: Upstream, guard: RequestGuard) {
    let (mut up_tx, mut up_rx) = upstream.split();
    let (mut cl_tx, mut cl_rx) = client.split();
    let mut cancel = guard.app().stream_cancel.subscribe();

    let to_provider = async {
        while let Some(Ok(m)) = cl_rx.next().await {
            let Some(m) = to_upstream(m) else { break };
            if up_tx.send(m).await.is_err() {
                break;
            }
        }
        let _ = up_tx.close().await;
    };
    let g = guard.clone();
    let to_client_side = async {
        let mut first = true;
        while let Some(Ok(m)) = up_rx.next().await {
            if first {
                first = false;
                g.mark_first_byte();
            }
            if let tungstenite::Message::Text(t) = &m {
                if t.contains("\"response.done\"") {
                    if let Ok(v) = serde_json::from_str::<Value>(t) {
                        if let Some(u) = Usage::sniff(&v) {
                            g.update(|s| {
                                s.usage.prompt_tokens += u.prompt_tokens;
                                s.usage.completion_tokens += u.completion_tokens;
                                s.usage.cached_tokens += u.cached_tokens;
                            });
                        }
                    }
                }
            }
            let Some(m) = to_client(m) else { break };
            if cl_tx.send(m).await.is_err() {
                break;
            }
        }
        let _ = cl_tx.send(Message::Close(Some(CloseFrame { code: 1000, reason: "".into() }))).await;
    };
    let shutdown = async {
        let _ = cancel.wait_for(|v| *v).await;
    };
    tokio::select! {
        _ = to_provider => {},
        _ = to_client_side => {},
        _ = shutdown => guard.update(|s| s.error = Some("gateway shutting down".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_subprotocol_key() {
        let mut h = HeaderMap::new();
        h.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("realtime, openai-insecure-api-key.gw_abc"),
        );
        assert_eq!(auth_headers(&h)["authorization"], "Bearer gw_abc");
        let mut h2 = HeaderMap::new();
        h2.insert("authorization", HeaderValue::from_static("Bearer other"));
        assert_eq!(auth_headers(&h2)["authorization"], "Bearer other");
    }
}
