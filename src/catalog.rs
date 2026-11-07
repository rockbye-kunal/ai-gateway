//! Live model lists from providers, for `/v1/models` and the dashboard.
//!
//! Each provider that has a key and a model-list API is asked for its models
//! (OpenAI-compatible `GET /models`, Anthropic `GET /v1/models`, Bedrock
//! `ListFoundationModels` and `ListInferenceProfiles`). Answers are cached for
//! an hour, in memory and in the store so replicas share them. A provider that
//! fails keeps its last good list and is asked again a minute later.

use std::{collections::HashMap, sync::Arc, time::Duration};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    app::AppState,
    config::ProviderKind,
    providers::Provider,
    proxy::{provider_ready, unix_secs},
};

const TTL_SECS: u64 = 3600;
const RETRY_SECS: u64 = 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    /// Prices the provider publishes with its list (OpenRouter), per million
    /// tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderModels {
    pub provider: String,
    /// When the list was fetched (unix seconds); 0 if never.
    pub fetched_at: u64,
    pub models: Vec<ModelInfo>,
    /// Why the last fetch failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When to ask again (unix seconds).
    #[serde(default)]
    pub next_fetch: u64,
}

#[derive(Default)]
pub struct Catalog {
    cache: RwLock<HashMap<String, ProviderModels>>,
}

/// Cache key: the list depends on the provider's endpoint and kind.
fn cache_key(p: &Provider) -> String {
    let mut h = Sha256::new();
    h.update(format!("{:?}|{}", p.cfg.kind, p.cfg.base_url).as_bytes());
    format!("catalog:{}:{}", p.name, &hex::encode(h.finalize())[..16])
}

/// The key used to list models: a stored key, else the configured one (never
/// a client's BYOK key).
fn listing_key(app: &AppState, p: &Provider) -> String {
    match app.stored_provider_keys.read().get(&p.name) {
        Some(k) => k.expose().to_string(),
        None => p.cfg.api_key.expose().to_string(),
    }
}

impl Catalog {
    /// Model lists of every provider that has a key, fetching the ones that
    /// are missing or stale (or all of them with `force`).
    pub async fn lists(&self, app: &AppState, force: bool) -> Vec<ProviderModels> {
        let rt = app.rt();
        let now = unix_secs();
        let mut out = Vec::new();
        let mut tasks = tokio::task::JoinSet::new();
        let mut names: Vec<&String> = rt.providers.keys().collect();
        names.sort();
        for name in names {
            let p = rt.providers[name].clone();
            if !provider_ready(app, &p, None) {
                continue;
            }
            let key = cache_key(&p);
            let cached = self.cache.read().get(&key).cloned();
            if let Some(c) = &cached {
                if !force && now < c.next_fetch {
                    out.push(c.clone());
                    continue;
                }
            }
            if !force {
                if let Ok(Some(raw)) = app.store.kv_get(&key).await {
                    if let Ok(c) = serde_json::from_str::<ProviderModels>(&raw) {
                        if now < c.next_fetch {
                            self.cache.write().insert(key, c.clone());
                            out.push(c);
                            continue;
                        }
                    }
                }
            }
            let api_key = listing_key(app, &p);
            let Some(requests) = p.list_models(&api_key) else {
                out.push(ProviderModels {
                    provider: p.name.clone(),
                    error: Some(match p.cfg.kind {
                        ProviderKind::Vertex => {
                            "Vertex AI has no model list API; use model names from Google's docs".into()
                        }
                        _ => "this provider type has no model list API".into(),
                    }),
                    ..Default::default()
                });
                continue;
            };
            tasks.spawn(async move {
                let result = match requests {
                    Ok(reqs) => fetch(&p, reqs, &api_key).await,
                    Err(e) => Err(e.message),
                };
                (p.name.clone(), key, result, cached)
            });
        }
        while let Some(done) = tasks.join_next().await {
            let Ok((provider, key, result, cached)) = done else { continue };
            let entry = match result {
                Ok(mut models) => {
                    models.sort_by(|a, b| a.id.cmp(&b.id));
                    models.dedup_by(|a, b| a.id == b.id);
                    metrics::counter!("gateway_catalog_fetch_total", "provider" => provider.clone(), "result" => "ok")
                        .increment(1);
                    ProviderModels {
                        provider,
                        fetched_at: now,
                        models,
                        error: None,
                        next_fetch: now + TTL_SECS,
                    }
                }
                Err(e) => {
                    metrics::counter!("gateway_catalog_fetch_total", "provider" => provider.clone(), "result" => "failed")
                        .increment(1);
                    tracing::warn!(%provider, error = %e, "could not fetch the provider's model list");
                    let old = cached.unwrap_or_default();
                    ProviderModels {
                        provider,
                        fetched_at: old.fetched_at,
                        models: old.models,
                        error: Some(e),
                        next_fetch: now + RETRY_SECS,
                    }
                }
            };
            if let Ok(raw) = serde_json::to_string(&entry) {
                if let Err(e) = app.store.kv_set(&key, &raw, TTL_SECS).await {
                    tracing::debug!(error = %e, "could not share the model list through the store");
                }
            }
            self.cache.write().insert(key, entry.clone());
            out.push(entry);
        }
        out.sort_by(|a, b| a.provider.cmp(&b.provider));
        out
    }
}

async fn fetch(
    p: &Arc<Provider>,
    requests: Vec<reqwest::RequestBuilder>,
    key: &str,
) -> Result<Vec<ModelInfo>, String> {
    let mut models = Vec::new();
    for (i, rb) in requests.into_iter().enumerate() {
        let resp = tokio::time::timeout(Duration::from_secs(20), p.send(rb, key))
            .await
            .map_err(|_| "timed out".to_string())??;
        let status = resp.status();
        if !status.is_success() {
            // Bedrock inference profiles are optional (older regions, or keys
            // without that permission); foundation models are enough.
            if p.cfg.kind == ProviderKind::Bedrock && i > 0 {
                continue;
            }
            return Err(format!("the provider answered {status}"));
        }
        let v: Value = resp.json().await.map_err(|e| format!("unreadable model list: {e}"))?;
        models.extend(match p.cfg.kind {
            ProviderKind::Anthropic => parse_anthropic(&v),
            ProviderKind::Bedrock if i == 0 => parse_bedrock_models(&v),
            ProviderKind::Bedrock => parse_bedrock_profiles(&v),
            _ => parse_openai(&v),
        });
    }
    Ok(models)
}

fn per_mtok(v: &Value) -> Option<f64> {
    let x = match v {
        Value::String(s) => s.parse::<f64>().ok()?,
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    (x.is_finite() && x >= 0.0).then(|| (x * 1e12).round() / 1e6)
}

/// OpenAI-compatible `GET /models` (`{"data": [{"id": ...}]}`), including
/// the extra fields OpenRouter, Groq, Mistral and Together add.
pub fn parse_openai(v: &Value) -> Vec<ModelInfo> {
    let items = v.get("data").and_then(Value::as_array).or_else(|| v.as_array());
    items
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?.trim();
            if id.is_empty() || !crate::config::valid_model(id) {
                return None;
            }
            let ctx = ["context_length", "context_window", "max_context_length"]
                .iter()
                .find_map(|k| m.get(*k).and_then(Value::as_u64))
                .or_else(|| m.pointer("/top_provider/context_length").and_then(Value::as_u64));
            Some(ModelInfo {
                id: id.to_string(),
                name: m
                    .get("name")
                    .or_else(|| m.get("display_name"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                context_length: ctx,
                created: m.get("created").and_then(Value::as_i64),
                input_per_mtok: m.pointer("/pricing/prompt").and_then(per_mtok),
                output_per_mtok: m.pointer("/pricing/completion").and_then(per_mtok),
            })
        })
        .collect()
}

/// Anthropic `GET /v1/models`.
pub fn parse_anthropic(v: &Value) -> Vec<ModelInfo> {
    v.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            crate::config::valid_model(id).then(|| ModelInfo {
                id: id.to_string(),
                name: m.get("display_name").and_then(Value::as_str).map(str::to_string),
                ..Default::default()
            })
        })
        .collect()
}

/// Bedrock `ListFoundationModels`: text models that can be called directly.
pub fn parse_bedrock_models(v: &Value) -> Vec<ModelInfo> {
    v.get("modelSummaries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| {
            let has = |field: &str, want: &str| {
                m.get(field).and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x == want))
            };
            has("outputModalities", "TEXT") && has("inferenceTypesSupported", "ON_DEMAND")
        })
        .filter_map(|m| {
            let id = m.get("modelId").and_then(Value::as_str)?;
            crate::config::valid_model(id).then(|| ModelInfo {
                id: id.to_string(),
                name: m.get("modelName").and_then(Value::as_str).map(str::to_string),
                ..Default::default()
            })
        })
        .collect()
}

/// Bedrock `ListInferenceProfiles`: cross-region profiles such as
/// `us.anthropic.claude-sonnet-4-5-…`, which newer models need.
pub fn parse_bedrock_profiles(v: &Value) -> Vec<ModelInfo> {
    v.get("inferenceProfileSummaries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("status").and_then(Value::as_str).is_none_or(|s| s == "ACTIVE"))
        .filter_map(|m| {
            let id = m.get("inferenceProfileId").and_then(Value::as_str)?;
            crate::config::valid_model(id).then(|| ModelInfo {
                id: id.to_string(),
                name: m.get("inferenceProfileName").and_then(Value::as_str).map(str::to_string),
                ..Default::default()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_openai_compatible_lists() {
        let v = json!({"object": "list", "data": [
            {"id": "gpt-4o-mini", "created": 1721172741, "owned_by": "system"},
            {"id": "openai/gpt-4o-mini", "name": "GPT-4o mini", "context_length": 128000,
             "pricing": {"prompt": "0.00000015", "completion": "0.0000006"}},
            {"id": "llama-3.3-70b-versatile", "context_window": 131072},
            {"id": ""}, {"id": "bad id with spaces"}, {"name": "no id"}
        ]});
        let m = parse_openai(&v);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].created, Some(1721172741));
        assert_eq!(m[1].context_length, Some(128000));
        assert_eq!(m[1].input_per_mtok, Some(0.15));
        assert_eq!(m[1].output_per_mtok, Some(0.6));
        assert_eq!(m[2].context_length, Some(131072));
    }

    #[test]
    fn parses_anthropic_and_bedrock_lists() {
        let a = parse_anthropic(
            &json!({"data": [{"id": "claude-haiku-4-5-20251001", "display_name": "Claude Haiku 4.5"}]}),
        );
        assert_eq!(a[0].name.as_deref(), Some("Claude Haiku 4.5"));
        let b = parse_bedrock_models(&json!({"modelSummaries": [
            {"modelId": "amazon.nova-pro-v1:0", "outputModalities": ["TEXT"], "inferenceTypesSupported": ["ON_DEMAND"]},
            {"modelId": "amazon.nova-canvas-v1:0", "outputModalities": ["IMAGE"], "inferenceTypesSupported": ["ON_DEMAND"]},
            {"modelId": "anthropic.claude-sonnet-4-5-20250929-v1:0", "outputModalities": ["TEXT"], "inferenceTypesSupported": ["INFERENCE_PROFILE"]}
        ]}));
        assert_eq!(b.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["amazon.nova-pro-v1:0"]);
        let p = parse_bedrock_profiles(&json!({"inferenceProfileSummaries": [
            {"inferenceProfileId": "us.anthropic.claude-sonnet-4-5-20250929-v1:0", "status": "ACTIVE"},
            {"inferenceProfileId": "us.old-model", "status": "DISABLED"}
        ]}));
        assert_eq!(p.len(), 1);
    }
}
