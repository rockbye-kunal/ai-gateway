//! End-to-end tests for zero-config providers, the live model catalog, and
//! providers and routes managed from the dashboard.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use ai_gateway::{app, auth::hash_key, config::Config};
use axum::{
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use parking_lot::Mutex;
use serde_json::{json, Value};

const KEY: &str = "key-dynamic";
const NARROW: &str = "key-narrow";
const ADMIN: &str = "admin-dynamic";
const ENC_KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

#[derive(Default)]
struct Mock {
    /// (path prefix, model, authorization) of every chat call.
    calls: Mutex<Vec<(String, String, String)>>,
    list_hits: Mutex<HashMap<String, usize>>,
    anthropic_list_fails: AtomicUsize,
}

fn auth_of(h: &HeaderMap) -> String {
    h.get("authorization")
        .or_else(|| h.get("x-api-key"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

async fn chat(
    State(m): State<Arc<Mock>>,
    OriginalUri(uri): OriginalUri,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let prefix = uri.path().split('/').nth(1).unwrap_or("").to_string();
    let model = body["model"].as_str().unwrap_or("").to_string();
    m.calls.lock().push((prefix.clone(), model.clone(), auth_of(&h)));
    Json(json!({
        "id": "c1", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": format!("{prefix} answered")}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 100}
    }))
    .into_response()
}

async fn models(State(m): State<Arc<Mock>>, OriginalUri(uri): OriginalUri, h: HeaderMap) -> Response {
    let prefix = uri.path().split('/').nth(1).unwrap_or("").to_string();
    *m.list_hits.lock().entry(prefix.clone()).or_default() += 1;
    let auth = auth_of(&h);
    if auth.is_empty() || auth == "Bearer " {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match prefix.as_str() {
        "openai" => Json(json!({"object": "list", "data": [
            {"id": "gpt-4o-mini", "created": 1721172741, "owned_by": "system"},
            {"id": "gpt-4.1", "created": 1744316542, "owned_by": "system"}
        ]})),
        "router" => Json(json!({"data": [
            {"id": "openai/gpt-4o-mini", "name": "GPT-4o mini", "context_length": 128000,
             "pricing": {"prompt": "0.00000015", "completion": "0.0000006"}},
            {"id": "acme/secret-model", "context_length": 8000,
             "pricing": {"prompt": "0.000001", "completion": "0.000002"}}
        ]})),
        "groq" => Json(json!({"data": [{"id": "llama-3.3-70b-versatile", "context_window": 131072}]})),
        "anthropic" => {
            if m.anthropic_list_fails.load(Ordering::SeqCst) > 0 {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            Json(json!({"data": [{"id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5"}]}))
        }
        _ => Json(json!({"data": []})),
    }
    .into_response()
}

async fn prices() -> Json<Value> {
    Json(json!({"gpt-4o-mini": {"input_cost_per_token": 1.5e-7, "output_cost_per_token": 6e-7}}))
}

async fn spawn(router: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    addr
}

struct H {
    base: String,
    /// The mock provider server, `http://127.0.0.1:port`.
    mock_url: String,
    admin: String,
    mock: Arc<Mock>,
    http: reqwest::Client,
    state: Arc<app::AppState>,
}

async fn start() -> H {
    let mock = Arc::new(Mock::default());
    let mut r = Router::new().route("/prices.json", get(prices));
    for p in ["openai", "router", "groq", "local", "corp"] {
        r = r
            .route(&format!("/{p}/chat/completions"), post(chat))
            .route(&format!("/{p}/models"), get(models));
    }
    r = r.route("/anthropic/models", get(models));
    let m = spawn(r.with_state(mock.clone())).await;

    // Built-in providers read their keys and endpoints from this
    // environment, as they would from the process environment.
    let env: HashMap<String, String> = [
        ("OPENAI_API_KEY", "sk-env"),
        ("OPENAI_BASE_URL", &format!("http://{m}/openai")),
        ("OPENROUTER_API_KEY", "or-env"),
        ("OPENROUTER_BASE_URL", &format!("http://{m}/router")),
        ("GROQ_BASE_URL", &format!("http://{m}/groq")),
        ("ANTHROPIC_BASE_URL", &format!("http://{m}/anthropic")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();

    // No providers section: everything comes from the built-in list, except
    // one config-file provider used to check that the file wins.
    let store = match std::env::var("AIGW_TEST_REDIS_URL") {
        Ok(url) => format!(
            "store:\n  backend: redis\n  redis_url: \"{url}\"\n  key_prefix: \"aigw-dynamic-{}:\"\n",
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
admin:
  token_sha256: "{admin}"
  encryption_key: "{ENC_KEY}"
pricing:
  url: http://{m}/prices.json
default_provider: openrouter
providers:
  corp:
    kind: openai
    base_url: http://{m}/corp
    api_key: corp-key
routes:
  - name: fixed
    targets:
      - provider: openai
        model: gpt-4o-mini
keys:
  - name: full
    key_sha256: "{full}"
    allowed_models: ["*"]
  - name: narrow
    key_sha256: "{narrow}"
    allowed_models: ["openai/*"]
"#,
        admin = hash_key(ADMIN),
        full = hash_key(KEY),
        narrow = hash_key(NARROW),
    );
    let cfg = Config::from_yaml_str_with_env(&yaml, &|k| env.get(k).cloned()).expect("valid config");
    let state = app::AppState::build(cfg).await.unwrap();
    let gw = spawn(app::router(state.clone())).await;
    let admin = spawn(app::admin_api_router(state.clone())).await;
    let h = H {
        state,
        base: format!("http://{gw}"),
        mock_url: format!("http://{m}"),
        admin: format!("http://{admin}"),
        mock,
        http: reqwest::Client::new(),
    };
    h.admin_req(reqwest::Method::POST, "/api/pricing/refresh", None).await;
    h
}

impl H {
    async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.admin_req(reqwest::Method::PUT, path, Some(body)).await
    }

    async fn chat(&self, key: &str, model: &str, extra: Value) -> (StatusCode, HeaderMap, Value) {
        let mut body =
            json!({"model": model, "messages": [{"role": "user", "content": format!("hi {model} {extra}")}]});
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        let r = self
            .http
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(key)
            .header("x-gateway-skip-cache", "true")
            .json(&body)
            .send()
            .await
            .unwrap();
        let (s, h) = (r.status(), r.headers().clone());
        (s, h, r.json().await.unwrap_or(Value::Null))
    }

    async fn admin_req(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut rb = self.http.request(method, format!("{}{path}", self.admin)).bearer_auth(ADMIN);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let s = r.status();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn models(&self, key: &str, query: &str) -> Vec<Value> {
        let v: Value = self
            .http
            .get(format!("{}/v1/models{query}", self.base))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["data"].as_array().cloned().unwrap_or_default()
    }

    fn last_call(&self) -> (String, String, String) {
        self.mock.calls.lock().last().cloned().unwrap()
    }

    fn hits(&self, p: &str) -> usize {
        self.mock.list_hits.lock().get(p).copied().unwrap_or(0)
    }
}

fn provider_of(h: &HeaderMap) -> &str {
    h.get("x-gateway-provider").and_then(|v| v.to_str().ok()).unwrap_or("")
}

#[tokio::test]
async fn built_in_providers_work_without_config() {
    let h = start().await;

    // OPENAI_API_KEY is set: openai/<model> goes straight to OpenAI.
    let (s, hd, _) = h.chat(KEY, "openai/gpt-4o-mini", json!({})).await;
    assert_eq!(s, 200);
    assert_eq!(provider_of(&hd), "openai");
    assert_eq!(h.last_call(), ("openai".into(), "gpt-4o-mini".into(), "Bearer sk-env".into()));

    // Groq has no key: its models go to the default provider unchanged.
    let (s, hd, _) = h.chat(KEY, "groq/llama-3.3-70b-versatile", json!({})).await;
    assert_eq!(s, 200);
    assert_eq!(provider_of(&hd), "openrouter");
    assert_eq!(h.last_call().1, "groq/llama-3.3-70b-versatile");

    // A key the client brings enables Groq for that request.
    let byok = json!({"providerOptions": {"gateway": {"byok": {"groq": [{"apiKey": "gsk-client"}]}}}});
    let (s, hd, _) = h.chat(KEY, "groq/llama-3.3-70b-versatile", byok).await;
    assert_eq!(s, 200);
    assert_eq!(provider_of(&hd), "groq");
    assert_eq!(h.last_call(), ("groq".into(), "llama-3.3-70b-versatile".into(), "Bearer gsk-client".into()));

    // A key stored in the dashboard enables it for everyone.
    let (s, _) = h
        .admin_req(reqwest::Method::PUT, "/api/provider-keys/groq", Some(json!({"api_key": "gsk-stored"})))
        .await;
    assert_eq!(s, 204);
    let (_, hd, _) = h.chat(KEY, "groq/llama-3.3-70b-versatile", json!({})).await;
    assert_eq!(provider_of(&hd), "groq");
    assert_eq!(h.last_call().2, "Bearer gsk-stored");

    // A config-file provider is used as written.
    let (_, hd, _) = h.chat(KEY, "corp/internal", json!({})).await;
    assert_eq!(provider_of(&hd), "corp");
    assert_eq!(h.last_call().2, "Bearer corp-key");

    // Native passthrough refuses a built-in provider that has no key.
    let r = h.http.get(format!("{}/native/anthropic/models", h.base)).bearer_auth(KEY).send().await.unwrap();
    assert_eq!(r.status(), 503);

    // The provider list shows where each key comes from.
    let (s, v) = h.admin_req(reqwest::Method::GET, "/api/providers", None).await;
    assert_eq!(s, 200);
    let find = |n: &str| v["data"].as_array().unwrap().iter().find(|p| p["name"] == n).cloned().unwrap();
    assert_eq!(find("openai")["key"], "environment");
    assert_eq!(find("openai")["source"], "builtin");
    assert_eq!(find("groq")["key"], "stored");
    assert_eq!(find("corp")["source"], "config");
    assert_eq!(find("mistral")["ready"], false);
    assert_eq!(find("openrouter")["default"], true);
    assert_eq!(find("anthropic")["label"], "Anthropic");
}

#[tokio::test]
async fn models_list_comes_from_the_providers() {
    let h = start().await;
    let all = h.models(KEY, "").await;
    let ids: Vec<&str> = all.iter().filter_map(|m| m["id"].as_str()).collect();
    // Routes first, then every model of every provider with a key.
    assert_eq!(ids[0], "fixed");
    for want in [
        "openai/gpt-4o-mini",
        "openai/gpt-4.1",
        "openrouter/openai/gpt-4o-mini",
        "openrouter/acme/secret-model",
    ] {
        assert!(ids.contains(&want), "{want} missing from {ids:?}");
    }
    assert!(!ids.iter().any(|i| i.starts_with("groq/")), "providers without a key are not listed");
    assert!(!ids.iter().any(|i| i.starts_with("anthropic/")));

    let entry = |id: &str| all.iter().find(|m| m["id"] == id).cloned().unwrap();
    // Price from the price list, or from the provider's own list.
    assert_eq!(entry("openai/gpt-4o-mini")["pricing"]["input_per_mtok"], 0.15);
    assert_eq!(entry("openai/gpt-4o-mini")["pricing"]["source"], "list");
    assert_eq!(entry("openrouter/acme/secret-model")["pricing"]["output_per_mtok"], 2.0);
    assert_eq!(entry("openrouter/acme/secret-model")["pricing"]["source"], "provider");
    assert_eq!(entry("openrouter/openai/gpt-4o-mini")["context_length"], 128000);
    assert_eq!(entry("openai/gpt-4.1")["owned_by"], "openai");

    // Cached: asking again doesn't call the providers.
    let before = h.hits("openai");
    h.models(KEY, "").await;
    assert_eq!(h.hits("openai"), before);

    // A key only sees models it may use, and the list can be filtered.
    let narrow: Vec<String> =
        h.models(NARROW, "").await.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect();
    assert!(!narrow.is_empty() && narrow.iter().all(|i| i.starts_with("openai/")), "{narrow:?}");
    let only: Vec<Value> = h.models(KEY, "?provider=openrouter").await;
    assert_eq!(only.len(), 2);

    // Every listed model can be called as listed.
    let (s, hd, _) = h.chat(KEY, "openrouter/acme/secret-model", json!({})).await;
    assert_eq!((s.as_u16(), provider_of(&hd)), (200, "openrouter"));
    assert_eq!(h.last_call().1, "acme/secret-model");

    // A provider gets listed once it has a key; a failing list is reported
    // without hiding the others.
    h.mock.anthropic_list_fails.store(1, Ordering::SeqCst);
    for (p, k) in [("groq", "gsk"), ("anthropic", "sk-ant")] {
        h.admin_req(reqwest::Method::PUT, &format!("/api/provider-keys/{p}"), Some(json!({"api_key": k})))
            .await;
    }
    let (s, v) = h.admin_req(reqwest::Method::POST, "/api/models/refresh", None).await;
    assert_eq!(s, 200);
    let status =
        |p: &str| v["providers"].as_array().unwrap().iter().find(|x| x["provider"] == p).cloned().unwrap();
    assert_eq!(status("groq")["models"], 1);
    assert!(status("anthropic")["error"].as_str().unwrap().contains("500"));
    assert_eq!(status("openai")["models"], 2);
    assert!(v["data"].as_array().unwrap().iter().any(|m| m["id"] == "groq/llama-3.3-70b-versatile"));
    assert!(h.hits("openai") > before, "refresh asks again");
}

#[tokio::test]
async fn providers_and_routes_from_the_dashboard() {
    let h = start().await;

    // Add a provider with its key.
    let local = format!("{}/local", h.mock_url);
    let (s, v) = h
        .put("/api/providers/local", json!({"kind": "openai", "base_url": local, "api_key": "loc-key"}))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["ready"], true);
    let (s, hd, _) = h.chat(KEY, "local/my-model", json!({})).await;
    assert_eq!((s.as_u16(), provider_of(&hd)), (200, "local"));
    assert_eq!(h.last_call(), ("local".into(), "my-model".into(), "Bearer loc-key".into()));

    // A route over it: Anthropic has no key, so it is skipped for the next target.
    let route = json!({"targets": [
        {"provider": "anthropic", "model": "claude-haiku-4-5"},
        {"provider": "local", "model": "fallback-model"}
    ]});
    let (s, v) = h.put("/api/routes/smart", route).await;
    assert_eq!(s, 200, "{v}");
    let (s, hd, _) = h.chat(KEY, "smart", json!({})).await;
    assert_eq!((s.as_u16(), provider_of(&hd)), (200, "local"));
    assert_eq!(h.last_call().1, "fallback-model");

    // Mistakes are refused with a reason.
    let (s, v) = h.put("/api/routes/bad", json!({"targets": [{"provider": "nope", "model": "x"}]})).await;
    assert_eq!(s, 400);
    assert!(v["error"]["message"].as_str().unwrap().contains("unknown provider"), "{v}");
    let (s, _) = h.put("/api/routes/fixed", json!({"targets": [{"provider": "local", "model": "x"}]})).await;
    assert_eq!(s, 409, "config-file routes are changed in the file");
    let (s, _) =
        h.put("/api/providers/corp", json!({"kind": "openai", "base_url": "https://x.example/v1"})).await;
    assert_eq!(s, 409, "config-file providers are changed in the file");
    let (s, _) = h
        .put("/api/providers/bad%20name", json!({"kind": "openai", "base_url": "https://x.example/v1"}))
        .await;
    assert_eq!(s, 400);
    let (s, _) =
        h.put("/api/providers/plain", json!({"kind": "openai", "base_url": "http://example.com/v1"})).await;
    assert_eq!(s, 400, "plain http to a remote host is refused");

    // Listed with where they come from.
    let (_, v) = h.admin_req(reqwest::Method::GET, "/api/routes", None).await;
    let src =
        |n: &str| v["data"].as_array().unwrap().iter().find(|r| r["name"] == n).unwrap()["source"].clone();
    assert_eq!(src("smart"), "dashboard");
    assert_eq!(src("fixed"), "config");
    let (_, v) = h.admin_req(reqwest::Method::GET, "/api/providers", None).await;
    let local = v["data"].as_array().unwrap().iter().find(|p| p["name"] == "local").cloned().unwrap();
    assert_eq!((local["source"].as_str(), local["key"].as_str()), (Some("dashboard"), Some("stored")));
    assert!(h.models(KEY, "").await.iter().any(|m| m["id"] == "smart"));

    // A provider in use by a route can't be removed until the route is.
    let (s, _) = h.admin_req(reqwest::Method::DELETE, "/api/providers/local", None).await;
    assert_eq!(s, 409);
    let (s, _) = h.admin_req(reqwest::Method::DELETE, "/api/routes/smart", None).await;
    assert_eq!(s, 204);
    let (s, _) = h.admin_req(reqwest::Method::DELETE, "/api/routes/fixed", None).await;
    assert_eq!(s, 404, "config-file routes can't be deleted here");
    let (s, _) = h.admin_req(reqwest::Method::DELETE, "/api/providers/local", None).await;
    assert_eq!(s, 204);
    let (_, hd, _) = h.chat(KEY, "local/my-model", json!({})).await;
    assert_eq!(provider_of(&hd), "openrouter", "unknown providers fall back to the default");

    // A built-in provider can be pointed somewhere else from the dashboard,
    // but its environment key never follows it to the new host.
    let corp = format!("{}/corp", h.mock_url);
    let (s, v) = h.put("/api/providers/openai", json!({"kind": "openai", "base_url": corp})).await;
    assert_eq!(s, 400, "a new endpoint needs its own key: {v}");
    let (s, _) = h
        .put("/api/providers/openai", json!({"kind": "openai", "base_url": corp, "api_key": "corp-key-2"}))
        .await;
    assert_eq!(s, 200);
    let (_, hd, _) = h.chat(KEY, "openai/gpt-4o-mini", json!({})).await;
    assert_eq!(provider_of(&hd), "openai");
    assert_eq!(h.last_call(), ("corp".into(), "gpt-4o-mini".into(), "Bearer corp-key-2".into()));

    // Bedrock and Vertex sign with the gateway's own cloud credentials, so
    // their endpoint can't be changed here.
    let (s, _) = h
        .put(
            "/api/providers/aws",
            json!({"kind": "bedrock", "region": "us-east-1", "base_url": "https://x.example"}),
        )
        .await;
    assert_eq!(s, 400);

    // Every change is in the audit log.
    let (_, v) = h.admin_req(reqwest::Method::GET, "/api/audit", None).await;
    let actions: Vec<&str> =
        v["data"].as_array().unwrap().iter().filter_map(|e| e["action"].as_str()).collect();
    for a in ["provider.set", "route.set", "route.delete", "provider.delete"] {
        assert!(actions.contains(&a), "{a} missing from {actions:?}");
    }
}

#[test]
fn config_needs_no_providers_or_keys_with_an_admin_token() {
    let env = |_: &str| None;
    let yaml = format!("admin:\n  token_sha256: \"{}\"\n", hash_key(ADMIN));
    let cfg = Config::from_yaml_str_with_env(&yaml, &env).expect("valid");
    assert!(cfg.providers.contains_key("openai") && cfg.providers.contains_key("openrouter"));
    assert!(cfg.keys.is_empty());
    assert!(Config::from_yaml_str_with_env("{}", &env).is_err(), "no keys and no admin token");
    let off = format!("auto_providers: false\nadmin:\n  token_sha256: \"{}\"\n", hash_key(ADMIN));
    assert!(Config::from_yaml_str_with_env(&off, &env).is_err(), "no providers at all");
}

#[tokio::test]
async fn a_bad_stored_entry_is_skipped_not_fatal() {
    let h = start().await;
    let ok = json!({"targets": [{"provider": "openai", "model": "gpt-4o-mini"}]});
    assert_eq!(h.put("/api/routes/good", ok).await.0, 200);

    // A route whose provider no longer exists, as after a config change or a
    // race between two replicas, written straight to the store.
    let bad = json!({"name": "broken", "targets": [{"provider": "gone", "model": "x"}]}).to_string();
    h.state.store.hset("dashboard_routes", "broken", &bad).await.unwrap();
    h.state.store.hset("dashboard_providers", "junk", "{not json").await.unwrap();
    ai_gateway::admin::refresh_dynamic(&h.state).await.unwrap();

    // Everything else keeps working, and new changes still apply.
    let (s, hd, _) = h.chat(KEY, "good", json!({})).await;
    assert_eq!((s.as_u16(), provider_of(&hd)), (200, "openai"));
    let more = json!({"targets": [{"provider": "openrouter", "model": "openai/gpt-4o-mini"}]});
    assert_eq!(h.put("/api/routes/more", more).await.0, 200);
    let (_, hd, _) = h.chat(KEY, "more", json!({})).await;
    assert_eq!(provider_of(&hd), "openrouter");

    // The bad entries are listed with the reason, and can be deleted.
    let (_, v) = h.admin_req(reqwest::Method::GET, "/api/routes", None).await;
    let broken = v["data"].as_array().unwrap().iter().find(|r| r["name"] == "broken").cloned().unwrap();
    assert!(broken["error"].as_str().unwrap().contains("unknown provider"), "{broken}");
    let (_, v) = h.admin_req(reqwest::Method::GET, "/api/providers", None).await;
    assert!(v["data"].as_array().unwrap().iter().any(|p| p["name"] == "junk" && p["error"].is_string()));
    assert_eq!(h.admin_req(reqwest::Method::DELETE, "/api/routes/broken", None).await.0, 204);
    assert_eq!(h.admin_req(reqwest::Method::DELETE, "/api/providers/junk", None).await.0, 204);
    let (_, v) = h.admin_req(reqwest::Method::GET, "/api/routes", None).await;
    assert!(!v["data"].as_array().unwrap().iter().any(|r| r["name"] == "broken" || r["error"].is_string()));
}

#[tokio::test]
async fn model_lists_are_fetched_once_and_follow_key_changes() {
    let h = start().await;
    // Twenty clients asking at once on a cold cache cause one fetch each.
    let calls = (0..20).map(|_| h.models(KEY, ""));
    let results = futures::future::join_all(calls).await;
    assert!(results.iter().all(|r| r.len() > 3));
    assert_eq!(h.hits("openai"), 1);
    assert_eq!(h.hits("router"), 1);

    // A new key for a provider gets a new list: the cache follows the key.
    let (s, _) = h
        .admin_req(reqwest::Method::PUT, "/api/provider-keys/openai", Some(json!({"api_key": "sk-new"})))
        .await;
    assert_eq!(s, 204);
    h.models(KEY, "").await;
    assert_eq!(h.hits("openai"), 2);
    assert_eq!(h.hits("router"), 1, "other providers stay cached");
}
