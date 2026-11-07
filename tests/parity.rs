//! End-to-end tests for the gateway features that match Cloudflare AI
//! Gateway and Vercel AI Gateway: request options, metadata and reporting,
//! every API surface, routing rules and branches, fallbacks, limits, alerts,
//! BYOK, ZDR, caching controls, guardrails with moderation, feedback, log
//! export, OpenTelemetry, audit, reload and realtime WebSockets.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use ai_gateway::{app, auth::hash_key, config::Config};
use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocketUpgrade},
        State,
    },
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};

const KEY_FULL: &str = "key-full";
const KEY_RESTRICTED: &str = "key-restricted";
const KEY_OTHER: &str = "key-other";
const ADMIN: &str = "admin-token";
const ENC_KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

#[derive(Default)]
struct Mock {
    openai_hits: AtomicUsize,
    anthropic_hits: AtomicUsize,
    last_openai: Mutex<Option<Value>>,
    last_anthropic: Mutex<Option<Value>>,
    last_anthropic_headers: Mutex<Option<HeaderMap>>,
    webhooks: Mutex<Vec<(HeaderMap, Value)>>,
    exported: Mutex<Vec<String>>,
    otlp: Mutex<Vec<Value>>,
}

fn openai_auth(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer test-openai")
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": {"message": "bad key", "type": "invalid_request_error"}})),
    )
        .into_response()
}

async fn chat(State(m): State<Arc<Mock>>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if !openai_auth(&h) {
        return unauthorized();
    }
    *m.last_openai.lock() = Some(body.clone());
    let model = body["model"].as_str().unwrap_or("").to_string();
    if model == "fail-500" {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "boom"}})))
            .into_response();
    }
    m.openai_hits.fetch_add(1, Ordering::SeqCst);
    let last = body["messages"].as_array().and_then(|a| a.last()).cloned().unwrap_or_default();
    if body["stream"] == true {
        let mut s = String::new();
        for c in [
            json!({"choices": [{"index": 0, "delta": {"content": "Hel"}}]}),
            json!({"choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2}}),
        ] {
            s.push_str(&format!("data: {c}\n\n"));
        }
        s.push_str("data: [DONE]\n\n");
        return ([(CONTENT_TYPE, "text/event-stream")], s).into_response();
    }
    let content = if model == "echo" { last["content"].clone() } else { json!("hi there") };
    Json(json!({
        "id": "c1", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 4}}
    }))
    .into_response()
}

async fn responses_api(State(m): State<Arc<Mock>>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if !openai_auth(&h) {
        return unauthorized();
    }
    *m.last_openai.lock() = Some(body.clone());
    if body["stream"] == true {
        let events = [
            ("response.created", json!({"type": "response.created", "response": {"id": "r1"}})),
            ("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": "Hi"})),
            (
                "response.completed",
                json!({"type": "response.completed", "response": {"id": "r1", "usage": {"input_tokens": 6, "output_tokens": 3}}}),
            ),
        ];
        let mut s = String::new();
        for (e, d) in events {
            s.push_str(&format!("event: {e}\ndata: {d}\n\n"));
        }
        return ([(CONTENT_TYPE, "text/event-stream")], s).into_response();
    }
    Json(json!({
        "id": "r1", "object": "response",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "Hi there"}]}],
        "usage": {"input_tokens": 5, "output_tokens": 2, "output_tokens_details": {"reasoning_tokens": 1}}
    }))
    .into_response()
}

async fn moderations(h: HeaderMap, Json(body): Json<Value>) -> Response {
    if !openai_auth(&h) {
        return unauthorized();
    }
    let text = body["input"].as_str().unwrap_or("");
    let bad = text.contains("attack");
    Json(json!({"results": [{
        "flagged": bad,
        "categories": {"violence": bad, "hate": false},
        "category_scores": {"violence": if bad { 0.97 } else { 0.01 }, "hate": 0.0}
    }]}))
    .into_response()
}

async fn images(h: HeaderMap, Json(body): Json<Value>) -> Response {
    if !openai_auth(&h) {
        return unauthorized();
    }
    Json(json!({"created": 1, "data": [{"url": format!("https://img.example/{}", body["prompt"].as_str().unwrap_or(""))}]}))
        .into_response()
}

async fn rerank(h: HeaderMap, Json(_): Json<Value>) -> Response {
    if !openai_auth(&h) {
        return unauthorized();
    }
    Json(json!({"results": [{"index": 1, "relevance_score": 0.9}], "usage": {"prompt_tokens": 8}}))
        .into_response()
}

async fn speech(h: HeaderMap, Json(_): Json<Value>) -> Response {
    if !openai_auth(&h) {
        return unauthorized();
    }
    ([(CONTENT_TYPE, "audio/mpeg")], Bytes::from_static(b"ID3fakeaudio")).into_response()
}

async fn models_list(h: HeaderMap) -> Response {
    if !openai_auth(&h) {
        return unauthorized();
    }
    Json(json!({"object": "list", "data": [{"id": "gpt-test"}]})).into_response()
}

async fn anthropic(State(m): State<Arc<Mock>>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if h.get("x-api-key").and_then(|v| v.to_str().ok()) != Some("test-anthropic") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"type": "error", "error": {"type": "authentication_error", "message": "bad key"}})),
        )
            .into_response();
    }
    m.anthropic_hits.fetch_add(1, Ordering::SeqCst);
    *m.last_anthropic.lock() = Some(body.clone());
    *m.last_anthropic_headers.lock() = Some(h.clone());
    if body["stream"] == true {
        let events = [
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "m1", "model": "claude-test", "usage": {"input_tokens": 12, "output_tokens": 1}}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}),
            ),
            ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 4}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ];
        let mut s = String::new();
        for (e, d) in events {
            s.push_str(&format!("event: {e}\ndata: {d}\n\n"));
        }
        return ([(CONTENT_TYPE, "text/event-stream")], s).into_response();
    }
    Json(json!({
        "id": "m1", "type": "message", "role": "assistant", "model": "claude-test",
        "content": [{"type": "text", "text": "Hello from Claude"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 8, "output_tokens": 6}
    }))
    .into_response()
}

async fn webhook(State(m): State<Arc<Mock>>, h: HeaderMap, Json(body): Json<Value>) -> StatusCode {
    m.webhooks.lock().push((h, body));
    StatusCode::OK
}

async fn export_sink(State(m): State<Arc<Mock>>, body: String) -> StatusCode {
    m.exported.lock().extend(body.lines().map(str::to_string));
    StatusCode::OK
}

async fn otlp_sink(State(m): State<Arc<Mock>>, Json(body): Json<Value>) -> StatusCode {
    m.otlp.lock().push(body);
    StatusCode::OK
}

async fn realtime_upstream(ws: WebSocketUpgrade, h: HeaderMap) -> Response {
    if !openai_auth(&h) {
        return unauthorized();
    }
    ws.on_upgrade(|mut socket| async move {
        while let Some(Ok(Message::Text(t))) = socket.recv().await {
            let _ = socket.send(Message::Text(format!("echo:{t}"))).await;
            let done = json!({"type": "response.done", "response": {"usage": {"input_tokens": 3, "output_tokens": 7}}});
            let _ = socket.send(Message::Text(done.to_string())).await;
        }
    })
}

async fn spawn(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

struct H {
    base: String,
    admin: String,
    mock: Arc<Mock>,
    mock_addr: SocketAddr,
    http: reqwest::Client,
    state: Arc<app::AppState>,
}

fn config_yaml(m: SocketAddr, extra: &str) -> String {
    let store = match std::env::var("AIGW_TEST_REDIS_URL") {
        Ok(url) => format!(
            "store:\n  backend: redis\n  redis_url: \"{url}\"\n  key_prefix: \"aigw-parity-{}:\"\n",
            uuid::Uuid::new_v4()
        ),
        Err(_) => String::new(),
    };
    // Tests never download the public price list, and never pick up
    // provider keys from the developer's environment.
    let pricing_block = format!(
        "{}{}",
        if extra.contains("pricing:") { "" } else { "pricing:\n  enabled: false\n" },
        if extra.contains("auto_providers:") { "" } else { "auto_providers: false\n" }
    );
    format!(
        r#"
{store}
{extra}
{pricing}
server:
  listen: 127.0.0.1:0
  admin_listen: 127.0.0.1:0
cache:
  enabled: true
  ttl_secs: 60
admin:
  token_sha256: "{admin}"
  key_refresh_secs: 1
  encryption_key: "{ENC_KEY}"
providers:
  openai:
    kind: openai
    base_url: http://{m}/openai
    api_key: test-openai
    max_retries: 0
  flaky:
    kind: openai
    base_url: http://{m}/openai
    api_key: test-openai
    max_retries: 1
  badkey:
    kind: openai
    base_url: http://{m}/openai
    api_key: wrong-key
    max_retries: 0
  zdrprov:
    kind: openai
    base_url: http://{m}/openai
    api_key: test-openai
    zero_data_retention: true
  anthropic:
    kind: anthropic
    base_url: http://{m}/anthropic
    api_key: test-anthropic
routes:
  - name: smart
    targets:
      - provider: flaky
        model: fail-500
      - provider: anthropic
        model: claude-test
  - name: tiered
    targets:
      - provider: openai
        model: gpt-pro
    branches:
      - when:
          metadata.plan: free
        targets:
          - provider: anthropic
            model: claude-test
keys:
  - name: full
    key_sha256: "{full}"
    allowed_models: ["*"]
  - name: other
    key_sha256: "{other}"
    allowed_models: ["*"]
  - name: restricted
    key_sha256: "{restricted}"
    allowed_models: ["smart"]
"#,
        pricing = pricing_block,
        admin = hash_key(ADMIN),
        full = hash_key(KEY_FULL),
        other = hash_key(KEY_OTHER),
        restricted = hash_key(KEY_RESTRICTED),
    )
}

async fn start_mock() -> (Arc<Mock>, SocketAddr) {
    let mock = Arc::new(Mock::default());
    let router = Router::new()
        .route("/openai/chat/completions", post(chat))
        .route("/openai/responses", post(responses_api))
        .route("/openai/moderations", post(moderations))
        .route("/openai/images/generations", post(images))
        .route("/openai/rerank", post(rerank))
        .route("/openai/audio/speech", post(speech))
        .route("/openai/models", get(models_list))
        .route("/openai/realtime", get(realtime_upstream))
        .route("/anthropic/messages", post(anthropic))
        .route("/webhook", post(webhook))
        .route("/export", post(export_sink))
        .route("/otel/v1/traces", post(otlp_sink))
        .with_state(mock.clone());
    let addr = spawn(router).await;
    (mock, addr)
}

async fn start(extra: &str) -> H {
    let (mock, m) = start_mock().await;
    let extra = extra.replace("MOCK", &m.to_string());
    let cfg = Config::from_yaml_str(&config_yaml(m, &extra)).expect("valid config");
    let state = app::AppState::build(cfg).await.unwrap();
    let gw = spawn(app::router(state.clone())).await;
    let admin = spawn(app::admin_api_router(state.clone())).await;
    H {
        base: format!("http://{gw}"),
        admin: format!("http://{admin}"),
        mock,
        mock_addr: m,
        http: reqwest::Client::new(),
        state,
    }
}

impl H {
    async fn post(&self, path: &str, key: &str, body: Value, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut rb = self.http.post(format!("{}{path}", self.base)).bearer_auth(key).json(&body);
        for (k, v) in headers {
            rb = rb.header(*k, *v);
        }
        rb.send().await.unwrap()
    }

    async fn chat(&self, body: Value, headers: &[(&str, &str)]) -> reqwest::Response {
        self.post("/v1/chat/completions", KEY_FULL, body, headers).await
    }

    async fn admin_get(&self, path: &str) -> Value {
        let r = self.http.get(format!("{}{path}", self.admin)).bearer_auth(ADMIN).send().await.unwrap();
        assert!(r.status().is_success(), "GET {path}: {}", r.status());
        r.json().await.unwrap()
    }

    async fn admin_send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> reqwest::Response {
        let mut rb = self.http.request(method, format!("{}{path}", self.admin)).bearer_auth(ADMIN);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        rb.send().await.unwrap()
    }

    async fn logs(&self, n: u64) -> Vec<Value> {
        for _ in 0..100 {
            let v = self.admin_get("/api/logs").await;
            if v["total"].as_u64().unwrap_or(0) >= n {
                return v["data"].as_array().unwrap().clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("log never reached {n} entries");
    }

    async fn log(&self, id: &str) -> Value {
        for _ in 0..100 {
            let r = self
                .http
                .get(format!("{}/api/logs/{id}", self.admin))
                .bearer_auth(ADMIN)
                .send()
                .await
                .unwrap();
            if r.status() == 200 {
                return r.json().await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("log entry {id} never appeared");
    }
}

fn hello(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hello"}]})
}

fn rid(r: &reqwest::Response) -> String {
    r.headers()["x-request-id"].to_str().unwrap().to_string()
}

#[tokio::test]
async fn metadata_user_tags_are_logged_reported_and_stripped() {
    let h = start("").await;
    let mut body = hello("openai/gpt-test");
    body["user"] = json!("body-user");
    body["providerOptions"] = json!({"gateway": {"tags": ["feature:chat"], "user": "u-42"}});
    let r = h
        .chat(
            body,
            &[
                ("cf-aig-metadata", r#"{"plan":"free","team":7}"#),
                ("ai-reporting-tags", "env:test"),
                ("x-gateway-event-id", "evt-1"),
            ],
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-gateway-generation-id"], r.headers()["x-request-id"]);
    let id = rid(&r);
    let sent = h.mock.last_openai.lock().clone().unwrap();
    assert!(sent.get("providerOptions").is_none(), "providerOptions never reach the provider");

    let e = h.log(&id).await;
    assert_eq!(e["user"], "u-42");
    assert_eq!(e["metadata"]["plan"], "free");
    assert_eq!(e["metadata"]["team"], "7");
    assert_eq!(e["event_id"], "evt-1");
    assert_eq!(e["tags"], json!(["feature:chat", "env:test"]));
    assert_eq!(e["cached_tokens"], 4);
    assert!(e["ttft_ms"].is_u64());
    assert_eq!(e["attempts"][0]["provider"], "openai");

    let filtered = h.admin_get("/api/logs?metadata=plan%3Dfree&user=u-42&tag=env:test").await;
    assert_eq!(filtered["total"], 1);

    let today = {
        let s = ai_gateway::proxy::unix_secs();
        let d = h.admin_get("/api/overview?range=1h").await;
        assert_eq!(d["users"][0]["name"], "u-42");
        assert_eq!(d["tags"].as_array().unwrap().len(), 2);
        s
    };
    let _ = today;
    // Reports by user and tag, for admins and for the key itself.
    let date = chrono_today();
    let rep = h.admin_get(&format!("/api/report?start_date={date}&end_date={date}&group_by=user")).await;
    assert_eq!(rep["results"][0]["user"], "u-42");
    assert_eq!(rep["results"][0]["input_tokens"], 10);
    let rep = h
        .admin_get(&format!("/api/report?start_date={date}&end_date={date}&group_by=tag&tags=env:test"))
        .await;
    assert_eq!(rep["results"].as_array().unwrap().len(), 2);
    let own = h
        .http
        .get(format!("{}/v1/report?start_date={date}&end_date={date}&group_by=model", h.base))
        .bearer_auth(KEY_FULL)
        .send()
        .await
        .unwrap();
    let own: Value = own.json().await.unwrap();
    assert_eq!(own["results"][0]["model"], "gpt-test");
    let other: Value = h
        .http
        .get(format!("{}/v1/report?start_date={date}&end_date={date}", h.base))
        .bearer_auth(KEY_OTHER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(other["results"].as_array().unwrap().is_empty(), "keys only see their own usage");

    // Generation lookup: own requests only.
    let g =
        h.http.get(format!("{}/v1/generation?id={id}", h.base)).bearer_auth(KEY_FULL).send().await.unwrap();
    assert_eq!(g.status(), 200);
    let g =
        h.http.get(format!("{}/v1/generation?id={id}", h.base)).bearer_auth(KEY_OTHER).send().await.unwrap();
    assert_eq!(g.status(), 404);
}

fn chrono_today() -> String {
    let secs = ai_gateway::proxy::unix_secs();
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
    format!("{y:04}-{m:02}-{d:02}")
}

#[tokio::test]
async fn collect_log_false_keeps_request_out_of_the_log() {
    let h = start("").await;
    assert_eq!(h.chat(hello("openai/gpt-test"), &[("cf-aig-collect-log", "false")]).await.status(), 200);
    assert_eq!(h.chat(hello("openai/gpt-1"), &[]).await.status(), 200);
    let logs = h.logs(1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let all = h.admin_get("/api/logs").await;
    assert_eq!(all["total"], 1);
    assert_eq!(logs[0]["model"], "gpt-1");
    let o = h.admin_get("/api/overview?range=1h").await;
    assert_eq!(o["totals"]["requests"], 2, "analytics still count it");
}

#[tokio::test]
async fn fallback_models_order_and_only() {
    let h = start("").await;
    // Request-level fallback models.
    let mut body = hello("openai/fail-500");
    body["providerOptions"] = json!({"gateway": {"models": ["anthropic/claude-test"]}});
    let r = h.chat(body, &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-gateway-provider"], "anthropic");
    let e = h.log(&rid(&r)).await;
    assert_eq!(e["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(e["attempts"][0]["status"], 502);

    // `only` skips the flaky first target of the route entirely.
    let mut body = hello("smart");
    body["providerOptions"] = json!({"gateway": {"only": ["anthropic"]}});
    let r = h.chat(body, &[]).await;
    assert_eq!(r.headers()["x-gateway-provider"], "anthropic");
    assert_eq!(h.log(&rid(&r)).await["attempts"].as_array().unwrap().len(), 1);

    // `order` puts anthropic first.
    let mut body = hello("smart");
    body["providerOptions"] = json!({"gateway": {"order": ["anthropic"]}});
    let r = h.chat(body, &[]).await;
    assert_eq!(h.log(&rid(&r)).await["attempts"].as_array().unwrap().len(), 1);

    // Per-request attempts: one try on the flaky provider instead of two.
    let mut body = hello("smart");
    body["providerOptions"] = json!({"gateway": {"order": ["flaky"]}});
    let r = h.chat(body, &[("cf-aig-max-attempts", "1"), ("cf-aig-retry-delay", "1")]).await;
    assert_eq!(h.log(&rid(&r)).await["attempts"].as_array().unwrap().len(), 2, "1 flaky + 1 anthropic");
}

#[tokio::test]
async fn routing_rules_from_config_and_admin_api() {
    let h = start("routing_rules:\n  - source: openai/old-model\n    action: rewrite\n    destination: openai/gpt-new\n").await;
    let r = h.chat(hello("openai/old-model"), &[]).await;
    assert_eq!(r.headers()["x-gateway-model"], "gpt-new");

    let r = h
        .admin_send(
            reqwest::Method::POST,
            "/api/rules",
            Some(json!({"source": "openai/banned", "action": "deny", "reason": "not approved"})),
        )
        .await;
    assert_eq!(r.status(), 201);
    let rule: Value = r.json().await.unwrap();
    let r = h.chat(hello("openai/banned"), &[]).await;
    assert_eq!(r.status(), 403);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "model_denied");
    assert!(body["error"]["message"].as_str().unwrap().contains("not approved"));

    // A denied fallback is skipped; the request still succeeds on the primary.
    let mut b = hello("openai/gpt-test");
    b["providerOptions"] = json!({"gateway": {"models": ["openai/banned"]}});
    assert_eq!(h.chat(b, &[]).await.status(), 200);

    let rules = h.admin_get("/api/rules").await;
    assert_eq!(rules["data"].as_array().unwrap().len(), 2);
    let id = rule["id"].as_str().unwrap();
    let r = h
        .admin_send(reqwest::Method::PATCH, &format!("/api/rules/{id}"), Some(json!({"enabled": false})))
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(h.chat(hello("openai/banned"), &[]).await.status(), 200);
    assert_eq!(h.admin_send(reqwest::Method::DELETE, &format!("/api/rules/{id}"), None).await.status(), 204);
    assert_eq!(h.admin_send(reqwest::Method::DELETE, "/api/rules/config-1", None).await.status(), 404);

    let audit = h.admin_get("/api/audit").await;
    let actions: Vec<&str> =
        audit["data"].as_array().unwrap().iter().filter_map(|a| a["action"].as_str()).collect();
    assert!(actions.contains(&"rule.create") && actions.contains(&"rule.delete"));
}

#[tokio::test]
async fn conditional_branches_route_by_metadata() {
    let h = start("").await;
    let r = h.chat(hello("tiered"), &[("x-gateway-metadata", r#"{"plan":"free"}"#)]).await;
    assert_eq!(r.headers()["x-gateway-provider"], "anthropic");
    let r = h.chat(hello("tiered"), &[("x-gateway-metadata", r#"{"plan":"pro"}"#)]).await;
    assert_eq!(r.headers()["x-gateway-model"], "gpt-pro");
}

#[tokio::test]
async fn limits_by_user_and_tokens_with_webhook_alerts() {
    let h = start(
        r#"limits:
  - name: user-rpm
    kind: requests
    limit: 2
    window: minute
    per: user
  - name: team-tokens
    kind: tokens
    limit: 50
    window: day
    per: metadata.team
alerts:
  webhook_url: http://MOCK/webhook
  webhook_secret: shh
  thresholds: [0.5]
"#,
    )
    .await;
    let u = |user: &str| ("ai-reporting-user", user.to_string());
    for _ in 0..2 {
        let (k, v) = u("alice");
        assert_eq!(h.chat(hello("openai/gpt-test"), &[(k, &v)]).await.status(), 200);
    }
    let (k, v) = u("alice");
    let r = h.chat(hello("openai/gpt-test"), &[(k, &v)]).await;
    assert_eq!(r.status(), 429);
    assert!(r.headers().get("retry-after").is_some());
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "limit_exceeded");
    let (k, v) = u("bob");
    assert_eq!(h.chat(hello("openai/gpt-test"), &[(k, &v)]).await.status(), 200, "bob has his own window");

    // 30 tokens, then over 50 with the second: the third is rejected.
    let team = ("x-gateway-metadata", r#"{"team":"red"}"#);
    let mut b = hello("openai/gpt-a");
    assert_eq!(h.chat(b.clone(), &[team]).await.status(), 200);
    tokio::time::sleep(Duration::from_millis(300)).await;
    b["model"] = json!("openai/gpt-b");
    assert_eq!(h.chat(b.clone(), &[team]).await.status(), 200);
    tokio::time::sleep(Duration::from_millis(300)).await;
    b["model"] = json!("openai/gpt-c");
    assert_eq!(h.chat(b, &[team]).await.status(), 429);

    // The 50% threshold alert was sent once, signed.
    let hooks = h.mock.webhooks.lock().clone();
    let alerts: Vec<&(HeaderMap, Value)> =
        hooks.iter().filter(|(_, b)| b["limit"] == "team-tokens").collect();
    assert_eq!(alerts.len(), 1, "{hooks:?}");
    let (hdrs, body) = alerts[0];
    assert_eq!(body["type"], "limit.threshold");
    assert_eq!(body["scope"], "red");
    let sig = hdrs["x-gateway-signature"].to_str().unwrap();
    assert_eq!(sig, ai_gateway::alerts::sign("shh", serde_json::to_vec(body).unwrap().as_slice()));

    let limits = h.admin_get("/api/limits").await;
    assert_eq!(limits["data"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn request_and_stored_byok() {
    let h = start("").await;
    // The config key for `badkey` is wrong; a client key fixes it per request.
    let mut b = hello("badkey/gpt-test");
    b["providerOptions"] = json!({"gateway": {"byok": {"badkey": [{"apiKey": "test-openai"}]}}});
    let r = h.chat(b, &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(h.log(&rid(&r)).await["credential"], "request");
    // A wrong client key is reported to the client, not hidden.
    let mut b = hello("badkey/gpt-test");
    b["providerOptions"] = json!({"gateway": {"byok": {"badkey": "nope"}}});
    let r = h.chat(b, &[]).await;
    assert_eq!(r.status(), 401);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "provider_key_rejected");

    assert_eq!(h.chat(hello("badkey/gpt-test"), &[]).await.status(), 502);
    let r = h
        .admin_send(
            reqwest::Method::PUT,
            "/api/provider-keys/badkey",
            Some(json!({"api_key": "test-openai"})),
        )
        .await;
    assert_eq!(r.status(), 204);
    let r = h.chat(hello("badkey/gpt-test"), &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(h.log(&rid(&r)).await["credential"], "stored");
    let keys = h.admin_get("/api/provider-keys").await;
    let bk = keys["data"].as_array().unwrap().iter().find(|k| k["provider"] == "badkey").unwrap().clone();
    assert_eq!(bk["source"], "stored");
    assert!(!bk.to_string().contains("test-openai"), "keys are never returned");
    // Survives a refresh from the store (decrypts correctly).
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(h.chat(hello("badkey/gpt-test"), &[]).await.status(), 200);
    assert_eq!(h.admin_send(reqwest::Method::DELETE, "/api/provider-keys/badkey", None).await.status(), 204);
    // The earlier answer is still cached; bypass the cache to hit the provider.
    assert_eq!(h.chat(hello("badkey/gpt-test"), &[("cf-aig-skip-cache", "true")]).await.status(), 502);
}

#[tokio::test]
async fn zero_data_retention_filters_providers_and_skips_storage() {
    let h = start("logging:\n  capture_bodies: true\n").await;
    let mut b = hello("openai/gpt-test");
    b["providerOptions"] = json!({"gateway": {"zeroDataRetention": true}});
    let r = h.chat(b.clone(), &[]).await;
    assert_eq!(r.status(), 400, "openai is not marked ZDR");
    b["model"] = json!("zdrprov/gpt-test");
    let r = h.chat(b.clone(), &[]).await;
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("x-gateway-cache").is_none(), "ZDR requests are never cached");
    let e = h.log(&rid(&r)).await;
    assert_eq!(e["zero_data_retention"], true);
    assert!(e.get("request_body").is_none() && e.get("response_body").is_none());

    // A key created with ZDR gets the same treatment without asking.
    let created: Value = h
        .admin_send(
            reqwest::Method::POST,
            "/api/keys",
            Some(json!({"name": "private", "allowed_models": ["*"], "zero_data_retention": true})),
        )
        .await
        .json()
        .await
        .unwrap();
    let key = created["api_key"].as_str().unwrap();
    assert_eq!(h.post("/v1/chat/completions", key, hello("openai/gpt-test"), &[]).await.status(), 400);
    assert_eq!(h.post("/v1/chat/completions", key, hello("zdrprov/gpt-test"), &[]).await.status(), 200);
}

#[tokio::test]
async fn cache_controls_and_stream_cache() {
    let h = start("").await;
    let r = h.chat(hello("openai/gpt-test"), &[("cf-aig-cache-key", "greeting")]).await;
    assert_eq!(r.headers()["x-gateway-cache"], "MISS");
    let mut other = hello("openai/gpt-test");
    other["messages"][0]["content"] = json!("different text, same key");
    let r = h.chat(other, &[("cf-aig-cache-key", "greeting")]).await;
    assert_eq!(r.headers()["x-gateway-cache"], "HIT");
    assert_eq!(r.headers()["cf-aig-cache-status"], "HIT");
    let r = h.chat(hello("openai/gpt-test"), &[("cf-aig-skip-cache", "true")]).await;
    assert!(r.headers().get("x-gateway-cache").is_none());

    let mut s = hello("openai/stream-model");
    s["stream"] = json!(true);
    let first = h.chat(s.clone(), &[]).await.text().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let hits = h.mock.openai_hits.load(Ordering::SeqCst);
    let r = h.chat(s, &[]).await;
    assert_eq!(r.headers()["x-gateway-cache"], "HIT");
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    assert_eq!(r.text().await.unwrap(), first, "a cached stream replays exactly");
    assert_eq!(h.mock.openai_hits.load(Ordering::SeqCst), hits);
}

#[tokio::test]
async fn responses_api_streams_events_through() {
    let h = start("").await;
    let r = h.post("/v1/responses", KEY_FULL, json!({"model": "openai/gpt-r", "input": "hi"}), &[]).await;
    assert_eq!(r.status(), 200);
    let id = rid(&r);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["output"][0]["content"][0]["text"], "Hi there");
    let e = h.log(&id).await;
    assert_eq!((e["prompt_tokens"].as_u64(), e["completion_tokens"].as_u64()), (Some(5), Some(2)));
    assert_eq!(e["reasoning_tokens"], 1);
    assert_eq!(e["endpoint"], "responses");

    let r = h
        .post("/v1/responses", KEY_FULL, json!({"model": "openai/gpt-r", "input": "hi", "stream": true}), &[])
        .await;
    let id = rid(&r);
    let text = r.text().await.unwrap();
    assert!(text.contains("event: response.completed"), "event names are kept: {text}");
    let e = h.log(&id).await;
    assert_eq!((e["prompt_tokens"].as_u64(), e["completion_tokens"].as_u64()), (Some(6), Some(3)));
    assert!(e["error"].is_null());

    // Responses only goes to OpenAI-style providers.
    let r = h
        .post("/v1/responses", KEY_FULL, json!({"model": "anthropic/claude-test", "input": "hi"}), &[])
        .await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn anthropic_messages_api_native_with_auto_caching() {
    let h = start("").await;
    let body = json!({
        "model": "anthropic/claude-test",
        "system": "You are terse.",
        "messages": [{"role": "user", "content": "hi"}],
        "providerOptions": {"gateway": {"caching": "auto"}}
    });
    let r = h
        .http
        .post(format!("{}/v1/messages", h.base))
        .header("x-api-key", KEY_FULL)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "prompt-caching-2024-07-31")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let id = rid(&r);
    let out: Value = r.json().await.unwrap();
    assert_eq!(out["content"][0]["text"], "Hello from Claude", "native response shape");
    let sent = h.mock.last_anthropic.lock().clone().unwrap();
    assert_eq!(sent["model"], "claude-test");
    assert_eq!(sent["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(sent["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
    assert!(sent.get("providerOptions").is_none());
    let hdrs = h.mock.last_anthropic_headers.lock().clone().unwrap();
    assert_eq!(hdrs["anthropic-beta"], "prompt-caching-2024-07-31");
    assert_eq!(h.log(&id).await["endpoint"], "messages");

    let mut s = body.clone();
    s["stream"] = json!(true);
    let r = h.post("/v1/messages", KEY_FULL, s, &[]).await;
    let id = rid(&r);
    let text = r.text().await.unwrap();
    assert!(text.contains("event: message_start") && text.contains("event: message_stop"));
    let e = h.log(&id).await;
    assert_eq!((e["prompt_tokens"].as_u64(), e["completion_tokens"].as_u64()), (Some(12), Some(4)));

    assert_eq!(h.post("/v1/messages", KEY_FULL, hello("openai/gpt-test"), &[]).await.status(), 400);
}

#[tokio::test]
async fn images_moderations_rerank_speech() {
    let h = start("").await;
    let r = h
        .post("/v1/images/generations", KEY_FULL, json!({"model": "openai/img", "prompt": "cat"}), &[])
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["data"][0]["url"], "https://img.example/cat");
    let r =
        h.post("/v1/moderations", KEY_FULL, json!({"model": "openai/omni", "input": "an attack"}), &[]).await;
    assert_eq!(r.json::<Value>().await.unwrap()["results"][0]["flagged"], true);
    let r = h
        .post(
            "/v1/rerank",
            KEY_FULL,
            json!({"model": "openai/rr", "query": "q", "documents": ["a", "b"]}),
            &[],
        )
        .await;
    let id = rid(&r);
    assert_eq!(r.json::<Value>().await.unwrap()["results"][0]["index"], 1);
    assert_eq!(h.log(&id).await["prompt_tokens"], 8);
    let r = h
        .post(
            "/v1/audio/speech",
            KEY_FULL,
            json!({"model": "openai/tts", "input": "hello", "voice": "x"}),
            &[],
        )
        .await;
    assert_eq!(r.headers()["content-type"], "audio/mpeg");
    assert_eq!(&r.bytes().await.unwrap()[..], b"ID3fakeaudio");
}

#[tokio::test]
async fn native_passthrough() {
    let h = start("").await;
    let r = h.post("/native/openai/chat/completions", KEY_FULL, hello("gpt-test"), &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-gateway-provider"], "openai");
    let id = rid(&r);
    let e = h.log(&id).await;
    assert_eq!(e["endpoint"], "native");
    assert_eq!(e["prompt_tokens"], 10);

    let r =
        h.http.get(format!("{}/native/openai/models", h.base)).bearer_auth(KEY_FULL).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["data"][0]["id"], "gpt-test");

    let mut s = json!({"model": "claude-test", "max_tokens": 10, "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    let r = h.post("/native/anthropic/messages", KEY_FULL, s.clone(), &[]).await;
    let id = rid(&r);
    assert!(r.text().await.unwrap().contains("event: content_block_delta"));
    assert_eq!(h.log(&id).await["completion_tokens"], 4);

    // Permissions apply: this key may only use the "smart" route.
    s["stream"] = json!(false);
    assert_eq!(h.post("/native/anthropic/messages", KEY_RESTRICTED, s, &[]).await.status(), 403);
    assert_eq!(h.post("/native/nope/x", KEY_FULL, json!({}), &[]).await.status(), 404);
}

#[tokio::test]
async fn moderation_guardrail_blocks_and_reports() {
    let h = start(
        r#"guardrails:
  rules:
    - name: violence
      moderation:
        provider: openai
        categories: [violence]
      action: block
      apply_to: request
    - name: emails
      builtin: email
      action: redact
"#,
    )
    .await;
    let mut b = hello("openai/echo");
    b["messages"][0]["content"] = json!("plan an attack");
    let r = h.chat(b, &[]).await;
    assert_eq!(r.status(), 400);
    assert_eq!(r.headers()["x-gateway-guardrails"], "violence");
    let mut b = hello("openai/echo");
    b["messages"][0]["content"] = json!("write to a@b.co");
    let r = h.chat(b, &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-gateway-guardrails"], "emails");
    let out: Value = r.json().await.unwrap();
    assert_eq!(out["choices"][0]["message"]["content"], "write to [REDACTED:emails]");
}

#[tokio::test]
async fn feedback_from_admin_and_client() {
    let h = start("").await;
    let r = h.chat(hello("openai/gpt-test"), &[]).await;
    let id = rid(&r);
    h.log(&id).await;
    let r = h
        .admin_send(
            reqwest::Method::POST,
            &format!("/api/logs/{id}/feedback"),
            Some(json!({"rating": 1, "score": 90})),
        )
        .await;
    assert_eq!(r.status(), 204);
    assert_eq!(h.log(&id).await["feedback"]["score"], 90);
    let r = h
        .post("/v1/feedback", KEY_FULL, json!({"request_id": id, "rating": -1, "comment": "wrong"}), &[])
        .await;
    assert_eq!(r.status(), 204);
    let e = h.log(&id).await;
    assert_eq!(e["feedback"]["rating"], -1);
    assert_eq!(e["feedback"]["comment"], "wrong");
    assert_eq!(
        h.post("/v1/feedback", KEY_OTHER, json!({"request_id": id, "rating": 1}), &[]).await.status(),
        404
    );
    let o = h.admin_get("/api/overview?range=1h").await;
    assert_eq!(o["totals"]["feedback"], 2);
    assert_eq!(o["models"][0]["thumbs_up"], 1);
    assert_eq!(o["models"][0]["thumbs_down"], 1);
}

#[tokio::test]
async fn delete_logs() {
    let h = start("").await;
    h.chat(hello("openai/a"), &[]).await;
    h.post("/v1/chat/completions", KEY_OTHER, hello("openai/b"), &[]).await;
    h.logs(2).await;
    let r = h.admin_send(reqwest::Method::DELETE, "/api/logs?tenant=other", None).await;
    assert_eq!(r.json::<Value>().await.unwrap()["deleted"], 1);
    assert_eq!(h.admin_get("/api/logs").await["total"], 1);
    let r = h.admin_send(reqwest::Method::DELETE, "/api/logs", None).await;
    assert_eq!(r.json::<Value>().await.unwrap()["deleted"], 1);
    assert_eq!(h.admin_get("/api/logs").await["total"], 0);
}

#[tokio::test]
async fn exports_logs_and_otel_traces() {
    let h = start(
        r#"logging:
  export:
    url: http://MOCK/export
    flush_secs: 1
  otlp:
    endpoint: http://MOCK/otel
    flush_secs: 1
"#,
    )
    .await;
    let r = h
        .chat(
            hello("openai/gpt-test"),
            &[("traceparent", "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")],
        )
        .await;
    let id = rid(&r);
    for _ in 0..60 {
        if !h.mock.exported.lock().is_empty() && !h.mock.otlp.lock().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let lines = h.mock.exported.lock().clone();
    let e: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(e["id"], id.as_str());
    let traces = h.mock.otlp.lock().clone();
    let spans = &traces[0]["resourceSpans"][0]["scopeSpans"][0]["spans"];
    assert_eq!(spans[0]["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736", "client trace is continued");
    assert_eq!(spans[0]["parentSpanId"], "00f067aa0ba902b7");
    assert_eq!(spans[1]["parentSpanId"], spans[0]["spanId"]);
}

#[tokio::test]
async fn config_reload_swaps_routes_and_keys() {
    let (mock, m) = start_mock().await;
    let dir = std::env::temp_dir().join(format!("aigw-reload-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(&path, config_yaml(m, "")).unwrap();
    let cfg = Config::load(path.to_str().unwrap()).unwrap();
    let state = app::AppState::build(cfg).await.unwrap();
    *state.config_path.write() = Some(path.to_str().unwrap().to_string());
    let gw = spawn(app::router(state.clone())).await;
    let admin = spawn(app::admin_api_router(state.clone())).await;
    let h = H {
        base: format!("http://{gw}"),
        admin: format!("http://{admin}"),
        mock,
        mock_addr: m,
        http: reqwest::Client::new(),
        state,
    };

    assert_eq!(h.chat(hello("fresh"), &[]).await.status(), 404);
    let new = config_yaml(m, "").replace(
        "routes:\n",
        "routes:\n  - name: fresh\n    targets:\n      - provider: anthropic\n        model: claude-test\n",
    );
    std::fs::write(&path, new).unwrap();
    let r = h.admin_send(reqwest::Method::POST, "/api/reload", None).await;
    assert_eq!(r.status(), 200);
    assert_eq!(h.chat(hello("fresh"), &[]).await.status(), 200);

    // A broken file is rejected and the running config stays.
    std::fs::write(&path, "providers: [").unwrap();
    assert_eq!(h.admin_send(reqwest::Method::POST, "/api/reload", None).await.status(), 400);
    assert_eq!(h.chat(hello("fresh"), &[]).await.status(), 200);
    let _ = (&h.mock_addr, &h.state);
}

#[tokio::test]
async fn realtime_websocket_relay_bills_usage() {
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as WsMessage};
    let h = start("").await;
    let url = format!("{}/v1/realtime?model=openai/gpt-rt", h.base.replace("http://", "ws://"));
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {KEY_FULL}").parse().unwrap());
    let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    let id = resp.headers()["x-request-id"].to_str().ok().map(str::to_string);
    ws.send(WsMessage::Text("ping-1".into())).await.unwrap();
    let echo = ws.next().await.unwrap().unwrap();
    assert_eq!(echo.into_text().unwrap(), "echo:ping-1");
    let done = ws.next().await.unwrap().unwrap();
    assert!(done.into_text().unwrap().contains("response.done"));
    ws.close(None).await.unwrap();
    drop(ws);

    let logs = h.logs(1).await;
    let e = logs.iter().find(|e| e["endpoint"] == "realtime").cloned().expect("realtime session logged");
    assert_eq!(e["prompt_tokens"], 3);
    assert_eq!(e["completion_tokens"], 7);
    let _ = id;

    // Unauthenticated sessions are refused before upgrading.
    let bad = format!("{}/v1/realtime?model=openai/gpt-rt", h.base.replace("http://", "ws://"));
    assert!(tokio_tungstenite::connect_async(bad).await.is_err());
}
