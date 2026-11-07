//! End-to-end tests for AWS Bedrock, Google Vertex AI and automatic model
//! pricing, against mock servers that check SigV4 signatures, Google token
//! exchange and the translated request bodies.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use ai_gateway::{
    app,
    auth::hash_key,
    aws::{self, AwsCredentials},
    config::Config,
};
use axum::{
    body::Bytes,
    extract::{OriginalUri, State},
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Form, Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use parking_lot::Mutex;
use serde_json::{json, Value};

const KEY: &str = "key-cloud";
const ADMIN: &str = "admin-cloud";

#[derive(Default)]
struct Mock {
    bedrock: Mutex<Vec<(String, Value)>>,
    vertex: Mutex<Vec<(String, Value)>>,
    token_hits: AtomicUsize,
    bad_signatures: AtomicUsize,
}

fn creds() -> AwsCredentials {
    AwsCredentials {
        access_key_id: "AKIDTEST".into(),
        secret_access_key: "secret/key+test".into(),
        session_token: Some("session-token".into()),
        expires_at: None,
    }
}

/// Recomputes the SigV4 signature from what arrived.
fn signature_ok(h: &HeaderMap, raw_path: &str, body: &[u8]) -> bool {
    let Some(auth) = h.get("authorization").and_then(|v| v.to_str().ok()) else { return false };
    let Some(amz) = h.get("x-amz-date").and_then(|v| v.to_str().ok()) else { return false };
    if h.get("x-amz-security-token").and_then(|v| v.to_str().ok()) != Some("session-token") {
        return false;
    }
    let host = h["host"].to_str().unwrap();
    let url = reqwest::Url::parse(&format!("http://{host}{raw_path}")).unwrap();
    let mut signed = reqwest::header::HeaderMap::new();
    for (k, v) in h {
        let k = k.as_str();
        if k == "host" || k == "content-type" || k.starts_with("x-amz-") {
            signed.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_bytes(v.as_bytes()).unwrap(),
            );
        }
    }
    let expected = aws::authorization(
        "POST",
        &url,
        &signed,
        &aws::sha256_hex(body),
        &creds(),
        "us-east-1",
        "bedrock",
        amz,
        &amz[..8],
    );
    expected == auth
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

async fn bedrock(
    State(m): State<Arc<Mock>>,
    OriginalUri(uri): OriginalUri,
    h: HeaderMap,
    body: Bytes,
) -> Response {
    let raw = uri.path().to_string();
    let rest = raw.strip_prefix("/bedrock/model/").unwrap();
    let (id, op) = rest.rsplit_once('/').unwrap();
    let id = percent_decode(id);
    let bearer = h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer bedrock-api-key");
    if !bearer && !signature_ok(&h, &raw, &body) {
        m.bad_signatures.fetch_add(1, Ordering::SeqCst);
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"message": "The request signature we calculated does not match"})),
        )
            .into_response();
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    m.bedrock.lock().push((format!("{id} {op}"), v));
    match op {
        "converse" => Json(json!({
            "output": {"message": {"role": "assistant", "content": [
                {"text": "Let me check."},
                {"toolUse": {"toolUseId": "tu1", "name": "weather", "input": {"city": "Pune"}}}
            ]}},
            "stopReason": "tool_use",
            "usage": {"inputTokens": 10, "outputTokens": 20, "totalTokens": 130, "cacheReadInputTokens": 100, "cacheWriteInputTokens": 0}
        }))
        .into_response(),
        "converse-stream" => {
            let mut out = Vec::new();
            let ev = |t: &str, p: Value| {
                aws::encode_event(&[(":event-type", t), (":content-type", "application/json"), (":message-type", "event")], p.to_string().as_bytes())
            };
            out.extend(ev("messageStart", json!({"role": "assistant"})));
            out.extend(ev("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "Hel"}})));
            out.extend(ev("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "lo"}})));
            out.extend(ev("contentBlockStop", json!({"contentBlockIndex": 0})));
            out.extend(ev("messageStop", json!({"stopReason": "end_turn"})));
            out.extend(ev("metadata", json!({"usage": {"inputTokens": 7, "outputTokens": 3}, "metrics": {"latencyMs": 5}})));
            ([(CONTENT_TYPE, "application/vnd.amazon.eventstream")], out).into_response()
        }
        "invoke" => Json(json!({"native": true, "model": id})).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn token(
    State(m): State<Arc<Mock>>,
    Form(f): Form<std::collections::HashMap<String, String>>,
) -> Response {
    assert_eq!(f["grant_type"], "urn:ietf:params:oauth:grant-type:jwt-bearer");
    let parts: Vec<&str> = f["assertion"].split('.').collect();
    assert_eq!(parts.len(), 3);
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["iss"], "gateway@p1.iam.gserviceaccount.com");
    assert_eq!(claims["scope"], "https://www.googleapis.com/auth/cloud-platform");
    m.token_hits.fetch_add(1, Ordering::SeqCst);
    Json(json!({"access_token": "ya29.test-token", "expires_in": 3600, "token_type": "Bearer"}))
        .into_response()
}

fn vertex_auth(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer ya29.test-token")
}

async fn vertex_openai(State(m): State<Arc<Mock>>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if !vertex_auth(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    m.vertex.lock().push(("openapi".into(), body.clone()));
    Json(json!({
        "id": "v1", "object": "chat.completion", "model": body["model"],
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "from gemini"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 500, "total_tokens": 1500}
    }))
    .into_response()
}

async fn vertex_anthropic(
    State(m): State<Arc<Mock>>,
    OriginalUri(uri): OriginalUri,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !vertex_auth(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let tail = uri.path().rsplit('/').next().unwrap().to_string();
    m.vertex.lock().push((tail.clone(), body.clone()));
    if tail.ends_with(":streamRawPredict") {
        let mut s = String::new();
        for e in [
            json!({"type": "message_start", "message": {"id": "m1", "model": "claude-test", "usage": {"input_tokens": 4, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "vertex claude"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ] {
            s.push_str(&format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()));
        }
        return ([(CONTENT_TYPE, "text/event-stream")], s).into_response();
    }
    Json(json!({
        "id": "msg1", "type": "message", "role": "assistant", "model": "claude-test",
        "content": [{"type": "text", "text": "hello from vertex claude"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 8, "output_tokens": 4}
    }))
    .into_response()
}

async fn openai(Json(body): Json<Value>) -> Response {
    Json(json!({
        "id": "c1", "object": "chat.completion", "model": body["model"],
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 2000, "completion_tokens": 1000}
    }))
    .into_response()
}

async fn prices() -> Json<Value> {
    Json(json!({
        "sample_spec": {"input_cost_per_token": 0, "output_cost_per_token": 0},
        "anthropic.claude-test-v1:0": {
            "input_cost_per_token": 3e-6, "output_cost_per_token": 1.5e-5,
            "cache_read_input_token_cost": 3e-7, "cache_creation_input_token_cost": 3.75e-6
        },
        "vertex_ai/gemini-test": {"input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
        "gpt-test": {"input_cost_per_token": 5e-7, "output_cost_per_token": 1e-6},
        "gpt-overridden": {"input_cost_per_token": 1e-3, "output_cost_per_token": 1e-3}
    }))
}

async fn spawn(router: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    addr
}

struct H {
    base: String,
    admin: String,
    mock: Arc<Mock>,
    http: reqwest::Client,
}

async fn start() -> H {
    let mock = Arc::new(Mock::default());
    let router = Router::new()
        .route("/bedrock/model/*rest", post(bedrock))
        .route("/token", post(token))
        .route(
            "/vertex/v1/projects/p1/locations/us-central1/endpoints/openapi/chat/completions",
            post(vertex_openai),
        )
        .route(
            "/vertex/v1/projects/p1/locations/us-central1/publishers/anthropic/models/*rest",
            post(vertex_anthropic),
        )
        .route("/openai/chat/completions", post(openai))
        .route("/prices.json", get(prices))
        .with_state(mock.clone());
    let m = spawn(router).await;

    let pem = include_str!("fixtures/test-service-account.pem");
    let key = json!({
        "type": "service_account",
        "client_email": "gateway@p1.iam.gserviceaccount.com",
        "private_key": pem,
        "token_uri": format!("http://{m}/token"),
    });
    let key_path = std::env::temp_dir().join(format!("aigw-sa-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&key_path, key.to_string()).unwrap();

    let yaml = format!(
        r#"
server:
  listen: 127.0.0.1:0
  admin_listen: 127.0.0.1:0
admin:
  token_sha256: "{admin}"
auto_providers: false
pricing:
  url: http://{m}/prices.json
  overrides:
    openai/gpt-overridden: {{input_per_mtok: 1.0, output_per_mtok: 2.0}}
providers:
  bedrock:
    kind: bedrock
    region: us-east-1
    base_url: http://{m}/bedrock
    aws_access_key_id: AKIDTEST
    aws_secret_access_key: secret/key+test
    aws_session_token: session-token
    max_retries: 0
  bedrockkey:
    kind: bedrock
    region: us-east-1
    base_url: http://{m}/bedrock
    api_key: bedrock-api-key
  vertex:
    kind: vertex
    project: p1
    region: us-central1
    base_url: http://{m}/vertex/v1
    credentials_file: "{key_path}"
  openai:
    kind: openai
    base_url: http://{m}/openai
    api_key: k
routes:
  - name: claude
    targets:
      - provider: bedrock
        model: anthropic.claude-test-v1:0
  - name: cheapest
    strategy: cost
    targets:
      - provider: bedrock
        model: anthropic.claude-test-v1:0
      - provider: openai
        model: gpt-test
keys:
  - name: cloud
    key_sha256: "{key}"
    allowed_models: ["*"]
"#,
        admin = hash_key(ADMIN),
        key = hash_key(KEY),
        key_path = key_path.display(),
    );
    let cfg = Config::from_yaml_str(&yaml).expect("valid config");
    let state = app::AppState::build(cfg).await.unwrap();
    let gw = spawn(app::router(state.clone())).await;
    let admin = spawn(app::admin_api_router(state)).await;
    let h = H {
        base: format!("http://{gw}"),
        admin: format!("http://{admin}"),
        mock,
        http: reqwest::Client::new(),
    };
    let r = h.http.post(format!("{}/api/pricing/refresh", h.admin)).bearer_auth(ADMIN).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["models"], 4, "price list loaded: {v}");
    h
}

impl H {
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.http.post(format!("{}{path}", self.base)).bearer_auth(KEY).json(&body).send().await.unwrap()
    }

    async fn log_for(&self, id: &str) -> Value {
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
        panic!("log {id} never appeared");
    }
}

fn rid(r: &reqwest::Response) -> String {
    r.headers()["x-request-id"].to_str().unwrap().to_string()
}

fn approx(v: &Value, expected: f64) {
    let got = v.as_f64().unwrap_or_else(|| panic!("no cost: {v}"));
    assert!((got - expected).abs() < 1e-9, "cost {got} != {expected}");
}

#[tokio::test]
async fn bedrock_converse_is_signed_translated_and_priced() {
    let h = start().await;
    let r = h
        .post(
            "/v1/chat/completions",
            json!({
                "model": "claude",
                "max_tokens": 50,
                "messages": [
                    {"role": "system", "content": "Be brief."},
                    {"role": "user", "content": "Weather in Pune?"}
                ],
                "tools": [{"type": "function", "function": {"name": "weather", "parameters": {"type": "object"}}}]
            }),
        )
        .await;
    assert_eq!(r.status(), 200, "{:?}", h.mock.bad_signatures);
    let id = rid(&r);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(v["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "weather");
    assert_eq!(v["usage"]["prompt_tokens"], 110);

    let (target, sent) = h.mock.bedrock.lock().last().cloned().unwrap();
    assert_eq!(target, "anthropic.claude-test-v1:0 converse");
    assert_eq!(sent["system"][0]["text"], "Be brief.");
    assert_eq!(sent["inferenceConfig"]["maxTokens"], 50);
    assert_eq!(sent["toolConfig"]["tools"][0]["toolSpec"]["name"], "weather");

    // 10 plain input at $3/M + 100 cache reads at $0.30/M + 20 output at $15/M.
    let log = h.log_for(&id).await;
    approx(&log["cost_usd"], (10.0 * 3.0 + 100.0 * 0.3 + 20.0 * 15.0) / 1e6);
    assert_eq!(log["provider"], "bedrock");
    assert_eq!(h.mock.bad_signatures.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn bedrock_stream_decodes_the_event_stream() {
    let h = start().await;
    let r = h
        .post(
            "/v1/chat/completions",
            json!({"model": "bedrock/anthropic.claude-test-v1:0", "stream": true,
                   "stream_options": {"include_usage": true},
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let id = rid(&r);
    let text = r.text().await.unwrap();
    let mut content = String::new();
    let mut usage = Value::Null;
    for line in text.lines().filter_map(|l| l.strip_prefix("data: ")) {
        if line == "[DONE]" {
            continue;
        }
        let v: Value = serde_json::from_str(line).unwrap();
        if let Some(c) = v["choices"][0]["delta"]["content"].as_str() {
            content.push_str(c);
        }
        if v.get("usage").is_some() {
            usage = v["usage"].clone();
        }
    }
    assert_eq!(content, "Hello");
    assert_eq!(usage["total_tokens"], 10);
    assert!(text.trim_end().ends_with("data: [DONE]"));
    let log = h.log_for(&id).await;
    assert_eq!(log["status"], 200);
    approx(&log["cost_usd"], (7.0 * 3.0 + 3.0 * 15.0) / 1e6);
    assert_eq!(h.mock.bedrock.lock().last().unwrap().0, "anthropic.claude-test-v1:0 converse-stream");
}

#[tokio::test]
async fn bedrock_api_key_and_native_passthrough() {
    let h = start().await;
    let r = h
        .post(
            "/v1/chat/completions",
            json!({"model": "bedrockkey/anthropic.claude-test-v1:0",
        "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(r.status(), 200, "Bedrock API keys are sent as bearer tokens");

    // Native passthrough is signed too.
    let r = h
        .http
        .post(format!("{}/native/bedrock/model/anthropic.claude-test-v1%3A0/invoke", h.base))
        .bearer_auth(KEY)
        .json(&json!({"anthropic_version": "bedrock-2023-05-31", "max_tokens": 5, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["model"], "anthropic.claude-test-v1:0");
    assert_eq!(h.mock.bad_signatures.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn vertex_gemini_and_claude_with_service_account() {
    let h = start().await;
    let r = h
        .post(
            "/v1/chat/completions",
            json!({"model": "vertex/gemini-test",
        "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let id = rid(&r);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "from gemini");
    let (kind, sent) = h.mock.vertex.lock().last().cloned().unwrap();
    assert_eq!(kind, "openapi");
    assert_eq!(sent["model"], "google/gemini-test");
    approx(&h.log_for(&id).await["cost_usd"], (1000.0 * 1.0 + 500.0 * 2.0) / 1e6);

    // Claude on Vertex: Anthropic format, model in the URL.
    let r = h
        .post(
            "/v1/chat/completions",
            json!({"model": "vertex/claude-test@1",
        "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hello from vertex claude");
    let (tail, sent) = h.mock.vertex.lock().last().cloned().unwrap();
    assert_eq!(tail, "claude-test@1:rawPredict");
    assert_eq!(sent["anthropic_version"], "vertex-2023-10-16");
    assert!(sent.get("model").is_none());

    // Streaming, and the native Messages API.
    let r = h
        .post(
            "/v1/messages",
            json!({"model": "vertex/claude-test@1", "max_tokens": 10, "stream": true,
        "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains("vertex claude") && text.contains("message_stop"), "{text}");
    assert_eq!(h.mock.vertex.lock().last().unwrap().0, "claude-test@1:streamRawPredict");

    // Gemini is not served through the Messages API.
    let r = h
        .post(
            "/v1/messages",
            json!({"model": "vertex/gemini-test", "max_tokens": 10,
        "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(r.status(), 400);

    assert_eq!(h.mock.token_hits.load(Ordering::SeqCst), 1, "the access token is cached");
}

#[tokio::test]
async fn pricing_list_overrides_status_and_cost_routing() {
    let h = start().await;
    let r = h
        .post(
            "/v1/chat/completions",
            json!({"model": "openai/gpt-overridden",
        "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let id = rid(&r);
    // The config override ($1/$2 per M) wins over the list ($1000/M).
    approx(&h.log_for(&id).await["cost_usd"], (2000.0 * 1.0 + 1000.0 * 2.0) / 1e6);

    let r = h
        .post(
            "/v1/chat/completions",
            json!({"model": "openai/unknown-model",
        "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let id = rid(&r);
    assert!(h.log_for(&id).await["cost_usd"].is_null(), "unknown models have no cost");

    // Cheapest first using list prices: gpt-test ($0.5/$1) beats Claude ($3/$15).
    let r = h
        .post(
            "/v1/chat/completions",
            json!({"model": "cheapest",
        "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let id = rid(&r);
    assert_eq!(h.log_for(&id).await["provider"], "openai");

    let st: Value = h
        .http
        .get(format!("{}/api/pricing", h.admin))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["models"], 4);
    let claude = st["targets"].as_array().unwrap().iter().find(|t| t["route"] == "claude").unwrap();
    assert_eq!(claude["source"], "list");
    assert_eq!(claude["price"]["input_per_mtok"], 3.0);

    let models: Value = h
        .http
        .get(format!("{}/v1/models", h.base))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let m = models["data"].as_array().unwrap().iter().find(|m| m["id"] == "claude").unwrap();
    assert_eq!(m["pricing"]["output_per_mtok"], 15.0);
}
