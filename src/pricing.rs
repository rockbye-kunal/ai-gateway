//! Model prices, so every request gets a cost without configuring prices
//! by hand.
//!
//! Prices come from, in order of precedence:
//! 1. `price_*_per_mtok` on a route target;
//! 2. `pricing.overrides` in the config (`provider/model` or `model`);
//! 3. a price list in LiteLLM's `model_prices_and_context_window.json`
//!    format, loaded from `pricing.url` (default: LiteLLM's public file) or
//!    `pricing.file`, refreshed every `pricing.refresh_hours`, and kept in
//!    the store so replicas share it and a restart does not need the network.
//!
//! Provider-reported cost (OpenRouter's `usage.cost`) still wins over all of
//! these, and a client's `custom-cost` header over everything.

use std::{collections::HashMap, time::Duration};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::{PricingConfig, ProviderConfig, ProviderKind, TargetConfig},
    usage::Usage,
};

const STORE_KEY: &str = "pricing:table";
const STORE_TTL_SECS: u64 = 30 * 86_400;

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    #[serde(default)]
    pub cached_input_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
}

impl Price {
    /// Cost of `u` in USD. Cached input is charged at its own rate when
    /// known; `prompt_tokens` already includes cached and cache-write tokens.
    pub fn cost(&self, u: &Usage) -> f64 {
        let cached = u.cached_tokens.min(u.prompt_tokens);
        let write = u.cache_write_tokens.min(u.prompt_tokens - cached);
        let plain = u.prompt_tokens - cached - write;
        let per = |t: u64, rate: f64| t as f64 * rate / 1_000_000.0;
        per(plain, self.input_per_mtok)
            + per(cached, self.cached_input_per_mtok.unwrap_or(self.input_per_mtok))
            + per(write, self.cache_write_per_mtok.unwrap_or(self.input_per_mtok))
            + per(u.completion_tokens, self.output_per_mtok)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Table {
    prices: HashMap<String, Price>,
    updated_at: u64,
    source: String,
}

#[derive(Default)]
pub struct Pricing {
    table: RwLock<Table>,
}

/// Parses a LiteLLM-format price list (per-token prices) into per-million prices.
pub fn parse_litellm(v: &Value) -> HashMap<String, Price> {
    let mut out = HashMap::new();
    let Some(obj) = v.as_object() else { return out };
    for (name, e) in obj {
        if name == "sample_spec" {
            continue;
        }
        let num = |k: &str| e.get(k).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0);
        let (Some(i), Some(o)) = (num("input_cost_per_token"), num("output_cost_per_token")) else {
            continue;
        };
        out.insert(
            name.clone(),
            Price {
                input_per_mtok: i * 1e6,
                output_per_mtok: o * 1e6,
                cached_input_per_mtok: num("cache_read_input_token_cost").map(|x| x * 1e6),
                cache_write_per_mtok: num("cache_creation_input_token_cost").map(|x| x * 1e6),
            },
        );
    }
    out
}

/// The LiteLLM key prefix for an OpenAI-compatible provider, from its host.
fn host_prefix(base_url: &str) -> Option<&'static str> {
    let host = base_url.split("://").nth(1).unwrap_or(base_url).split('/').next().unwrap_or("");
    let table = [
        ("api.openai.com", ""),
        ("groq.com", "groq/"),
        ("generativelanguage.googleapis.com", "gemini/"),
        ("api.mistral.ai", "mistral/"),
        ("api.together.xyz", "together_ai/"),
        ("api.deepseek.com", "deepseek/"),
        ("api.x.ai", "xai/"),
        ("api.fireworks.ai", "fireworks_ai/"),
        ("api.cerebras.ai", "cerebras/"),
        ("api.perplexity.ai", "perplexity/"),
        ("api.anthropic.com", ""),
    ];
    table.iter().find(|(h, _)| host.ends_with(h)).map(|(_, p)| *p)
}

/// Strips a trailing `-YYYYMMDD`, `-YYYY-MM-DD` or `-latest`.
fn undated(model: &str) -> Option<String> {
    if let Some(m) = model.strip_suffix("-latest") {
        return Some(m.to_string());
    }
    let (head, tail) = model.rsplit_once('-')?;
    if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) {
        return Some(head.to_string());
    }
    // 2024-08-06 style
    let parts: Vec<&str> = model.rsplitn(4, '-').collect();
    if parts.len() == 4
        && parts[0].len() == 2
        && parts[1].len() == 2
        && parts[2].len() == 4
        && parts[..3].iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
    {
        return Some(parts[3].to_string());
    }
    None
}

/// Price-list keys to try for a target, most specific first.
pub fn candidates(provider_name: &str, p: &ProviderConfig, model: &str) -> Vec<String> {
    let mut c = Vec::new();
    let mut push = |s: String| {
        if !c.contains(&s) {
            c.push(s);
        }
    };
    if let Some(prefix) = &p.pricing_prefix {
        push(format!("{prefix}{model}"));
    }
    match p.kind {
        ProviderKind::Bedrock => {
            push(format!("bedrock/{}/{model}", p.region.as_deref().unwrap_or("")));
            push(model.to_string());
            push(format!("bedrock/{model}"));
            // Cross-region inference profiles: us.anthropic... -> anthropic...
            if let Some((scope, rest)) = model.split_once('.') {
                if matches!(scope, "us" | "eu" | "apac" | "au" | "global" | "jp" | "ca") {
                    push(rest.to_string());
                }
            }
        }
        ProviderKind::Vertex => {
            let bare = model.split('/').next_back().unwrap_or(model);
            push(format!("vertex_ai/{bare}"));
            push(bare.to_string());
            push(format!("gemini/{bare}"));
            if let Some(b) = bare.split_once('@').map(|(m, _)| m) {
                push(format!("vertex_ai/{b}"));
                push(b.to_string());
            }
        }
        ProviderKind::Azure => {
            push(format!("azure/{model}"));
            push(model.to_string());
        }
        ProviderKind::Anthropic => {
            push(model.to_string());
            push(format!("anthropic/{model}"));
        }
        ProviderKind::Openai => {
            if let Some(prefix) = host_prefix(&p.base_url) {
                push(format!("{prefix}{model}"));
            }
            push(format!("{provider_name}/{model}"));
            push(model.to_string());
        }
    }
    let undated: Vec<String> = c.iter().filter_map(|k| undated(k)).collect();
    for u in undated {
        if !c.contains(&u) {
            c.push(u);
        }
    }
    c
}

impl Pricing {
    pub fn len(&self) -> usize {
        self.table.read().prices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn status(&self) -> (usize, u64, String) {
        let t = self.table.read();
        (t.prices.len(), t.updated_at, t.source.clone())
    }

    fn install(&self, prices: HashMap<String, Price>, updated_at: u64, source: String) {
        *self.table.write() = Table { prices, updated_at, source };
    }

    /// The price for a target and where it came from (`route`, `override`,
    /// `list`), or None when unknown.
    pub fn lookup(
        &self,
        cfg: &PricingConfig,
        provider_name: &str,
        p: &ProviderConfig,
        target: &TargetConfig,
    ) -> Option<(Price, &'static str)> {
        if let (Some(i), Some(o)) = (target.price_input_per_mtok, target.price_output_per_mtok) {
            return Some((
                Price {
                    input_per_mtok: i,
                    output_per_mtok: o,
                    cached_input_per_mtok: None,
                    cache_write_per_mtok: None,
                },
                "route",
            ));
        }
        let full = format!("{provider_name}/{}", target.model);
        if let Some(o) = cfg.overrides.get(&full).or_else(|| cfg.overrides.get(&target.model)) {
            return Some((*o, "override"));
        }
        let t = self.table.read();
        candidates(provider_name, p, &target.model).iter().find_map(|k| t.prices.get(k)).map(|p| (*p, "list"))
    }

    /// Loads the shared copy from the store, then fetches a fresh list if
    /// it is older than the refresh interval (or always, with `force`).
    pub async fn refresh(&self, app: &crate::app::AppState, force: bool) {
        let rt = app.rt();
        let cfg = &rt.cfg.pricing;
        if !cfg.enabled {
            return;
        }
        let now = crate::proxy::unix_secs();
        if self.is_empty() {
            if let Ok(Some(raw)) = app.store.kv_get(STORE_KEY).await {
                if let Ok(t) = serde_json::from_str::<Table>(&raw) {
                    self.install(t.prices, t.updated_at, t.source);
                }
            }
        }
        let (_, updated, _) = self.status();
        if !force && !self.is_empty() && now.saturating_sub(updated) < cfg.refresh_hours * 3600 {
            return;
        }
        let (source, fetched) = match &cfg.file {
            Some(path) => (path.clone(), std::fs::read_to_string(path).map_err(|e| e.to_string())),
            None => (cfg.url.clone(), fetch(&app.http, &cfg.url).await),
        };
        let parsed = fetched.and_then(|s| serde_json::from_str::<Value>(&s).map_err(|e| e.to_string()));
        match parsed.map(|v| parse_litellm(&v)) {
            Ok(prices) if !prices.is_empty() => {
                tracing::info!(models = prices.len(), source = %source, "model price list loaded");
                let table = Table { prices, updated_at: now, source };
                if let Ok(raw) = serde_json::to_string(&table) {
                    if let Err(e) = app.store.kv_set(STORE_KEY, &raw, STORE_TTL_SECS).await {
                        tracing::warn!(error = %e, "could not share the price list through the store");
                    }
                }
                self.install(table.prices, table.updated_at, table.source);
                metrics::counter!("gateway_pricing_refresh_total", "result" => "ok").increment(1);
            }
            Ok(_) => {
                tracing::warn!(source = %source, "price list had no usable entries; keeping the current one")
            }
            Err(e) => {
                metrics::counter!("gateway_pricing_refresh_total", "result" => "failed").increment(1);
                tracing::warn!(error = %e, source = %source, "could not load the model price list; keeping the current one");
            }
        }
    }
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<String, String> {
    let r = http.get(url).timeout(Duration::from_secs(60)).send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("HTTP {}", r.status()));
    }
    r.text().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(kind: ProviderKind, base_url: &str) -> ProviderConfig {
        let yaml = format!(
            "kind: {}\nbase_url: {base_url}\napi_key: k\n",
            serde_json::to_value(kind).unwrap().as_str().unwrap()
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn parses_litellm_and_prices_usage() {
        let list = parse_litellm(&json!({
            "sample_spec": {"input_cost_per_token": 0, "output_cost_per_token": 0},
            "gpt-4o": {"input_cost_per_token": 2.5e-6, "output_cost_per_token": 1e-5, "cache_read_input_token_cost": 1.25e-6},
            "dall-e-3": {"output_cost_per_image": 0.04}
        }));
        assert_eq!(list.len(), 1);
        let p = list["gpt-4o"];
        assert!((p.input_per_mtok - 2.5).abs() < 1e-9);
        let u = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 100_000,
            cached_tokens: 400_000,
            ..Default::default()
        };
        // 600k plain at 2.5 + 400k cached at 1.25 + 100k out at 10
        assert!((p.cost(&u) - (1.5 + 0.5 + 1.0)).abs() < 1e-9);
    }

    #[test]
    fn candidate_keys() {
        let openai = provider(ProviderKind::Openai, "https://api.openai.com/v1");
        assert_eq!(candidates("openai", &openai, "gpt-4o")[0], "gpt-4o");
        let groq = provider(ProviderKind::Openai, "https://api.groq.com/openai/v1");
        assert_eq!(candidates("groq", &groq, "llama-3.3-70b")[0], "groq/llama-3.3-70b");
        let anth = provider(ProviderKind::Anthropic, "https://api.anthropic.com/v1");
        assert!(candidates("anthropic", &anth, "claude-sonnet-4-5-20250929")
            .contains(&"claude-sonnet-4-5".to_string()));
        let mut br = provider(ProviderKind::Bedrock, "https://bedrock-runtime.us-east-1.amazonaws.com");
        br.region = Some("us-east-1".into());
        let c = candidates("bedrock", &br, "us.anthropic.claude-3-5-sonnet-20240620-v1:0");
        assert!(c.contains(&"anthropic.claude-3-5-sonnet-20240620-v1:0".to_string()));
        let vx = provider(ProviderKind::Vertex, "https://us-central1-aiplatform.googleapis.com/v1");
        assert!(candidates("vertex", &vx, "google/gemini-2.5-flash")
            .contains(&"vertex_ai/gemini-2.5-flash".to_string()));
        assert_eq!(undated("gpt-4o-2024-08-06").as_deref(), Some("gpt-4o"));
        assert_eq!(undated("claude-3-5-haiku-latest").as_deref(), Some("claude-3-5-haiku"));
        assert_eq!(undated("gpt-4o"), None);
    }

    #[test]
    fn precedence() {
        let pr = Pricing::default();
        let mut list = HashMap::new();
        list.insert(
            "gpt-4o".to_string(),
            Price {
                input_per_mtok: 2.5,
                output_per_mtok: 10.0,
                cached_input_per_mtok: None,
                cache_write_per_mtok: None,
            },
        );
        pr.install(list, 1, "test".into());
        let openai = provider(ProviderKind::Openai, "https://api.openai.com/v1");
        let mut cfg = PricingConfig::default();
        let t = TargetConfig {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            price_input_per_mtok: None,
            price_output_per_mtok: None,
            weight: None,
        };
        assert_eq!(pr.lookup(&cfg, "openai", &openai, &t).unwrap().1, "list");
        cfg.overrides.insert(
            "openai/gpt-4o".into(),
            Price {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                cached_input_per_mtok: None,
                cache_write_per_mtok: None,
            },
        );
        assert_eq!(pr.lookup(&cfg, "openai", &openai, &t).unwrap().1, "override");
        let routed =
            TargetConfig { price_input_per_mtok: Some(9.0), price_output_per_mtok: Some(9.0), ..t.clone() };
        assert_eq!(pr.lookup(&cfg, "openai", &openai, &routed).unwrap().1, "route");
        let unknown = TargetConfig { model: "mystery".into(), ..t };
        assert!(pr.lookup(&cfg, "openai", &openai, &unknown).is_none());
    }
}
