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

    /// Builds the upstream HTTP request for one target.
    pub fn build(&self, body: &Value, model: &str, stream: bool) -> Result<RequestBuilder, ApiError> {
        let base = self.cfg.base_url.trim_end_matches('/');
        let key = self.cfg.api_key.expose();
        let rb = match self.cfg.kind {
            ProviderKind::Openai => {
                let b = openai::prepare(body, model, stream && self.cfg.stream_usage);
                self.client.post(format!("{base}/chat/completions")).bearer_auth(key).json(&b)
            }
            ProviderKind::Azure => {
                let b = openai::prepare(body, model, stream && self.cfg.stream_usage);
                // Validated at config load: deployment name and api_version are URL-safe.
                let version = self.cfg.api_version.as_deref().unwrap_or_default();
                self.client
                    .post(format!("{base}/{model}/chat/completions?api-version={version}"))
                    .header("api-key", Self::secret_header(key)?)
                    .json(&b)
            }
            ProviderKind::Anthropic => {
                let b = anthropic::to_request(body, model, stream, self.cfg.default_max_tokens)?;
                self.client
                    .post(format!("{base}/messages"))
                    .header("x-api-key", Self::secret_header(key)?)
                    .header("anthropic-version", anthropic::API_VERSION)
                    .json(&b)
            }
        };
        Ok(rb.headers(self.extra_headers.clone()))
    }

    /// Converts a successful non-streaming upstream body to an OpenAI
    /// chat completion and extracts token usage.
    pub fn translate_response(&self, bytes: &[u8], model: &str) -> Result<(Vec<u8>, Usage), ApiError> {
        match self.cfg.kind {
            ProviderKind::Openai | ProviderKind::Azure => {
                let v: Value = serde_json::from_slice(bytes)
                    .map_err(|_| ApiError::upstream("upstream returned an invalid response"))?;
                let usage = Usage::from_openai(&v).unwrap_or_default();
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
