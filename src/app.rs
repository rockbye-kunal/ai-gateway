//! Application state and HTTP routers.

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
    routing::{delete, get, post},
    Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::{watch, Semaphore};
use tower_http::{catch_panic::CatchPanicLayer, set_header::SetResponseHeaderLayer};

use crate::{
    admin,
    auth::KeyRing,
    config::{Config, RouteConfig},
    guardrails::Guardrails,
    providers::Provider,
    proxy,
    store::Store,
};

pub struct AppState {
    pub cfg: Config,
    pub keys: KeyRing,
    pub providers: HashMap<String, Arc<Provider>>,
    pub routes: HashMap<String, RouteConfig>,
    pub guardrails: Guardrails,
    pub store: Store,
    pub limiter: Arc<Semaphore>,
    /// False while draining for shutdown, so the load balancer stops routing here.
    pub ready: AtomicBool,
    /// Token-usage writes still in flight; shutdown waits for these.
    pub pending_usage_writes: AtomicUsize,
    /// Set to true when the shutdown grace period ends, to close open streams.
    pub stream_cancel: watch::Sender<bool>,
}

impl AppState {
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

        let mut providers = HashMap::new();
        for (name, p) in &cfg.providers {
            providers.insert(name.clone(), Arc::new(Provider::new(name, p.clone(), client.clone())?));
        }
        let routes = cfg.routes.iter().map(|r| (r.name.clone(), r.clone())).collect();
        let guardrails = Guardrails::compile(&cfg.guardrails)?;
        let store = Store::connect(&cfg.store).await?;

        let state = Arc::new(Self {
            keys: KeyRing::new(&cfg.keys),
            guardrails,
            limiter: Arc::new(Semaphore::new(cfg.server.max_concurrent_requests)),
            providers,
            routes,
            store,
            ready: AtomicBool::new(true),
            pending_usage_writes: AtomicUsize::new(0),
            stream_cancel: watch::channel(false).0,
            cfg,
        });

        // Keys created through the admin API live in the store. Load them now
        // and keep every replica in sync in the background.
        if let Err(e) = admin::refresh_keys(&state).await {
            tracing::warn!(error = %e, "could not load API keys created through the admin API");
        }
        let weak = Arc::downgrade(&state);
        let every = Duration::from_secs(state.cfg.admin.key_refresh_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(app) = weak.upgrade() else { break };
                if let Err(e) = admin::refresh_keys(&app).await {
                    metrics::counter!("gateway_store_errors_total", "op" => "key_refresh").increment(1);
                    tracing::warn!(error = %e, "API key refresh failed; keeping the current keys");
                }
            }
        });
        Ok(state)
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let max_body = state.cfg.server.max_body_bytes;
    Router::new()
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/v1/embeddings", post(proxy::embeddings))
        .route("/v1/models", get(proxy::list_models))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        // The chat handler reads its body itself after authentication and
        // enforces its own deadline (504 with an OpenAI error body).
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
        .route("/api/logs", get(admin::list_logs))
        .route("/api/logs/:id", get(admin::get_log))
        .route("/api/keys", get(admin::list_keys).post(admin::create_key))
        .route("/api/keys/:name", delete(admin::delete_key))
        .route("/api/config", get(admin::get_config))
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
