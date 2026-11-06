//! Application state and HTTP routers.
//!
//! Settings that can change while the gateway runs (providers, routes,
//! guardrails, limits, routing rules, keys, cache and logging options) live
//! in a `Runtime` that is swapped atomically on reload (SIGHUP or
//! `POST /api/reload`). Listeners, the store and the admin token are read
//! once at startup.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Context;
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{any, delete, get, post},
    Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use parking_lot::RwLock;
use tokio::sync::{watch, Semaphore};
use tower_http::{catch_panic::CatchPanicLayer, set_header::SetResponseHeaderLayer};

use crate::{
    admin,
    auth::KeyRing,
    config::{Config, RouteConfig, Secret},
    export::Exporter,
    guardrails::Guardrails,
    native,
    providers::Provider,
    proxy, realtime,
    rules::RuleSet,
    secrets::Cipher,
    stats::TargetStats,
    store::Store,
};

/// Everything that can change on a config reload.
pub struct Runtime {
    pub cfg: Config,
    pub providers: HashMap<String, Arc<Provider>>,
    pub routes: HashMap<String, RouteConfig>,
    pub guardrails: Guardrails,
}

impl Runtime {
    /// Builds providers, reusing `prev`'s provider objects whose config did not
    /// change, so their circuit-breaker state survives a reload.
    pub fn build(cfg: Config, client: &reqwest::Client, prev: Option<&Runtime>) -> anyhow::Result<Self> {
        let mut providers = HashMap::new();
        for (name, p) in &cfg.providers {
            let reused = prev.and_then(|r| r.providers.get(name)).filter(|old| old.cfg == *p).cloned();
            let provider = match reused {
                Some(p) => p,
                None => Arc::new(Provider::new(name, p.clone(), client.clone())?),
            };
            providers.insert(name.clone(), provider);
        }
        let routes = cfg.routes.iter().map(|r| (r.name.clone(), r.clone())).collect();
        let guardrails = Guardrails::compile(&cfg.guardrails)?;
        Ok(Self { cfg, providers, routes, guardrails })
    }
}

pub struct AppState {
    rt: RwLock<Arc<Runtime>>,
    /// Config file to re-read on reload (none in tests).
    pub config_path: RwLock<Option<String>>,
    pub keys: KeyRing,
    pub rules: RuleSet,
    /// Provider keys stored through the admin API (BYOK), decrypted.
    pub stored_provider_keys: RwLock<HashMap<String, Secret>>,
    pub cipher: Option<Cipher>,
    pub admin_token_sha256: Option<String>,
    pub store: Store,
    pub http: reqwest::Client,
    pub stats: TargetStats,
    pub exporter: Exporter,
    pub limiter: Arc<Semaphore>,
    /// False while draining for shutdown, so the load balancer stops routing here.
    pub ready: AtomicBool,
    /// Token-usage writes still in flight; shutdown waits for these.
    pub pending_usage_writes: AtomicUsize,
    /// Set to true when the shutdown grace period ends, to close open streams.
    pub stream_cancel: watch::Sender<bool>,
}

impl AppState {
    pub fn rt(&self) -> Arc<Runtime> {
        self.rt.read().clone()
    }

    pub async fn build(cfg: Config) -> anyhow::Result<Arc<Self>> {
        crate::install_crypto_provider();

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(64)
            .tcp_keepalive(Duration::from_secs(60))
            .user_agent(concat!("ai-gateway/", env!("CARGO_PKG_VERSION")))
            // Never follow redirects: they would re-send provider keys
            // (api-key / x-api-key headers) and prompts to another URL.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building HTTP client")?;

        let store = Store::connect(&cfg.store).await?;
        let cipher = match &cfg.admin.encryption_key {
            Some(k) => Some(Cipher::from_base64(k.expose()).context("admin.encryption_key")?),
            None => None,
        };
        let rules = RuleSet::default();
        rules.set_config(&cfg.routing_rules);
        let exporter = Exporter::start(&cfg.logging, client.clone());
        let state = Arc::new(Self {
            keys: KeyRing::new(&cfg.keys),
            rules,
            stored_provider_keys: RwLock::new(HashMap::new()),
            cipher,
            admin_token_sha256: cfg.admin.token_sha256.clone(),
            limiter: Arc::new(Semaphore::new(cfg.server.max_concurrent_requests)),
            store,
            exporter,
            stats: TargetStats::default(),
            ready: AtomicBool::new(true),
            pending_usage_writes: AtomicUsize::new(0),
            stream_cancel: watch::channel(false).0,
            config_path: RwLock::new(None),
            rt: RwLock::new(Arc::new(Runtime::build(cfg, &client, None)?)),
            http: client,
        });

        // Keys, routing rules and provider keys created through the admin
        // API live in the store. Load them now and keep every replica in sync.
        if let Err(e) = admin::refresh_dynamic(&state).await {
            tracing::warn!(error = %e, "could not load settings created through the admin API");
        }
        let weak = Arc::downgrade(&state);
        let every = Duration::from_secs(state.rt().cfg.admin.key_refresh_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(app) = weak.upgrade() else { break };
                if let Err(e) = admin::refresh_dynamic(&app).await {
                    metrics::counter!("gateway_store_errors_total", "op" => "key_refresh").increment(1);
                    tracing::warn!(error = %e, "refresh of admin settings failed; keeping the current ones");
                }
            }
        });
        Ok(state)
    }

    /// Re-reads the config file and swaps in the new runtime settings.
    /// Returns warnings about settings that only change with a restart.
    pub fn reload(&self) -> anyhow::Result<Vec<String>> {
        let path =
            self.config_path.read().clone().context("the gateway was not started from a config file")?;
        let cfg = Config::load(&path)?;
        let old = self.rt();
        let mut notes = cfg.warnings();
        if cfg.server.listen != old.cfg.server.listen
            || cfg.server.admin_listen != old.cfg.server.admin_listen
            || cfg.server.max_concurrent_requests != old.cfg.server.max_concurrent_requests
        {
            notes.push("server listen addresses and max_concurrent_requests change only on restart".into());
        }
        if cfg.store.backend != old.cfg.store.backend || cfg.store.key_prefix != old.cfg.store.key_prefix {
            notes.push("store settings change only on restart".into());
        }
        if cfg.admin.token_sha256 != self.admin_token_sha256 {
            notes.push("admin.token_sha256 changes only on restart".into());
        }
        if cfg.logging.export.is_some() != old.cfg.logging.export.is_some()
            || cfg.logging.otlp.is_some() != old.cfg.logging.otlp.is_some()
        {
            notes.push("log export and OpenTelemetry settings change only on restart".into());
        }
        let rt = Runtime::build(cfg, &self.http, Some(&old))?;
        self.keys.set_config(&rt.cfg.keys);
        self.rules.set_config(&rt.cfg.routing_rules);
        *self.rt.write() = Arc::new(rt);
        metrics::counter!("gateway_config_reloads_total").increment(1);
        tracing::info!(path = %path, "configuration reloaded");
        Ok(notes)
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let max_body = state.rt().cfg.server.max_body_bytes;
    Router::new()
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/v1/embeddings", post(proxy::embeddings))
        .route("/v1/responses", post(proxy::responses))
        .route("/v1/messages", post(proxy::messages))
        .route("/v1/images/generations", post(proxy::images))
        .route("/v1/moderations", post(proxy::moderations))
        .route("/v1/rerank", post(proxy::rerank))
        .route("/v1/audio/speech", post(proxy::speech))
        .route("/v1/realtime", get(realtime::handler))
        .route("/v1/models", get(proxy::list_models))
        .route("/v1/report", get(admin::client_report))
        .route("/v1/generation", get(admin::client_generation))
        .route("/v1/feedback", post(admin::client_feedback))
        .route("/native/:provider/*path", any(native::handler))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        // Handlers read their bodies themselves after authentication and
        // enforce their own deadline (504 with an OpenAI error body).
        .layer(DefaultBodyLimit::max(max_body))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

/// Served on the admin listener only. Do not expose it publicly.
pub fn admin_router(state: Arc<AppState>, metrics: PrometheusHandle) -> Router {
    admin_api_router(state).route(
        "/metrics",
        get(move || {
            let m = metrics.clone();
            async move { m.render() }
        }),
    )
}

/// Dashboard and admin API, without `/metrics` (tests mount this alone
/// because the Prometheus recorder is process-wide).
pub fn admin_api_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(admin::dashboard))
        .route("/assets/app.js", get(admin::dashboard_js))
        .route("/api/overview", get(admin::overview))
        .route("/api/logs", get(admin::list_logs).delete(admin::delete_logs))
        .route("/api/logs/:id", get(admin::get_log))
        .route("/api/logs/:id/feedback", post(admin::set_feedback))
        .route("/api/report", get(admin::report))
        .route("/api/keys", get(admin::list_keys).post(admin::create_key))
        .route("/api/keys/:name", delete(admin::delete_key))
        .route("/api/rules", get(admin::list_rules).post(admin::create_rule))
        .route("/api/rules/:id", delete(admin::delete_rule).patch(admin::update_rule))
        .route("/api/provider-keys", get(admin::list_provider_keys))
        .route(
            "/api/provider-keys/:provider",
            axum::routing::put(admin::put_provider_key).delete(admin::delete_provider_key),
        )
        .route("/api/limits", get(admin::list_limits))
        .route("/api/audit", get(admin::audit_log))
        .route("/api/config", get(admin::get_config))
        .route("/api/reload", post(admin::reload))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// Readiness reflects only this instance (draining or not). It deliberately
/// does not check Redis: a Redis outage would otherwise fail every replica at
/// once and leave the load balancer with no backends. The request path
/// already applies `store.on_error` per request, and Redis health is visible
/// in `gateway_store_errors_total`.
async fn readyz(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    if app.ready.load(Ordering::SeqCst) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "draining")
    }
}
