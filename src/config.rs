//! Gateway configuration, loaded from YAML.
//!
//! Any string value may reference environment variables as `${NAME}` or
//! `${NAME:-default}`. Substitution happens after YAML parsing, so a secret
//! containing `:` or `#` can never change the structure of the file.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    net::SocketAddr,
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_yaml::Value;

/// A string that never appears in Debug output or logs.
#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"***\"")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    /// Providers defined in the file. Built-in providers (see `builtin.rs`)
    /// and providers added in the dashboard are merged in at runtime.
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// Make the built-in providers (OpenAI, Anthropic, Groq, …) available as
    /// `<name>/<model>` once they have a key, without listing them here.
    #[serde(default = "d_true")]
    pub auto_providers: bool,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    /// Provider that receives any `model` that is neither a route name nor
    /// `provider/model`, with the model name passed through unchanged.
    /// Typically an aggregator such as OpenRouter.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// May be empty when an admin token is set and keys are created in the
    /// dashboard.
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub guardrails: GuardrailsConfig,
    /// Spend, token and request limits, each scoped by key, user, model,
    /// provider or a metadata field.
    #[serde(default)]
    pub limits: Vec<LimitConfig>,
    /// Model rewrite and deny rules applied before routing.
    #[serde(default)]
    pub routing_rules: Vec<RoutingRuleConfig>,
    #[serde(default)]
    pub alerts: AlertsConfig,
    #[serde(default)]
    pub pricing: PricingConfig,
}

pub const LITELLM_PRICES_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Automatic model prices (see `pricing.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PricingConfig {
    /// Load a public price list so requests get a cost without `price_*` on
    /// every target.
    pub enabled: bool,
    /// A price list in LiteLLM's JSON format.
    pub url: String,
    /// Read the list from this file instead of `url` (air-gapped installs).
    pub file: Option<String>,
    pub refresh_hours: u64,
    /// Prices that win over the list, keyed `provider/model` or `model`.
    pub overrides: HashMap<String, crate::pricing::Price>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: LITELLM_PRICES_URL.to_string(),
            file: None,
            refresh_hours: 24,
            overrides: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    /// Number of requests.
    Requests,
    /// Prompt plus completion tokens.
    Tokens,
    /// Spend in US dollars (provider-reported or configured prices).
    Cost,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitWindow {
    Minute,
    Hour,
    Day,
    Week,
    Month,
}

impl LimitWindow {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitWindow::Minute => "minute",
            LimitWindow::Hour => "hour",
            LimitWindow::Day => "day",
            LimitWindow::Week => "week",
            LimitWindow::Month => "month",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LimitConfig {
    pub name: String,
    pub kind: LimitKind,
    /// Requests, tokens, or US dollars per window.
    pub limit: f64,
    pub window: LimitWindow,
    /// What each bucket is counted per: `global` (default), `key`, `user`,
    /// `model`, `provider`, or `metadata.<field>`.
    #[serde(default = "d_global")]
    pub per: String,
    /// Only count requests from these API keys (empty = all keys).
    #[serde(default)]
    pub keys: Vec<String>,
    /// Only count requests for these models (`provider/model`, `provider/*`,
    /// or route names; empty = all).
    #[serde(default)]
    pub models: Vec<String>,
    /// For `requests` limits: smooth the window edge (sliding window
    /// approximation) instead of resetting at the boundary.
    #[serde(default)]
    pub sliding: bool,
}

fn d_global() -> String {
    "global".into()
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    /// Serve the request with `destination` instead.
    Rewrite,
    /// Reject the request with 403.
    Deny,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingRuleConfig {
    /// Model to match: a route name, `provider/model`, `provider/*`, or any
    /// model string the client sends (with default_provider).
    pub source: String,
    pub action: RuleAction,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AlertsConfig {
    /// Receives JSON POSTs for limit thresholds and provider circuit changes.
    pub webhook_url: Option<String>,
    /// HMAC-SHA256 key; the signature is sent in `x-gateway-signature`.
    pub webhook_secret: Option<Secret>,
    /// Fractions of a limit that trigger an alert, once per window each.
    pub thresholds: Vec<f64>,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self { webhook_url: None, webhook_secret: None, thresholds: vec![0.8, 1.0] }
    }
}

/// The admin API and dashboard, served on `server.admin_listen`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AdminConfig {
    /// Lowercase hex SHA-256 of the admin token (`ai-gateway generate-key`).
    /// The admin API and dashboard are disabled while this is unset.
    pub token_sha256: Option<String>,
    /// How often each replica reloads API keys created through the admin API.
    /// A key revoked on one replica stops working everywhere within this time.
    pub key_refresh_secs: u64,
    /// 32 random bytes, base64. Encrypts provider keys stored through the
    /// admin API (BYOK). Storing provider keys is disabled while unset.
    pub encryption_key: Option<Secret>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self { token_sha256: None, key_refresh_secs: 5, encryption_key: None }
    }
}

/// Request logs and analytics shown in the dashboard.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LoggingConfig {
    pub enabled: bool,
    /// Most recent requests kept in the searchable log (shared by all replicas).
    pub max_entries: usize,
    /// Also store prompts and responses. Off by default: they may contain
    /// personal data. Guardrail redactions are applied before capture.
    pub capture_bodies: bool,
    /// Prompt and response text longer than this is truncated in the log.
    pub max_body_bytes: usize,
    /// How long a request's full detail (including bodies) is kept.
    pub detail_ttl_secs: u64,
    /// How long hourly usage reports (by key, user, tag, model, provider) are kept.
    pub report_retention_days: u64,
    /// Ship every log entry to an HTTP endpoint (NDJSON batches).
    pub export: Option<HttpExportConfig>,
    /// Send an OpenTelemetry trace per request (OTLP/HTTP JSON).
    pub otlp: Option<OtlpConfig>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 10_000,
            capture_bodies: false,
            max_body_bytes: 32 * 1024,
            detail_ttl_secs: 7 * 86_400,
            report_retention_days: 90,
            export: None,
            otlp: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpExportConfig {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, Secret>,
    #[serde(default = "d_batch")]
    pub batch_size: usize,
    #[serde(default = "d_flush")]
    pub flush_secs: u64,
    /// Include prompts and responses (when captured) in exported entries.
    #[serde(default)]
    pub include_bodies: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtlpConfig {
    /// Collector base URL, e.g. `http://otel-collector:4318`; traces go to `/v1/traces`.
    pub endpoint: String,
    #[serde(default)]
    pub headers: HashMap<String, Secret>,
    #[serde(default = "d_service")]
    pub service_name: String,
    #[serde(default = "d_batch")]
    pub batch_size: usize,
    #[serde(default = "d_flush")]
    pub flush_secs: u64,
}

fn d_batch() -> usize {
    100
}
fn d_flush() -> u64 {
    5
}
fn d_service() -> String {
    "ai-gateway".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GuardrailsConfig {
    pub rules: Vec<GuardrailRule>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GuardAction {
    /// Reject the request (or response) with an error.
    Block,
    /// Replace each match with `[REDACTED:<rule>]` and continue.
    Redact,
    /// Let it through unchanged and record the match in the request log.
    Flag,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GuardScope {
    Request,
    Response,
    #[default]
    Both,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModerationConfig {
    pub provider: String,
    #[serde(default = "d_mod_model")]
    pub model: String,
    /// Categories that trigger the rule (empty = any flagged category),
    /// e.g. `hate`, `harassment`, `self-harm`, `sexual`, `violence`.
    #[serde(default)]
    pub categories: Vec<String>,
    /// Category score at or above which the rule matches (default: the
    /// provider's own `flagged` decision).
    #[serde(default)]
    pub threshold: Option<f64>,
    /// Let requests through when the moderation call fails (default true).
    #[serde(default = "d_true")]
    pub fail_open: bool,
}

fn d_mod_model() -> String {
    "omni-moderation-latest".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardrailRule {
    pub name: String,
    /// One of the built-in detectors (see `guardrails::BUILTINS`).
    #[serde(default)]
    pub builtin: Option<String>,
    /// A custom regular expression (Rust `regex` syntax).
    #[serde(default)]
    pub pattern: Option<String>,
    /// A moderation model (OpenAI-compatible `/moderations`) that scores
    /// prompts and responses by category.
    #[serde(default)]
    pub moderation: Option<ModerationConfig>,
    pub action: GuardAction,
    #[serde(default)]
    pub apply_to: GuardScope,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// Public API listener.
    pub listen: String,
    /// Metrics / health listener. Keep this off the public network.
    pub admin_listen: String,
    pub max_body_bytes: usize,
    /// Upper bound on the time to produce response headers (not on stream length).
    pub request_timeout_secs: u64,
    /// In-flight requests per instance before new ones get 503.
    pub max_concurrent_requests: usize,
    /// After SIGTERM, how long /readyz reports "draining" before the listener closes,
    /// so the load balancer stops sending traffic first.
    pub shutdown_drain_secs: u64,
    /// After the listener closes, how long in-flight requests get to finish.
    pub shutdown_grace_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            admin_listen: "127.0.0.1:9090".into(),
            max_body_bytes: 10 * 1024 * 1024,
            request_timeout_secs: 600,
            max_concurrent_requests: 1024,
            shutdown_drain_secs: 5,
            shutdown_grace_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StoreBackend {
    /// Per-instance state. Fine for one replica or local development.
    Memory,
    /// Shared state across replicas. Required behind a load balancer.
    Redis,
}

/// What to do with rate limit / budget checks when the store is unreachable.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailMode {
    /// Let requests through without limits (availability first).
    Open,
    /// Reject requests with 503 (cost control first).
    Closed,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StoreConfig {
    pub backend: StoreBackend,
    /// e.g. `rediss://:password@host:6380/0` (use `rediss://` for TLS).
    pub redis_url: Option<Secret>,
    pub key_prefix: String,
    pub op_timeout_ms: u64,
    pub on_error: FailMode,
    pub memory_max_cache_entries: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            backend: StoreBackend::Memory,
            redis_url: None,
            key_prefix: "aigw:".into(),
            op_timeout_ms: 250,
            on_error: FailMode::Closed,
            memory_max_cache_entries: 10_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    pub enabled: bool,
    pub ttl_secs: u64,
    pub max_entry_bytes: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { enabled: false, ttl_secs: 300, max_entry_bytes: 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI and every OpenAI-compatible API (Gemini's /openai endpoint, Groq,
    /// Together, Mistral, vLLM, Ollama, ...).
    Openai,
    /// Azure OpenAI. The target `model` is the deployment name.
    Azure,
    /// Anthropic Messages API. Requests and responses are translated.
    Anthropic,
    /// AWS Bedrock (Converse API), signed with SigV4.
    Bedrock,
    /// Google Vertex AI: Gemini and partner models through the
    /// OpenAI-compatible endpoint, Claude through the Anthropic publisher.
    Vertex,
}

/// Where a provider definition came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSource {
    /// The config file.
    #[default]
    Config,
    /// Known to the gateway; enabled by a key from the environment, the
    /// dashboard or the request.
    Builtin,
    /// Added through the dashboard or admin API.
    Dashboard,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Not read from YAML; set by the gateway.
    #[serde(skip)]
    pub source: ProviderSource,
    pub kind: ProviderKind,
    /// Derived from `region` (and `project`) for Bedrock and Vertex.
    #[serde(default)]
    pub base_url: String,
    /// Not needed for Bedrock (AWS credentials) or Vertex (Google
    /// credentials); when set there it is sent as a bearer token (a Bedrock
    /// API key, or a Google access token).
    #[serde(default)]
    pub api_key: Secret,
    /// Bedrock: AWS region. Vertex: location (`us-central1`, `global`, ...).
    #[serde(default)]
    pub region: Option<String>,
    /// Vertex: Google Cloud project id.
    #[serde(default)]
    pub project: Option<String>,
    /// Bedrock: static AWS keys. Without them the standard AWS credential
    /// chain is used (environment, web identity, ECS, EC2 instance role).
    #[serde(default)]
    pub aws_access_key_id: Option<Secret>,
    #[serde(default)]
    pub aws_secret_access_key: Option<Secret>,
    #[serde(default)]
    pub aws_session_token: Option<Secret>,
    /// Vertex: service account key JSON, inline or as a file path. Without
    /// either, `GOOGLE_APPLICATION_CREDENTIALS` and then the metadata server
    /// are used.
    #[serde(default)]
    pub credentials_json: Option<Secret>,
    #[serde(default)]
    pub credentials_file: Option<String>,
    /// Prefix for looking this provider's models up in the price list, e.g.
    /// `openrouter/` (see `pricing`).
    #[serde(default)]
    pub pricing_prefix: Option<String>,
    /// Required for Azure, e.g. `2024-10-21`.
    #[serde(default)]
    pub api_version: Option<String>,
    /// Time allowed to receive response headers, and for non-streaming
    /// requests the full body.
    #[serde(default = "d_timeout")]
    pub timeout_secs: u64,
    /// Max silence between chunks of a streaming response.
    #[serde(default = "d_idle")]
    pub stream_idle_timeout_secs: u64,
    /// Retries on the same provider before falling back to the next target.
    #[serde(default = "d_retries")]
    pub max_retries: u32,
    /// Ask OpenAI-style providers for token usage on streams so budgets work.
    #[serde(default = "d_true")]
    pub stream_usage: bool,
    /// Consecutive failures that open the circuit.
    #[serde(default = "d_threshold")]
    pub breaker_failure_threshold: u32,
    /// How long an open circuit rejects traffic before a probe is allowed.
    #[serde(default = "d_open")]
    pub breaker_open_secs: u64,
    /// Allow plain http to a non-loopback host (e.g. an in-cluster vLLM).
    #[serde(default)]
    pub allow_insecure_http: bool,
    /// Anthropic requires max_tokens; used when the client sends none.
    #[serde(default = "d_max_tokens")]
    pub default_max_tokens: u64,
    /// Values are treated as secrets (redacted in logs, marked sensitive).
    #[serde(default)]
    pub extra_headers: HashMap<String, Secret>,
    /// The provider account has zero data retention. Requests that ask for
    /// ZDR only go to providers with this set.
    #[serde(default)]
    pub zero_data_retention: bool,
    /// The provider does not train on prompts. Requests that disallow
    /// prompt training only go to providers with this set.
    #[serde(default)]
    pub no_prompt_training: bool,
    /// Anthropic: add prompt-caching markers automatically (system prompt and
    /// latest turn), like `caching: auto`.
    #[serde(default)]
    pub auto_prompt_cache: bool,
    /// Clients may send their own key for this provider (request-scoped BYOK).
    #[serde(default = "d_true")]
    pub allow_client_keys: bool,
}

fn d_max_tokens() -> u64 {
    4096
}
fn d_timeout() -> u64 {
    120
}
fn d_idle() -> u64 {
    60
}
fn d_retries() -> u32 {
    2
}
fn d_true() -> bool {
    true
}
fn d_threshold() -> u32 {
    5
}
fn d_open() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// The name clients send as `model`.
    pub name: String,
    #[serde(default)]
    pub strategy: RouteStrategy,
    /// Tried in order; later targets are fallbacks.
    pub targets: Vec<TargetConfig>,
    /// Conditional branches, checked in order. The first whose `when`
    /// matches replaces `targets` (and `strategy`) for that request.
    #[serde(default)]
    pub branches: Vec<RouteBranch>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteBranch {
    /// Field → condition. Fields: `key`, `user`, `tags`, `metadata.<name>`.
    /// A condition is a string (equals) or `{eq|ne|in|not_in|exists}`.
    pub when: HashMap<String, Condition>,
    #[serde(default)]
    pub strategy: RouteStrategy,
    pub targets: Vec<TargetConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum Condition {
    Equals(String),
    Op(ConditionOp),
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConditionOp {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eq: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ne: Option<String>,
    #[serde(rename = "in", skip_serializing_if = "Option::is_none")]
    pub one_of: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_in: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteStrategy {
    /// Always start with the first target; the rest are fallbacks in order.
    #[default]
    Fallback,
    /// Pick the first target at random by `weight` (A/B tests, gradual
    /// rollouts, load spreading); the others remain fallbacks in order.
    /// Requests with a session id stick to the same first target.
    Weighted,
    /// Fastest first, by recent time to first token on this replica.
    Latency,
    /// Cheapest first, by configured prices.
    Cost,
}

#[derive(Debug, Clone, Deserialize, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub provider: String,
    pub model: String,
    /// USD per million input tokens, for cost reporting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_input_per_mtok: Option<f64>,
    /// USD per million output tokens, for cost reporting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_output_per_mtok: Option<f64>,
    /// Share of traffic for `strategy: weighted` routes (default 1). A weight
    /// of 0 makes the target a fallback only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// Tenant / client name used in logs, metrics and limits.
    pub name: String,
    /// Lowercase hex SHA-256 of the API key. Generate with `ai-gateway generate-key`.
    pub key_sha256: String,
    /// Hashes of older keys still accepted during a rotation. They share the
    /// tenant's limits, budget and metrics.
    #[serde(default)]
    pub previous_key_sha256: Vec<String>,
    #[serde(default)]
    pub requests_per_minute: Option<u64>,
    #[serde(default)]
    pub daily_token_budget: Option<u64>,
    /// Max in-flight requests for this key on each instance, so one client's
    /// long streams cannot take all of the gateway's capacity.
    #[serde(default)]
    pub max_concurrent_requests: Option<usize>,
    /// Route names, `provider/model`, `provider/*`, or `*` for everything.
    pub allowed_models: Vec<String>,
    /// Never store this key's prompts or responses, never cache them, and only
    /// use providers marked `zero_data_retention`.
    #[serde(default)]
    pub zero_data_retention: bool,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading config file {path}"))?;
        Self::from_yaml_str(&text).with_context(|| format!("loading config file {path}"))
    }

    pub fn from_yaml_str(text: &str) -> Result<Self> {
        Self::from_yaml_str_with_env(text, &|k| std::env::var(k).ok())
    }

    /// Like `from_yaml_str`, reading built-in provider keys through `env`
    /// (tests pass their own).
    pub fn from_yaml_str_with_env(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let mut value: Value = serde_yaml::from_str(text).context("parsing YAML")?;
        substitute_env(&mut value)?;
        let mut cfg: Config = serde_yaml::from_value(value).context("invalid configuration")?;
        if cfg.auto_providers {
            for (name, p) in crate::builtin::providers(env) {
                cfg.providers.entry(name).or_insert(p);
            }
        }
        cfg.fill_defaults();
        cfg.validate()?;
        Ok(cfg)
    }

    /// This config plus providers and routes added in the dashboard.
    /// Dashboard providers replace built-in ones of the same name but never
    /// config-file ones; routes never replace config-file routes.
    pub fn with_dynamic(
        &self,
        providers: &std::collections::BTreeMap<String, ProviderConfig>,
        routes: &std::collections::BTreeMap<String, RouteConfig>,
    ) -> Result<Self> {
        let mut cfg = self.clone();
        for (name, p) in providers {
            let old = cfg.providers.get(name);
            if old.is_some_and(|o| o.source == ProviderSource::Config) {
                continue;
            }
            let mut p = p.clone();
            p.source = ProviderSource::Dashboard;
            // Overriding a built-in provider (say, a new endpoint) keeps the
            // key it has from the environment.
            if p.api_key.expose().is_empty() {
                if let Some(o) = old.filter(|o| o.source == ProviderSource::Builtin) {
                    p.api_key = o.api_key.clone();
                }
            }
            cfg.providers.insert(name.clone(), p);
        }
        for r in routes.values() {
            if !cfg.routes.iter().any(|x| x.name == r.name) {
                cfg.routes.push(r.clone());
            }
        }
        cfg.fill_defaults();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Derives Bedrock and Vertex endpoints from their region and project.
    fn fill_defaults(&mut self) {
        for p in self.providers.values_mut() {
            if p.kind == ProviderKind::Bedrock && p.region.is_none() {
                p.region = std::env::var("AWS_REGION")
                    .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                    .ok()
                    .filter(|r| !r.is_empty());
            }
            if p.kind == ProviderKind::Vertex && p.region.is_none() {
                p.region = Some("us-central1".into());
            }
            if !p.base_url.trim().is_empty() {
                continue;
            }
            let region = p.region.clone().unwrap_or_default();
            p.base_url = match p.kind {
                ProviderKind::Bedrock => format!("https://bedrock-runtime.{region}.amazonaws.com"),
                ProviderKind::Vertex if region == "global" => "https://aiplatform.googleapis.com/v1".into(),
                ProviderKind::Vertex => format!("https://{region}-aiplatform.googleapis.com/v1"),
                _ => continue,
            };
        }
    }

    fn validate(&self) -> Result<()> {
        self.server
            .listen
            .parse::<SocketAddr>()
            .with_context(|| format!("server.listen '{}' is not a socket address", self.server.listen))?;
        self.server.admin_listen.parse::<SocketAddr>().with_context(|| {
            format!("server.admin_listen '{}' is not a socket address", self.server.admin_listen)
        })?;
        if self.server.max_concurrent_requests == 0 {
            bail!("server.max_concurrent_requests must be greater than 0");
        }
        if self.server.request_timeout_secs == 0 {
            bail!("server.request_timeout_secs must be greater than 0");
        }
        if self.server.max_body_bytes == 0 {
            bail!("server.max_body_bytes must be greater than 0");
        }
        if self.store.backend == StoreBackend::Redis && self.store.redis_url.is_none() {
            bail!("store.redis_url is required when store.backend is redis");
        }
        if self.store.op_timeout_ms == 0 {
            bail!("store.op_timeout_ms must be greater than 0");
        }

        if self.providers.is_empty() {
            bail!("at least one provider must be configured");
        }
        for (name, p) in &self.providers {
            if !valid_ident(name) {
                bail!("provider name '{name}' may only contain letters, digits, '-', '_' and '.'");
            }
            let cloud = matches!(p.kind, ProviderKind::Bedrock | ProviderKind::Vertex);
            // Built-in and dashboard providers get their key from the
            // environment, the dashboard or the request.
            if p.source == ProviderSource::Config && !cloud && p.api_key.expose().trim().is_empty() {
                bail!("provider {name}: api_key is empty");
            }
            if p.base_url.trim().is_empty() {
                bail!("provider {name}: base_url is required");
            }
            if let Some(r) = &p.region {
                if r.is_empty() || !r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    bail!("provider {name}: region '{r}' is not valid");
                }
            }
            if p.kind == ProviderKind::Bedrock {
                if p.region.is_none() {
                    bail!("provider {name}: bedrock providers need a region (or AWS_REGION)");
                }
                if p.aws_access_key_id.is_some() != p.aws_secret_access_key.is_some() {
                    bail!(
                        "provider {name}: set both aws_access_key_id and aws_secret_access_key, or neither"
                    );
                }
            }
            if p.kind == ProviderKind::Vertex {
                match &p.project {
                    Some(v)
                        if !v.is_empty()
                            && v.chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == ':') => {}
                    _ => bail!("provider {name}: vertex providers need a valid project"),
                }
                if p.credentials_json.is_some() && p.credentials_file.is_some() {
                    bail!("provider {name}: set credentials_json or credentials_file, not both");
                }
            }
            let url = p.base_url.trim();
            if url.contains('@') || url.contains(char::is_whitespace) || url.contains(['?', '#']) {
                bail!("provider {name}: base_url must not contain credentials, whitespace, a query or a fragment");
            }
            if p.default_max_tokens == 0 {
                bail!("provider {name}: default_max_tokens must be greater than 0");
            }
            if url.starts_with("http://") {
                if !p.allow_insecure_http && !is_loopback_url(url) {
                    bail!(
                        "provider {name}: base_url must use https \
                         (set allow_insecure_http: true only for private network endpoints)"
                    );
                }
            } else if !url.starts_with("https://") {
                bail!("provider {name}: base_url must start with https://");
            }
            if p.kind == ProviderKind::Azure {
                match &p.api_version {
                    Some(v)
                        if !v.is_empty()
                            && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') => {}
                    _ => bail!("provider {name}: azure providers need a valid api_version"),
                }
            }
            if p.max_retries > 10 {
                bail!("provider {name}: max_retries must be 10 or less");
            }
            if p.breaker_failure_threshold == 0 {
                bail!("provider {name}: breaker_failure_threshold must be at least 1");
            }
            if p.timeout_secs == 0 || p.stream_idle_timeout_secs == 0 {
                bail!("provider {name}: timeouts must be greater than 0");
            }
            for (k, v) in &p.extra_headers {
                reqwest::header::HeaderName::from_bytes(k.as_bytes())
                    .with_context(|| format!("provider {name}: invalid header name '{k}'"))?;
                reqwest::header::HeaderValue::from_str(v.expose())
                    .with_context(|| format!("provider {name}: invalid value for header '{k}'"))?;
            }
        }

        if let Some(d) = &self.default_provider {
            match self.providers.get(d) {
                None => bail!("default_provider '{d}' is not a configured provider"),
                Some(p) if p.kind == ProviderKind::Azure => {
                    bail!("default_provider cannot be an azure provider (model names are deployment names)")
                }
                Some(_) => {}
            }
        }

        let mut route_names = HashSet::new();
        for r in &self.routes {
            if !valid_ident(&r.name) {
                bail!("route name '{}' may only contain letters, digits, '-', '_' and '.'", r.name);
            }
            if !route_names.insert(r.name.as_str()) {
                bail!("duplicate route name '{}'", r.name);
            }
            if r.targets.is_empty() {
                bail!("route '{}' has no targets", r.name);
            }
            validate_targets(&r.name, r.strategy, &r.targets, self)?;
            for b in &r.branches {
                if b.targets.is_empty() {
                    bail!("route '{}': every branch needs targets", r.name);
                }
                if b.when.is_empty() {
                    bail!("route '{}': every branch needs a when condition", r.name);
                }
                for field in b.when.keys() {
                    if !valid_condition_field(field) {
                        bail!(
                            "route '{}': condition field '{field}' must be key, user, tags or metadata.<name>",
                            r.name
                        );
                    }
                }
                validate_targets(&r.name, b.strategy, &b.targets, self)?;
            }
            for t in &r.targets {
                let Some(p) = self.providers.get(&t.provider) else {
                    bail!("route '{}' references unknown provider '{}'", r.name, t.provider);
                };
                if !valid_model(&t.model) {
                    bail!("route '{}': invalid model name '{}'", r.name, t.model);
                }
                if p.kind == ProviderKind::Azure && t.model.contains('/') {
                    bail!("route '{}': azure deployment names cannot contain '/'", r.name);
                }
            }
        }

        if self.keys.is_empty() && self.admin.token_sha256.is_none() {
            bail!(
                "configure at least one API key, or set admin.token_sha256 and create keys in the dashboard"
            );
        }
        let mut key_names = HashSet::new();
        let mut key_hashes = HashSet::new();
        for k in &self.keys {
            if !valid_ident(&k.name) {
                bail!("key name '{}' may only contain letters, digits, '-', '_' and '.'", k.name);
            }
            if !key_names.insert(k.name.as_str()) {
                bail!("duplicate key name '{}'", k.name);
            }
            for hash in std::iter::once(&k.key_sha256).chain(&k.previous_key_sha256) {
                if !is_sha256_hex(hash) {
                    bail!("key '{}': key hashes must be 64 lowercase hex characters", k.name);
                }
                if !key_hashes.insert(hash.as_str()) {
                    bail!("key '{}' reuses a key hash that is already configured", k.name);
                }
            }
            if k.max_concurrent_requests == Some(0) {
                bail!("key '{}': max_concurrent_requests must be greater than 0", k.name);
            }
            if k.allowed_models.is_empty() {
                bail!("key '{}': allowed_models must list at least one entry", k.name);
            }
        }

        if let Some(h) = &self.admin.token_sha256 {
            if !is_sha256_hex(h) {
                bail!("admin.token_sha256 must be 64 lowercase hex characters");
            }
            if key_hashes.contains(h.as_str()) {
                bail!("admin.token_sha256 must not be the same as a client key");
            }
        }
        if self.admin.key_refresh_secs == 0 {
            bail!("admin.key_refresh_secs must be greater than 0");
        }
        if self.logging.max_entries == 0 || self.logging.max_entries > 1_000_000 {
            bail!("logging.max_entries must be between 1 and 1000000");
        }
        if self.logging.detail_ttl_secs == 0 {
            bail!("logging.detail_ttl_secs must be greater than 0");
        }
        crate::guardrails::Guardrails::compile(&self.guardrails)?;
        for g in &self.guardrails.rules {
            if let Some(m) = &g.moderation {
                match self.providers.get(&m.provider) {
                    Some(p) if p.kind == ProviderKind::Openai => {}
                    _ => bail!(
                        "guardrail '{}': moderation provider '{}' must be a configured openai-kind provider",
                        g.name,
                        m.provider
                    ),
                }
            }
        }

        let mut limit_names = HashSet::new();
        for l in &self.limits {
            if !valid_ident(&l.name) {
                bail!("limit name '{}' may only contain letters, digits, '-', '_' and '.'", l.name);
            }
            if !limit_names.insert(l.name.as_str()) {
                bail!("duplicate limit name '{}'", l.name);
            }
            if !(l.limit.is_finite() && l.limit > 0.0) {
                bail!("limit '{}': limit must be greater than 0", l.name);
            }
            if !valid_limit_dimension(&l.per) {
                bail!(
                    "limit '{}': per must be global, key, user, model, provider or metadata.<name>",
                    l.name
                );
            }
            if l.sliding && l.kind != LimitKind::Requests {
                bail!("limit '{}': sliding only applies to requests limits", l.name);
            }
        }
        for r in &self.routing_rules {
            if r.source.trim().is_empty() {
                bail!("routing rule source must not be empty");
            }
            match (r.action, &r.destination) {
                (RuleAction::Rewrite, Some(d)) if valid_model(d) => {}
                (RuleAction::Rewrite, _) => {
                    bail!("routing rule for '{}': rewrite needs a valid destination", r.source)
                }
                (RuleAction::Deny, _) => {}
            }
        }
        if let Some(u) = &self.alerts.webhook_url {
            if !(u.starts_with("https://") || is_loopback_url(u) || u.starts_with("http://")) {
                bail!("alerts.webhook_url must be an http(s) URL");
            }
        }
        if self.alerts.thresholds.iter().any(|t| !(t.is_finite() && *t > 0.0 && *t <= 1.0)) {
            bail!("alerts.thresholds must be fractions between 0 and 1");
        }
        if let Some(k) = &self.admin.encryption_key {
            crate::secrets::Cipher::from_base64(k.expose())
                .context("admin.encryption_key must be 32 random bytes, base64-encoded")?;
        }
        if let Some(e) = &self.logging.export {
            if !(e.url.starts_with("https://") || e.url.starts_with("http://")) {
                bail!("logging.export.url must be an http(s) URL");
            }
            if e.batch_size == 0 || e.flush_secs == 0 {
                bail!("logging.export batch_size and flush_secs must be greater than 0");
            }
        }
        if let Some(o) = &self.logging.otlp {
            if !(o.endpoint.starts_with("https://") || o.endpoint.starts_with("http://")) {
                bail!("logging.otlp.endpoint must be an http(s) URL");
            }
            if o.batch_size == 0 || o.flush_secs == 0 {
                bail!("logging.otlp batch_size and flush_secs must be greater than 0");
            }
        }
        if self.logging.report_retention_days == 0 || self.logging.report_retention_days > 400 {
            bail!("logging.report_retention_days must be between 1 and 400");
        }
        Ok(())
    }
}

fn validate_targets(
    route: &str,
    strategy: RouteStrategy,
    targets: &[TargetConfig],
    cfg: &Config,
) -> Result<()> {
    if strategy == RouteStrategy::Weighted {
        let total: u64 = targets.iter().map(|t| u64::from(t.weight.unwrap_or(1))).sum();
        if total == 0 {
            bail!("route '{route}': at least one target needs a weight above 0");
        }
    } else if targets.iter().any(|t| t.weight.is_some()) {
        bail!("route '{route}': weight is only used with strategy: weighted");
    }
    for t in targets {
        let Some(p) = cfg.providers.get(&t.provider) else {
            bail!("route '{route}' references unknown provider '{}'", t.provider);
        };
        if !valid_model(&t.model) {
            bail!("route '{route}': invalid model name '{}'", t.model);
        }
        if p.kind == ProviderKind::Azure && t.model.contains('/') {
            bail!("route '{route}': azure deployment names cannot contain '/'");
        }
    }
    Ok(())
}

pub fn valid_condition_field(f: &str) -> bool {
    matches!(f, "key" | "user" | "tags") || f.strip_prefix("metadata.").is_some_and(|m| !m.is_empty())
}

pub fn valid_limit_dimension(f: &str) -> bool {
    matches!(f, "global" | "key" | "user" | "model" | "provider")
        || f.strip_prefix("metadata.").is_some_and(|m| !m.is_empty())
}

pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

impl Config {
    /// Non-fatal problems worth logging at startup. Currently: routes whose
    /// worst-case retry and fallback chain cannot finish inside
    /// `server.request_timeout_secs`, so later fallbacks would never run.
    pub fn warnings(&self) -> Vec<String> {
        let budget = self.server.request_timeout_secs;
        let mut out = Vec::new();
        for r in &self.routes {
            let worst: u64 = r
                .targets
                .iter()
                .filter_map(|t| self.providers.get(&t.provider))
                .map(|p| {
                    let attempts = u64::from(p.max_retries) + 1;
                    // Backoff is capped at 4s per retry.
                    p.timeout_secs * attempts + 4 * u64::from(p.max_retries)
                })
                .sum();
            if worst > budget {
                out.push(format!(
                    "route '{}': worst case of {worst}s across retries and fallbacks exceeds \
                     server.request_timeout_secs ({budget}s); later targets may never be tried",
                    r.name
                ));
            }
        }
        if self.guardrails.rules.iter().any(|r| r.apply_to != GuardScope::Request) {
            out.push(
                "guardrails on responses apply to non-streaming responses only; \
                 streamed responses are not checked"
                    .into(),
            );
        }
        if self.admin.token_sha256.is_none() {
            out.push("admin.token_sha256 is not set; the admin API and dashboard are disabled".into());
        }
        if self.logging.capture_bodies {
            out.push(
                "logging.capture_bodies is on: prompts and responses are stored in the log; \
                 make sure that is allowed for your data"
                    .into(),
            );
        }
        out
    }
}

/// Names used in metrics labels, store keys and headers.
pub fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Model names as they appear in requests and upstream URLs.
pub fn valid_model(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !s.contains("..")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '@'))
}

fn is_loopback_url(url: &str) -> bool {
    let rest = url.trim_start_matches("http://");
    if rest.starts_with("[::1]") {
        return true;
    }
    let end = rest.find([':', '/']).unwrap_or(rest.len());
    matches!(&rest[..end], "localhost" | "127.0.0.1")
}

fn substitute_env(v: &mut Value) -> Result<()> {
    match v {
        Value::String(s) => *s = expand_env(s)?,
        Value::Sequence(seq) => {
            for x in seq {
                substitute_env(x)?;
            }
        }
        Value::Mapping(m) => {
            for (_, x) in m.iter_mut() {
                substitute_env(x)?;
            }
        }
        Value::Tagged(t) => substitute_env(&mut t.value)?,
        _ => {}
    }
    Ok(())
}

/// Expands `${NAME}` and `${NAME:-default}`.
pub fn expand_env(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').with_context(|| "unterminated '${' in a config value".to_string())?;
        let expr = &after[..end];
        let (name, default) = match expr.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (expr, None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("invalid environment variable name '{name}' in config");
        }
        match std::env::var(name) {
            Ok(val) => out.push_str(&val),
            Err(_) => match default {
                Some(d) => out.push_str(d),
                None => bail!("environment variable {name} is referenced in config but not set"),
            },
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn base(extra: &str) -> String {
        format!(
            r#"
providers:
  openai:
    kind: openai
    base_url: https://api.openai.com/v1
    api_key: sk-test
keys:
  - name: client
    key_sha256: "{HASH}"
    allowed_models: ["*"]
{extra}"#
        )
    }

    #[test]
    fn loads_minimal_config() {
        let cfg = Config::from_yaml_str(&base("")).unwrap();
        assert_eq!(cfg.providers["openai"].max_retries, 2);
        assert_eq!(cfg.store.backend, StoreBackend::Memory);
    }

    #[test]
    fn rejects_plain_http_to_remote_host() {
        let yaml = base("").replace("https://api.openai.com/v1", "http://api.openai.com/v1");
        assert!(Config::from_yaml_str(&yaml).is_err());
        let yaml = base("").replace("https://api.openai.com/v1", "http://127.0.0.1:8000/v1");
        assert!(Config::from_yaml_str(&yaml).is_ok());
        let yaml = base("").replace("https://api.openai.com/v1", "http://localhost.evil.com/v1");
        assert!(Config::from_yaml_str(&yaml).is_err());
        let yaml = base("").replace("https://api.openai.com/v1", "http://localhost:1@evil.com/v1");
        assert!(Config::from_yaml_str(&yaml).is_err());
    }

    #[test]
    fn rejects_unknown_route_provider() {
        let yaml = base("routes:\n  - name: smart\n    targets:\n      - provider: nope\n        model: x\n");
        assert!(Config::from_yaml_str(&yaml).is_err());
    }

    #[test]
    fn rejects_bad_hash() {
        let yaml = base("").replace(HASH, "abc");
        assert!(Config::from_yaml_str(&yaml).is_err());
    }

    #[test]
    fn default_provider_must_exist() {
        assert!(Config::from_yaml_str(&base("default_provider: openai\n")).is_ok());
        assert!(Config::from_yaml_str(&base("default_provider: nope\n")).is_err());
    }

    #[test]
    fn redis_requires_url() {
        let yaml = base("store:\n  backend: redis\n");
        assert!(Config::from_yaml_str(&yaml).is_err());
    }

    #[test]
    fn expands_env_vars() {
        std::env::set_var("AIGW_TEST_VALUE", "a:b#c");
        assert_eq!(expand_env("x-${AIGW_TEST_VALUE}-y").unwrap(), "x-a:b#c-y");
        assert_eq!(expand_env("${AIGW_TEST_MISSING:-fallback}").unwrap(), "fallback");
        assert!(expand_env("${AIGW_TEST_MISSING}").is_err());
        assert!(expand_env("${unterminated").is_err());
    }

    #[test]
    fn weighted_routes() {
        let ok = base(
            "routes:\n  - name: ab\n    strategy: weighted\n    targets:\n      - provider: openai\n        model: a\n        weight: 90\n      - provider: openai\n        model: b\n        weight: 10\n",
        );
        assert!(Config::from_yaml_str(&ok).is_ok());
        let zero = ok.replace("weight: 90", "weight: 0").replace("weight: 10", "weight: 0");
        assert!(Config::from_yaml_str(&zero).is_err());
        let no_strategy = ok.replace("    strategy: weighted\n", "");
        assert!(Config::from_yaml_str(&no_strategy).is_err());
    }

    #[test]
    fn admin_token_must_be_a_hash() {
        assert!(Config::from_yaml_str(&base("admin:\n  token_sha256: nope\n")).is_err());
        let other = "1".repeat(64);
        assert!(Config::from_yaml_str(&base(&format!("admin:\n  token_sha256: \"{other}\"\n"))).is_ok());
        // Reusing a client key hash as the admin token is rejected.
        assert!(Config::from_yaml_str(&base(&format!("admin:\n  token_sha256: \"{HASH}\"\n"))).is_err());
    }

    #[test]
    fn guardrail_rules_are_validated() {
        let good =
            base("guardrails:\n  rules:\n    - name: email\n      builtin: email\n      action: redact\n");
        assert!(Config::from_yaml_str(&good).is_ok());
        let unknown = good.replace("builtin: email", "builtin: nope");
        assert!(Config::from_yaml_str(&unknown).is_err());
        let bad_regex = good.replace("builtin: email", "pattern: \"(\"");
        assert!(Config::from_yaml_str(&bad_regex).is_err());
    }

    #[test]
    fn secret_is_redacted_in_debug() {
        let s = Secret::new("sk-very-secret");
        assert!(!format!("{s:?}").contains("sk-very"));
    }
}
