//! Upstream providers: building requests, translating responses and errors.

pub mod anthropic;
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
        Ok(Self { name: name.to_string(), cfg, breaker, client, extra_headers })
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
        };
        self.authed(rb, key)
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
            ProviderKind::Anthropic => {
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
        if self.cfg.kind != ProviderKind::Anthropic {
            return Err(ApiError::bad_request(format!(
                "provider '{}' does not serve the Messages API",
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
        match self.cfg.kind {
            ProviderKind::Openai | ProviderKind::Azure => {
                let v: Value = serde_json::from_slice(bytes)
                    .map_err(|_| ApiError::upstream("upstream returned an invalid response"))?;
                let usage = Usage::sniff(&v).unwrap_or_default();
                Ok((bytes.to_vec(), usage))
            }
            ProviderKind::Anthropic => {
                let v: Value = serde_json::from_slice(bytes)
                    .map_err(|_| ApiError::upstream("upstream returned an invalid response"))?;
                let (out, usage) = anthropic::from_response(&v, model);
                let body = serde_json::to_vec(&out)
                    .map_err(|_| ApiError::upstream("failed to encode translated response"))?;
                Ok((body, usage))
            }
        }
    }

    /// Normalises an upstream client-error body into the OpenAI error shape.
    /// Both OpenAI and Anthropic nest details under `error`.
    pub fn translate_error(&self, status: u16, bytes: &[u8]) -> Value {
        if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
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
        match self.cfg.kind {
            ProviderKind::Anthropic => Box::new(anthropic::StreamTx::new(model, client_wants_usage)),
            ProviderKind::Openai | ProviderKind::Azure => {
                Box::new(openai::Passthrough::new(client_wants_usage))
            }
        }
    }
}
