//! End-to-end tests: a real gateway on a random port in front of mock
//! OpenAI and Anthropic servers.

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
    extract::State,
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use parking_lot::Mutex;
use serde_json::{json, Value};

#[derive(Default)]
struct Mock {
    openai_hits: AtomicUsize,
    failing_hits: AtomicUsize,
    anthropic_hits: AtomicUsize,
    last_anthropic_body: Mutex<Option<Value>>,
}

async fn openai(State(m): State<Arc<Mock>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    if auth != "Bearer test-openai" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "Incorrect API key provided: sk-...abcd"}})),
        )
            .into_response();
    }
    let model = body["model"].as_str().unwrap_or("").to_string();
    match model.as_str() {
        "fail-500" => {
            m.failing_hits.fetch_add(1, Ordering::SeqCst);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "boom"}})))
                .into_response();
        }
        "bad-request" => {
            m.failing_hits.fetch_add(1, Ordering::SeqCst);
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": {"message": "bad param", "type": "invalid_request_error", "code": "x"}}),
                ),
            )
                .into_response();
        }
        _ => {}
    }
    m.openai_hits.fetch_add(1, Ordering::SeqCst);

    if body["stream"] == true {
        assert_eq!(body["stream_options"]["include_usage"], true, "gateway must request stream usage");
        let chunks = [
            json!({"id": "c1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hel"}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}),
        ];
        let mut s = String::new();
        for c in chunks {
            s.push_str(&format!("data: {c}\n\n"));
        }
        s.push_str("data: [DONE]\n\n");
        return ([(CONTENT_TYPE, "text/event-stream")], s).into_response();
    }

    Json(json!({
        "id": "cmpl-1", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi there"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
    }))
    .into_response()
}

async fn anthropic(State(m): State<Arc<Mock>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some("test-anthropic")
        || headers.get("anthropic-version").is_none()
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"type": "error", "error": {"type": "authentication_error", "message": "bad key"}})),
        )
            .into_response();
    }
    m.anthropic_hits.fetch_add(1, Ordering::SeqCst);
    *m.last_anthropic_body.lock() = Some(body.clone());

    if body["stream"] == true {
        let events = [
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-test", "usage": {"input_tokens": 12, "output_tokens": 1}}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            ("ping", json!({"type": "ping"})),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": " world"}}),
            ),
            ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 4}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ];
        let mut s = String::new();
        for (name, data) in events {
            s.push_str(&format!("event: {name}\r\ndata: {data}\r\n\r\n"));
        }
        return ([(CONTENT_TYPE, "text/event-stream")], s).into_response();
    }

    if body.get("tools").is_some() {
        return Json(json!({
            "id": "msg_2", "type": "message", "role": "assistant", "model": "claude-test",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Pune"}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 50, "output_tokens": 10}
        }))
        .into_response();
    }

    Json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-test",
        "content": [{"type": "text", "text": "Hello from Claude"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 8, "output_tokens": 6}
    }))
    .into_response()
}

async fn spawn(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

struct Harness {
    base: String,
    mock: Arc<Mock>,
    http: reqwest::Client,
}

const KEY_FULL: &str = "key-full";
const KEY_LIMITED: &str = "key-limited";
const KEY_BUDGET: &str = "key-budget";
const KEY_RESTRICTED: &str = "key-restricted";

async fn start() -> Harness {
    let mock = Arc::new(Mock::default());
    let mock_router = Router::new()
        .route("/openai/chat/completions", post(openai))
        .route("/anthropic/messages", post(anthropic))
        .with_state(mock.clone());
    let m = spawn(mock_router).await;

    // With AIGW_TEST_REDIS_URL set (as in CI), every test runs against real
    // Redis instead of the in-memory store, each under its own key prefix.
    let store = match std::env::var("AIGW_TEST_REDIS_URL") {
        Ok(url) => format!(
            "store:\n  backend: redis\n  redis_url: \"{url}\"\n  key_prefix: \"aigw-test-{}:\"\n",
            uuid::Uuid::new_v4()
        ),
        Err(_) => String::new(),
    };
    let yaml = format!(
        r#"
{store}
server:
  listen: 127.0.0.1:0
  admin_listen: 127.0.0.1:0
  max_body_bytes: 8192
  max_concurrent_requests: 64
cache:
  enabled: true
  ttl_secs: 60
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
    breaker_failure_threshold: 2
    breaker_open_secs: 60
  badkey:
    kind: openai
    base_url: http://{m}/openai
    api_key: wrong-key
    max_retries: 2
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
  - name: creds
    targets:
      - provider: badkey
        model: gpt-test
      - provider: openai
        model: gpt-test
keys:
  - name: full
    key_sha256: "{full}"
    allowed_models: ["*"]
  - name: limited
    key_sha256: "{limited}"
    requests_per_minute: 2
    allowed_models: ["*"]
  - name: budget
    key_sha256: "{budget}"
    daily_token_budget: 25
    allowed_models: ["*"]
  - name: restricted
    key_sha256: "{restricted}"
    allowed_models: ["smart"]
"#,
        full = hash_key(KEY_FULL),
        limited = hash_key(KEY_LIMITED),
        budget = hash_key(KEY_BUDGET),
        restricted = hash_key(KEY_RESTRICTED),
    );
    let cfg = Config::from_yaml_str(&yaml).expect("valid test config");
    let state = app::AppState::build(cfg).await.unwrap();
    let gw = spawn(app::router(state)).await;
    Harness { base: format!("http://{gw}"), mock, http: reqwest::Client::new() }
}

impl Harness {
    async fn chat(&self, key: Option<&str>, body: Value) -> reqwest::Response {
        let mut rb = self.http.post(format!("{}/v1/chat/completions", self.base)).json(&body);
        if let Some(k) = key {
            rb = rb.bearer_auth(k);
        }
        rb.send().await.unwrap()
    }
}

fn hello(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hello"}]})
}

fn sse_payloads(text: &str) -> Vec<String> {
    text.split("\n\n").filter_map(|e| e.strip_prefix("data: ")).map(str::to_string).collect()
}

#[tokio::test]
async fn rejects_missing_and_invalid_keys() {
    let h = start().await;
    let r = h.chat(None, hello("openai/gpt-test")).await;
    assert_eq!(r.status(), 401);
    assert!(r.headers().get("x-request-id").is_some());
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_api_key");

    let r = h.chat(Some("nope"), hello("openai/gpt-test")).await;
    assert_eq!(r.status(), 401);
    assert_eq!(h.mock.openai_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn proxies_openai_non_streaming() {
    let h = start().await;
    let r = h.chat(Some(KEY_FULL), hello("openai/gpt-test")).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-gateway-provider"], "openai");
    assert_eq!(r.headers()["x-gateway-model"], "gpt-test");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hi there");
    assert_eq!(body["usage"]["total_tokens"], 30);
}

#[tokio::test]
async fn validates_requests() {
    let h = start().await;
    let r = h.chat(Some(KEY_FULL), json!({"messages": [{"role": "user", "content": "x"}]})).await;
    assert_eq!(r.status(), 400);
    let r = h.chat(Some(KEY_FULL), json!({"model": "openai/gpt-test", "messages": []})).await;
    assert_eq!(r.status(), 400);
    let r = h.chat(Some(KEY_FULL), hello("nothing/here")).await;
    assert_eq!(r.status(), 404);
    let r = h.chat(Some(KEY_FULL), hello("unknown-route")).await;
    assert_eq!(r.status(), 404);

    let big = "x".repeat(10_000);
    let big_body = json!({"model": "openai/gpt-test", "messages": [{"role": "user", "content": big}]});
    let r = h.chat(Some(KEY_FULL), big_body.clone()).await;
    assert_eq!(r.status(), 413);
    // Unauthenticated callers are rejected before the body is read.
    let r = h.chat(None, big_body).await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn enforces_model_permissions() {
    let h = start().await;
    let r = h.chat(Some(KEY_RESTRICTED), hello("openai/gpt-test")).await;
    assert_eq!(r.status(), 403);
    let r = h.chat(Some(KEY_RESTRICTED), hello("smart")).await;
    assert_eq!(r.status(), 200);

    let r = h.http.get(format!("{}/v1/models", h.base)).bearer_auth(KEY_RESTRICTED).send().await.unwrap();
    let body: Value = r.json().await.unwrap();
    let ids: Vec<&str> = body["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["smart"]);
}

#[tokio::test]
async fn retries_then_falls_back_and_opens_circuit() {
    let h = start().await;
    let r = h.chat(Some(KEY_FULL), hello("smart")).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-gateway-provider"], "anthropic");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "Hello from Claude");
    assert_eq!(body["usage"]["total_tokens"], 14);
    // max_retries: 1 => two attempts, which also reaches the breaker threshold of 2.
    assert_eq!(h.mock.failing_hits.load(Ordering::SeqCst), 2);

    let r = h
        .chat(Some(KEY_FULL), json!({"model": "smart", "messages": [{"role": "user", "content": "again"}]}))
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(h.mock.failing_hits.load(Ordering::SeqCst), 2, "open circuit skips the failing provider");
    assert_eq!(h.mock.anthropic_hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn falls_back_when_provider_rejects_credentials() {
    let h = start().await;
    let r = h.chat(Some(KEY_FULL), hello("creds")).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-gateway-provider"], "openai");
}

#[tokio::test]
async fn upstream_auth_details_are_not_leaked() {
    let h = start().await;
    let r = h.chat(Some(KEY_FULL), hello("badkey/gpt-test")).await;
    assert_eq!(r.status(), 502);
    let text = r.text().await.unwrap();
    assert!(!text.contains("sk-"), "upstream key hints must not reach clients: {text}");
}

#[tokio::test]
async fn passes_client_errors_through_without_retry() {
    let h = start().await;
    let r = h.chat(Some(KEY_FULL), hello("flaky/bad-request")).await;
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["message"], "bad param");
    assert_eq!(h.mock.failing_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn streams_openai_and_hides_injected_usage() {
    let h = start().await;
    let r = h
        .chat(Some(KEY_FULL), json!({"model": "openai/gpt-test", "stream": true, "messages": [{"role": "user", "content": "hi"}]}))
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()[CONTENT_TYPE.as_str()], "text/event-stream");
    let payloads = sse_payloads(&r.text().await.unwrap());
    assert_eq!(payloads.last().unwrap(), "[DONE]");
    let content: String = payloads[..payloads.len() - 1]
        .iter()
        .map(|p| serde_json::from_str::<Value>(p).unwrap())
        .filter_map(|v| v["choices"][0]["delta"]["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(content, "Hello");
    assert!(!payloads.iter().any(|p| p.contains("\"usage\"")));

    let r = h
        .chat(
            Some(KEY_FULL),
            json!({"model": "openai/gpt-test", "stream": true, "stream_options": {"include_usage": true},
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let payloads = sse_payloads(&r.text().await.unwrap());
    assert!(payloads.iter().any(|p| p.contains("\"total_tokens\":7")));
}

#[tokio::test]
async fn streams_anthropic_as_openai_chunks() {
    let h = start().await;
    let r = h
        .chat(
            Some(KEY_FULL),
            json!({"model": "anthropic/claude-test", "stream": true, "stream_options": {"include_usage": true},
                   "messages": [{"role": "system", "content": "be nice"}, {"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let payloads = sse_payloads(&r.text().await.unwrap());
    assert_eq!(payloads.last().unwrap(), "[DONE]");
    let chunks: Vec<Value> =
        payloads[..payloads.len() - 1].iter().map(|p| serde_json::from_str(p).unwrap()).collect();
    let content: String = chunks
        .iter()
        .filter_map(|v| v["choices"][0]["delta"]["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(content, "Hello world");
    assert!(chunks.iter().any(|c| c["choices"][0]["finish_reason"] == "stop"));
    let usage = chunks.iter().find(|c| c.get("usage").is_some()).unwrap();
    assert_eq!(usage["usage"]["prompt_tokens"], 12);
    assert_eq!(usage["usage"]["completion_tokens"], 4);

    let sent = h.mock.last_anthropic_body.lock().clone().unwrap();
    assert_eq!(sent["system"], "be nice");
    assert_eq!(sent["stream"], true);
}

#[tokio::test]
async fn translates_anthropic_tool_calls() {
    let h = start().await;
    let r = h
        .chat(
            Some(KEY_FULL),
            json!({
                "model": "anthropic/claude-test",
                "messages": [{"role": "user", "content": "weather in Pune?"}],
                "tools": [{"type": "function", "function": {"name": "get_weather", "description": "Weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]
            }),
        )
        .await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "get_weather");
    let args: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Pune");

    let sent = h.mock.last_anthropic_body.lock().clone().unwrap();
    assert_eq!(sent["tools"][0]["input_schema"]["type"], "object");
}

#[tokio::test]
async fn caches_identical_requests_per_tenant() {
    let h = start().await;
    let body = json!({"model": "openai/gpt-test", "temperature": 0, "messages": [{"role": "user", "content": "cache me"}]});
    let r1 = h.chat(Some(KEY_FULL), body.clone()).await;
    assert_eq!(r1.headers()["x-gateway-cache"], "MISS");
    let b1: Value = r1.json().await.unwrap();
    let r2 = h.chat(Some(KEY_FULL), body.clone()).await;
    assert_eq!(r2.headers()["x-gateway-cache"], "HIT");
    let b2: Value = r2.json().await.unwrap();
    assert_eq!(b1, b2);
    assert_eq!(h.mock.openai_hits.load(Ordering::SeqCst), 1);

    // Another tenant never sees the first tenant's cache entry.
    let r3 = h.chat(Some(KEY_LIMITED), body.clone()).await;
    assert_eq!(r3.headers()["x-gateway-cache"], "MISS");

    // Explicit bypass.
    let r4 = h
        .http
        .post(format!("{}/v1/chat/completions", h.base))
        .bearer_auth(KEY_FULL)
        .header("x-gateway-cache", "skip")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(r4.headers().get("x-gateway-cache").is_none());
    assert_eq!(h.mock.openai_hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn enforces_rate_limit() {
    // Avoid straddling a minute boundary, which would reset the window mid-test.
    let secs = ai_gateway::proxy::unix_secs() % 60;
    if secs >= 57 {
        tokio::time::sleep(Duration::from_secs(61 - secs)).await;
    }
    let h = start().await;
    for i in 0..2 {
        let r = h.chat(Some(KEY_LIMITED), hello(&format!("openai/gpt-test-{i}"))).await;
        assert_eq!(r.status(), 200);
    }
    let r = h.chat(Some(KEY_LIMITED), hello("openai/gpt-test")).await;
    assert_eq!(r.status(), 429);
    assert!(r.headers().get("retry-after").is_some());
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");
}

#[tokio::test]
async fn enforces_daily_token_budget() {
    let h = start().await;
    let r = h.chat(Some(KEY_BUDGET), hello("openai/gpt-test")).await;
    assert_eq!(r.status(), 200); // uses 30 tokens against a budget of 25
    tokio::time::sleep(Duration::from_millis(200)).await; // usage is recorded asynchronously
    let r = h.chat(Some(KEY_BUDGET), hello("openai/gpt-test-2")).await;
    assert_eq!(r.status(), 429);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "budget_exceeded");
}

#[tokio::test]
async fn streaming_usage_counts_toward_budget() {
    let h = start().await;
    let stream_body = json!({"model": "anthropic/claude-test", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    let _ = h.chat(Some(KEY_BUDGET), stream_body.clone()).await.text().await.unwrap(); // 16 tokens
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = h.chat(Some(KEY_BUDGET), stream_body.clone()).await.text().await.unwrap(); // 32 total
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = h.chat(Some(KEY_BUDGET), stream_body).await;
    assert_eq!(r.status(), 429);
}

#[tokio::test]
async fn health_endpoints() {
    let h = start().await;
    let r = h.http.get(format!("{}/healthz", h.base)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = h.http.get(format!("{}/readyz", h.base)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
}
