//! Upstream providers: building requests, translating responses and errors.

pub mod anthropic;
pub mod bedrock;
pub mod openai;

use std::time::Duration;

use anyhow::Context;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, RequestBuilder,
};
use serde_json::{json, Value};

use crate::{
    breaker::CircuitBreaker,
    config::{ProviderConfig, ProviderKind},
    error::ApiError,
    sse::StreamTranslator,
    usage::Usage,
};

pub struct Provider {
    pub name: String,
    pub cfg: ProviderConfig,
    pub breaker: CircuitBreaker,
    client: Client,
    extra_headers: HeaderMap,
    aws_keys: Option<crate::aws::AwsCredentials>,
    aws: crate::aws::CredentialCache,
    gcp_key: Option<Value>,
    gcp: crate::gcp::TokenSource,
}

/// The wire format a target speaks, which can depend on the model (Vertex
/// serves Claude through the Anthropic format).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    OpenAi,
    Anthropic,
    Converse,
}

/// Vertex partner models in the Anthropic format.
fn vertex_claude(model: &str) -> bool {
    model.starts_with("claude-") || model.starts_with("anthropic/")
}

impl Provider {
    pub fn new(name: &str, cfg: ProviderConfig, client: Client) -> anyhow::Result<Self> {
        let mut extra_headers = HeaderMap::new();
        for (k, v) in &cfg.extra_headers {
            let name_h = HeaderName::from_bytes(k.as_bytes())
                .with_context(|| format!("provider {name}: invalid header name {k}"))?;
            let mut value = HeaderValue::from_str(v.expose())
                .with_context(|| format!("provider {name}: invalid header value for {k}"))?;
            value.set_sensitive(true);
            extra_headers.insert(name_h, value);
        }
        // A probe that never reports back frees its slot after one full
        // request timeout plus margin.
        let probe_timeout = Duration::from_secs(cfg.timeout_secs.saturating_mul(2).max(30));
        let breaker = CircuitBreaker::new(
            name,
            cfg.breaker_failure_threshold,
            Duration::from_secs(cfg.breaker_open_secs),
            probe_timeout,
        );
        let aws_keys = match (&cfg.aws_access_key_id, &cfg.aws_secret_access_key) {
            (Some(id), Some(secret)) => Some(crate::aws::AwsCredentials {
                access_key_id: id.expose().to_string(),
                secret_access_key: secret.expose().to_string(),
                session_token: cfg.aws_session_token.as_ref().map(|t| t.expose().to_string()),
                expires_at: None,
            }),
            _ => None,
        };
        let gcp_key = match (&cfg.credentials_json, &cfg.credentials_file) {
            (Some(j), _) => Some(
                serde_json::from_str::<Value>(j.expose())
                    .with_context(|| format!("provider {name}: credentials_json is not valid JSON"))?,
            ),
            (None, Some(path)) => {
                let raw = std::fs::read_to_string(path)
                    .with_context(|| format!("provider {name}: reading credentials_file {path}"))?;
                Some(
                    serde_json::from_str::<Value>(&raw)
                        .with_context(|| format!("provider {name}: {path} is not valid JSON"))?,
                )
            }
            _ => None,
        };
        if let Some(k) = &gcp_key {
            let kind = k.get("type").and_then(Value::as_str).unwrap_or("");
            if !matches!(kind, "service_account" | "authorized_user") {
                anyhow::bail!(
                    "provider {name}: Google credentials must be a service_account or authorized_user key"
                );
            }
        }
        Ok(Self {
            name: name.to_string(),
            cfg,
            breaker,
            client,
            extra_headers,
            aws_keys,
            aws: Default::default(),
            gcp_key,
            gcp: Default::default(),
        })
    }

    pub fn dialect(&self, model: &str) -> Dialect {
        match self.cfg.kind {
            ProviderKind::Openai | ProviderKind::Azure => Dialect::OpenAi,
            ProviderKind::Anthropic => Dialect::Anthropic,
            ProviderKind::Bedrock => Dialect::Converse,
            ProviderKind::Vertex if vertex_claude(model) => Dialect::Anthropic,
            ProviderKind::Vertex => Dialect::OpenAi,
        }
    }

    /// `{base}/projects/{project}/locations/{region}`.
    fn vertex_base(&self) -> String {
        format!(
            "{}/projects/{}/locations/{}",
            self.base(),
            self.cfg.project.as_deref().unwrap_or_default(),
            self.cfg.region.as_deref().unwrap_or("us-central1")
        )
    }

    /// Sends a built request, first signing it (Bedrock) or adding a Google
    /// access token (Vertex) when no API key is in use.
    pub async fn send(&self, rb: RequestBuilder, key: &str) -> Result<reqwest::Response, String> {
        let mut req = rb.build().map_err(|e| e.to_string())?;
        if key.is_empty() {
            match self.cfg.kind {
                ProviderKind::Bedrock => {
                    let creds = self.aws.get(&self.client, self.aws_keys.clone()).await?;
                    let region = self.cfg.region.as_deref().unwrap_or("us-east-1");
                    crate::aws::sign(&mut req, &creds, region, "bedrock", crate::proxy::unix_secs());
                }
                ProviderKind::Vertex => {
                    let token = self.gcp.token(&self.client, self.gcp_key.as_ref()).await?;
                    let v = Self::secret_header(&format!("Bearer {token}")).map_err(|e| e.message)?;
                    req.headers_mut().insert(reqwest::header::AUTHORIZATION, v);
                }
                _ => {}
            }
        }
        self.client.execute(req).await.map_err(|e| e.to_string())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.timeout_secs)
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.stream_idle_timeout_secs)
    }

    fn secret_header(value: &str) -> Result<HeaderValue, ApiError> {
        let mut v = HeaderValue::from_str(value)
            .map_err(|_| ApiError::internal_config("provider api_key contains invalid characters"))?;
        v.set_sensitive(true);
        Ok(v)
    }

    fn base(&self) -> &str {
        self.cfg.base_url.trim_end_matches('/')
    }

    /// Adds this provider's credentials (`key`) in the header it expects.
    fn authed(&self, rb: RequestBuilder, key: &str) -> Result<RequestBuilder, ApiError> {
        let rb = match self.cfg.kind {
            ProviderKind::Openai => rb.bearer_auth(key),
            ProviderKind::Azure => rb.header("api-key", Self::secret_header(key)?),
            ProviderKind::Anthropic => rb.header("x-api-key", Self::secret_header(key)?),
            // An API key (Bedrock API key, Google access token) when given;
            // otherwise `send` signs the request.
            ProviderKind::Bedrock | ProviderKind::Vertex if !key.is_empty() => rb.bearer_auth(key),
            ProviderKind::Bedrock | ProviderKind::Vertex => rb,
        };
        Ok(rb.headers(self.extra_headers.clone()))
    }

    /// Builds the upstream request for a chat completion (translated for
    /// Anthropic).
    pub fn build(
        &self,
        body: &Value,
        model: &str,
        stream: bool,
        key: &str,
        auto_cache: bool,
    ) -> Result<RequestBuilder, ApiError> {
        let base = self.base();
        let rb = match self.cfg.kind {
            ProviderKind::Openai => {
                let b = openai::prepare(body, model, stream && self.cfg.stream_usage);
                self.client.post(format!("{base}/chat/completions")).json(&b)
            }
            ProviderKind::Azure => {
                let b = openai::prepare(body, model, stream && self.cfg.stream_usage);
                // Validated at config load: deployment name and api_version are URL-safe.
                let version = self.cfg.api_version.as_deref().unwrap_or_default();
                self.client.post(format!("{base}/{model}/chat/completions?api-version={version}")).json(&b)
            }
            ProviderKind::Anthropic => {
                let mut b = anthropic::to_request(body, model, stream, self.cfg.default_max_tokens)?;
                if auto_cache || self.cfg.auto_prompt_cache {
                    anthropic::add_cache_markers(&mut b);
                }
                self.client
                    .post(format!("{base}/messages"))
                    .header("anthropic-version", anthropic::API_VERSION)
                    .json(&b)
            }
            ProviderKind::Bedrock => {
                let b = bedrock::to_request(
                    body,
                    self.cfg.default_max_tokens,
                    auto_cache || self.cfg.auto_prompt_cache,
                )?;
                let op = if stream { "converse-stream" } else { "converse" };
                let id = crate::aws::uri_encode(model, false);
                self.client.post(format!("{base}/model/{id}/{op}")).json(&b)
            }
            ProviderKind::Vertex if vertex_claude(model) => {
                let mut b = anthropic::to_request(body, model, stream, self.cfg.default_max_tokens)?;
                if auto_cache || self.cfg.auto_prompt_cache {
                    anthropic::add_cache_markers(&mut b);
                }
                self.vertex_anthropic(b, model, stream)
            }
            ProviderKind::Vertex => {
                // Gemini ids are published as `google/<model>`; other partner
                // models are given with their publisher (`meta/llama-...`).
                let full = if model.contains('/') { model.to_string() } else { format!("google/{model}") };
                let b = openai::prepare(body, &full, stream && self.cfg.stream_usage);
                self.client
                    .post(format!("{}/endpoints/openapi/chat/completions", self.vertex_base()))
                    .json(&b)
            }
        };
        self.authed(rb, key)
    }

    /// A Claude request on Vertex: the model goes in the URL and
    /// `anthropic_version` in the body.
    fn vertex_anthropic(&self, mut b: Value, model: &str, stream: bool) -> RequestBuilder {
        let model = model.strip_prefix("anthropic/").unwrap_or(model);
        if let Some(o) = b.as_object_mut() {
            o.remove("model");
            o.insert("anthropic_version".into(), json!(anthropic::VERTEX_VERSION));
            if stream {
                o.insert("stream".into(), json!(true));
            }
        }
        let op = if stream { "streamRawPredict" } else { "rawPredict" };
        self.client.post(format!("{}/publishers/anthropic/models/{model}:{op}", self.vertex_base())).json(&b)
    }

    /// Builds a request for another OpenAI-style JSON endpoint
    /// (`embeddings`, `responses`, `images/generations`, `moderations`,
    /// `rerank`, `audio/speech`) with the model replaced.
    pub fn build_json(
        &self,
        path: &str,
        body: &Value,
        model: &str,
        key: &str,
    ) -> Result<RequestBuilder, ApiError> {
        let base = self.base();
        let mut b = body.clone();
        if let Some(o) = b.as_object_mut() {
            o.insert("model".into(), Value::String(model.to_string()));
        }
        let rb = match self.cfg.kind {
            ProviderKind::Openai => self.client.post(format!("{base}/{path}")).json(&b),
            ProviderKind::Azure => {
                let version = self.cfg.api_version.as_deref().unwrap_or_default();
                self.client.post(format!("{base}/{model}/{path}?api-version={version}")).json(&b)
            }
            ProviderKind::Anthropic | ProviderKind::Bedrock | ProviderKind::Vertex => {
                return Err(ApiError::bad_request(format!(
                    "provider '{}' does not support /{path}",
                    self.name
                )))
            }
        };
        self.authed(rb, key)
    }

    /// Builds a native Anthropic Messages API request. The client's
    /// `anthropic-version` and `anthropic-beta` headers are forwarded.
    pub fn build_messages(
        &self,
        body: &Value,
        model: &str,
        key: &str,
        client: &axum::http::HeaderMap,
        auto_cache: bool,
    ) -> Result<RequestBuilder, ApiError> {
        if self.dialect(model) != Dialect::Anthropic {
            return Err(ApiError::bad_request(format!(
                "provider '{}' does not serve the Messages API for '{model}'",
                self.name
            )));
        }
        let mut b = body.clone();
        if let Some(o) = b.as_object_mut() {
            o.insert("model".into(), Value::String(model.to_string()));
            if !o.contains_key("max_tokens") {
                o.insert("max_tokens".into(), json!(self.cfg.default_max_tokens));
            }
        }
        if auto_cache || self.cfg.auto_prompt_cache {
            anthropic::add_cache_markers(&mut b);
        }
        if self.cfg.kind == ProviderKind::Vertex {
            let stream = b.get("stream").and_then(Value::as_bool).unwrap_or(false);
            let mut rb = self.vertex_anthropic(b, model, stream);
            if let Some(beta) = client.get("anthropic-beta") {
                rb = rb.header("anthropic-beta", beta.clone());
            }
            return self.authed(rb, key);
        }
        let version = client
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() < 64)
            .unwrap_or(anthropic::API_VERSION)
            .to_string();
        let mut rb = self
            .client
            .post(format!("{}/messages", self.base()))
            .header("anthropic-version", version)
            .json(&b);
        if let Some(beta) = client.get("anthropic-beta") {
            rb = rb.header("anthropic-beta", beta.clone());
        }
        self.authed(rb, key)
    }

    /// Builds a raw request to any path of this provider (provider-native
    /// passthrough). Only safe, content-describing client headers are forwarded.
    pub fn build_native(
        &self,
        method: reqwest::Method,
        path_and_query: &str,
        body: axum::body::Bytes,
        key: &str,
        client: &axum::http::HeaderMap,
    ) -> Result<RequestBuilder, ApiError> {
        let mut url = format!("{}/{}", self.base(), path_and_query.trim_start_matches('/'));
        if self.cfg.kind == ProviderKind::Azure && !url.contains("api-version=") {
            let version = self.cfg.api_version.as_deref().unwrap_or_default();
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(&format!("api-version={version}"));
        }
        let mut rb = self.client.request(method, url).body(body);
        for h in ["content-type", "accept", "anthropic-beta", "openai-beta"] {
            if let Some(v) = client.get(h) {
                rb = rb.header(h, v.clone());
            }
        }
        if self.cfg.kind == ProviderKind::Anthropic {
            let version = client
                .get("anthropic-version")
                .cloned()
                .unwrap_or(HeaderValue::from_static(anthropic::API_VERSION));
            rb = rb.header("anthropic-version", version);
        }
        self.authed(rb, key)
    }

    pub fn supports(&self, endpoint: crate::proxy::Endpoint) -> bool {
        endpoint.supports(self.cfg.kind)
    }

    /// Converts a successful non-streaming upstream body to an OpenAI
    /// chat completion and extracts token usage.
    pub fn translate_response(&self, bytes: &[u8], model: &str) -> Result<(Vec<u8>, Usage), ApiError> {
        let v: Value = serde_json::from_slice(bytes)
            .map_err(|_| ApiError::upstream("upstream returned an invalid response"))?;
        let (out, usage) = match self.dialect(model) {
            Dialect::OpenAi => {
                let usage = Usage::sniff(&v).unwrap_or_default();
                return Ok((bytes.to_vec(), usage));
            }
            Dialect::Anthropic => anthropic::from_response(&v, model),
            Dialect::Converse => bedrock::from_response(&v, model),
        };
        let body = serde_json::to_vec(&out)
            .map_err(|_| ApiError::upstream("failed to encode translated response"))?;
        Ok((body, usage))
    }

    /// Normalises an upstream client-error body into the OpenAI error shape.
    /// Both OpenAI and Anthropic nest details under `error`.
    pub fn translate_error(&self, status: u16, bytes: &[u8]) -> Value {
        if let Ok(mut v) = serde_json::from_slice::<Value>(bytes) {
            // Vertex's OpenAI endpoint wraps errors in a one-element array.
            if let Some(first) = v.as_array().and_then(|a| a.first()).cloned() {
                v = first;
            }
            // Bedrock: `{"message": "..."}`.
            if v.get("error").is_none() {
                if let Some(m) = v.get("message").and_then(Value::as_str) {
                    let message: String = m.chars().take(2000).collect();
                    return json!({"error": {"message": message, "type": "invalid_request_error", "code": null}});
                }
            }
            if let Some(e) = v.get("error") {
                let message = e.get("message").and_then(Value::as_str).unwrap_or("upstream error");
                let kind = e.get("type").and_then(Value::as_str).unwrap_or("invalid_request_error");
                let code = e.get("code").cloned().unwrap_or(Value::Null);
                let message: String = message.chars().take(2000).collect();
                return json!({"error": {"message": message, "type": kind, "code": code}});
            }
        }
        json!({"error": {
            "message": format!("upstream provider returned status {status}"),
            "type": "upstream_error",
            "code": null
        }})
    }

    pub fn stream_translator(&self, model: &str, client_wants_usage: bool) -> Box<dyn StreamTranslator> {
        match self.dialect(model) {
            Dialect::Anthropic => Box::new(anthropic::StreamTx::new(model, client_wants_usage)),
            Dialect::Converse => Box::new(bedrock::StreamTx::new(model, client_wants_usage)),
            Dialect::OpenAi => Box::new(openai::Passthrough::new(client_wants_usage)),
        }
    }
}
