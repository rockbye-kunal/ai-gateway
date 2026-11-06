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
#[derive(Clone, Deserialize)]
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
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    /// Provider that receives any `model` that is neither a route name nor
    /// `provider/model`, with the model name passed through unchanged.
    /// Typically an aggregator such as OpenRouter.
    #[serde(default)]
    pub default_provider: Option<String>,
    pub keys: Vec<KeyConfig>,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub guardrails: GuardrailsConfig,
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
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self { token_sha256: None, key_refresh_secs: 5 }
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
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 10_000,
            capture_bodies: false,
            max_body_bytes: 32 * 1024,
            detail_ttl_secs: 7 * 86_400,
        }
    }
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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI and every OpenAI-compatible API (Gemini's /openai endpoint, Groq,
    /// Together, Mistral, vLLM, Ollama, ...).
    Openai,
    /// Azure OpenAI. The target `model` is the deployment name.
    Azure,
    /// Anthropic Messages API. Requests and responses are translated.
    Anthropic,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Secret,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// The name clients send as `model`.
    pub name: String,
    #[serde(default)]
    pub strategy: RouteStrategy,
    /// Tried in order; later targets are fallbacks.
    pub targets: Vec<TargetConfig>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteStrategy {
    /// Always start with the first target; the rest are fallbacks in order.
    #[default]
    Fallback,
    /// Pick the first target at random by `weight` (A/B tests, gradual
    /// rollouts, load spreading); the others remain fallbacks in order.
    Weighted,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub provider: String,
    pub model: String,
    /// USD per million input tokens, for cost reporting.
    #[serde(default)]
    pub price_input_per_mtok: Option<f64>,
    /// USD per million output tokens, for cost reporting.
    #[serde(default)]
    pub price_output_per_mtok: Option<f64>,
    /// Share of traffic for `strategy: weighted` routes (default 1). A weight
    /// of 0 makes the target a fallback only.
    #[serde(default)]
    pub weight: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
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
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading config file {path}"))?;
        Self::from_yaml_str(&text).with_context(|| format!("loading config file {path}"))
    }

    pub fn from_yaml_str(text: &str) -> Result<Self> {
        let mut value: Value = serde_yaml::from_str(text).context("parsing YAML")?;
        substitute_env(&mut value)?;
        let cfg: Config = serde_yaml::from_value(value).context("invalid configuration")?;
        cfg.validate()?;
        Ok(cfg)
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
            if p.api_key.expose().trim().is_empty() {
                bail!("provider {name}: api_key is empty");
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
            if r.strategy == RouteStrategy::Weighted {
                let total: u64 = r.targets.iter().map(|t| u64::from(t.weight.unwrap_or(1))).sum();
                if total == 0 {
                    bail!("route '{}': at least one target needs a weight above 0", r.name);
                }
            } else if r.targets.iter().any(|t| t.weight.is_some()) {
                bail!("route '{}': weight is only used with strategy: weighted", r.name);
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

        if self.keys.is_empty() {
            bail!("at least one API key must be configured");
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
        Ok(())
    }
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
