//! Providers the gateway knows without any configuration.
//!
//! With `auto_providers: true` (the default) every provider below is
//! available as `<name>/<model>` as soon as it has a key: from its usual
//! environment variable (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …), from a key
//! stored in the dashboard, or from a key the client sends
//! (`providerOptions.gateway.byok`). Until then requests for it go to
//! `default_provider`. A provider of the same name in the config file wins.
//!
//! `<NAME>_BASE_URL` (for example `OPENAI_BASE_URL`) overrides the endpoint.
//! AWS Bedrock is added when `AWS_REGION` and AWS credentials are in the
//! environment, and Google Vertex AI when `GOOGLE_CLOUD_PROJECT` is set.

use serde_json::json;

use crate::config::{ProviderConfig, ProviderSource};

pub struct Builtin {
    pub name: &'static str,
    /// Shown in the dashboard.
    pub label: &'static str,
    kind: &'static str,
    base_url: &'static str,
    /// Environment variables holding the API key, first match wins.
    key_env: &'static [&'static str],
}

pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "openai",
        label: "OpenAI",
        kind: "openai",
        base_url: "https://api.openai.com/v1",
        key_env: &["OPENAI_API_KEY"],
    },
    Builtin {
        name: "anthropic",
        label: "Anthropic",
        kind: "anthropic",
        base_url: "https://api.anthropic.com/v1",
        key_env: &["ANTHROPIC_API_KEY"],
    },
    Builtin {
        name: "google",
        label: "Google Gemini API",
        kind: "openai",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_env: &["GEMINI_API_KEY", "GOOGLE_GENERATIVE_AI_API_KEY"],
    },
    Builtin {
        name: "mistral",
        label: "Mistral",
        kind: "openai",
        base_url: "https://api.mistral.ai/v1",
        key_env: &["MISTRAL_API_KEY"],
    },
    Builtin {
        name: "deepseek",
        label: "DeepSeek",
        kind: "openai",
        base_url: "https://api.deepseek.com/v1",
        key_env: &["DEEPSEEK_API_KEY"],
    },
    Builtin {
        name: "xai",
        label: "xAI",
        kind: "openai",
        base_url: "https://api.x.ai/v1",
        key_env: &["XAI_API_KEY"],
    },
    Builtin {
        name: "groq",
        label: "Groq",
        kind: "openai",
        base_url: "https://api.groq.com/openai/v1",
        key_env: &["GROQ_API_KEY"],
    },
    Builtin {
        name: "together",
        label: "Together AI",
        kind: "openai",
        base_url: "https://api.together.xyz/v1",
        key_env: &["TOGETHER_API_KEY"],
    },
    Builtin {
        name: "fireworks",
        label: "Fireworks AI",
        kind: "openai",
        base_url: "https://api.fireworks.ai/inference/v1",
        key_env: &["FIREWORKS_API_KEY"],
    },
    Builtin {
        name: "cerebras",
        label: "Cerebras",
        kind: "openai",
        base_url: "https://api.cerebras.ai/v1",
        key_env: &["CEREBRAS_API_KEY"],
    },
    Builtin {
        name: "perplexity",
        label: "Perplexity",
        kind: "openai",
        base_url: "https://api.perplexity.ai",
        key_env: &["PERPLEXITY_API_KEY"],
    },
    Builtin {
        name: "openrouter",
        label: "OpenRouter",
        kind: "openai",
        base_url: "https://openrouter.ai/api/v1",
        key_env: &["OPENROUTER_API_KEY"],
    },
];

/// The dashboard label of a built-in provider.
pub fn label(name: &str) -> Option<&'static str> {
    match name {
        "bedrock" => Some("AWS Bedrock"),
        "vertex" => Some("Google Vertex AI"),
        _ => BUILTINS.iter().find(|b| b.name == name).map(|b| b.label),
    }
}

fn provider(v: serde_json::Value) -> ProviderConfig {
    let mut p: ProviderConfig = serde_json::from_value(v).expect("built-in provider definitions are valid");
    p.source = ProviderSource::Builtin;
    p
}

/// An endpoint from the environment the gateway will use: https, or plain
/// http to this machine, with no credentials, query or fragment.
fn usable_url(url: &str) -> bool {
    let local = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|p| url.strip_prefix(p).is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/'])));
    (url.starts_with("https://") || local)
        && !url.contains(['@', '?', '#'])
        && !url.contains(char::is_whitespace)
}

/// Built-in providers, given a way to read environment variables.
pub fn providers(env: &dyn Fn(&str) -> Option<String>) -> Vec<(String, ProviderConfig)> {
    let get = |k: &str| env(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let mut out = Vec::new();
    for b in BUILTINS {
        let key = b.key_env.iter().find_map(|k| get(k)).unwrap_or_default();
        let base_env = format!("{}_BASE_URL", b.name.to_ascii_uppercase());
        let base = match get(&base_env) {
            // SDKs read some of these variables too (OPENAI_BASE_URL), so a
            // value the gateway can't use is ignored, together with the
            // built-in provider: falling back to the public endpoint could
            // send a key meant for that URL somewhere else.
            Some(url) if !usable_url(&url) => {
                tracing::warn!(variable = %base_env, "ignoring built-in provider '{}': {base_env} must be https, or http to localhost", b.name);
                continue;
            }
            Some(url) => url,
            None => b.base_url.to_string(),
        };
        out.push((b.name.to_string(), provider(json!({"kind": b.kind, "base_url": base, "api_key": key}))));
    }

    // Bedrock: only with a region and a credential source the environment
    // shows (an EC2 instance role can't be detected without a network call;
    // configure the provider in the file for that).
    let region = get("AWS_REGION").or_else(|| get("AWS_DEFAULT_REGION"));
    let bearer = get("AWS_BEARER_TOKEN_BEDROCK");
    let aws_creds = bearer.is_some()
        || get("AWS_ACCESS_KEY_ID").is_some()
        || get("AWS_WEB_IDENTITY_TOKEN_FILE").is_some()
        || get("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
        || get("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some();
    if let (Some(region), true) = (region, aws_creds) {
        let mut v = json!({"kind": "bedrock", "region": region, "api_key": bearer.unwrap_or_default()});
        if let Some(base) = get("BEDROCK_BASE_URL") {
            v["base_url"] = json!(base);
        }
        out.push(("bedrock".into(), provider(v)));
    }

    // Vertex: credentials come from GOOGLE_APPLICATION_CREDENTIALS or the
    // metadata server, as with a configured Vertex provider.
    if let Some(project) = get("GOOGLE_CLOUD_PROJECT") {
        let region = get("GOOGLE_CLOUD_LOCATION").unwrap_or_else(|| "us-central1".into());
        let mut v = json!({"kind": "vertex", "project": project, "region": region});
        if let Some(base) = get("VERTEX_BASE_URL") {
            v["base_url"] = json!(base);
        }
        out.push(("vertex".into(), provider(v)));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::ProviderKind;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn keys_and_base_urls_from_the_environment() {
        let all = providers(&env(&[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_BASE_URL", "http://127.0.0.1:9/v1"),
            ("GOOGLE_GENERATIVE_AI_API_KEY", "g-key"),
            ("GROQ_API_KEY", "  "),
        ]));
        let get = |n: &str| all.iter().find(|(k, _)| k == n).map(|(_, p)| p).unwrap();
        assert_eq!(get("openai").api_key.expose(), "sk-test");
        assert_eq!(get("openai").base_url, "http://127.0.0.1:9/v1");
        assert_eq!(get("google").api_key.expose(), "g-key");
        assert_eq!(get("groq").api_key.expose(), "", "blank keys count as missing");
        assert_eq!(get("anthropic").kind, ProviderKind::Anthropic);
        assert!(all.iter().all(|(_, p)| p.source == ProviderSource::Builtin));
        assert!(!all.iter().any(|(n, _)| n == "bedrock" || n == "vertex"));
        assert_eq!(all.len(), BUILTINS.len());
    }

    #[test]
    fn unusable_base_urls_drop_the_provider() {
        let all = providers(&env(&[
            ("OPENAI_API_KEY", "sk"),
            ("OPENAI_BASE_URL", "http://litellm:4000/v1"),
            ("GROQ_BASE_URL", "https://user:pw@groq.example/v1"),
            ("MISTRAL_BASE_URL", "http://localhost:8000/v1"),
        ]));
        let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"openai"), "plain http to another host is not used");
        assert!(!names.contains(&"groq"), "credentials in the URL are not used");
        assert!(names.contains(&"mistral"), "http to localhost is fine");
        assert!(usable_url("https://api.example.com/v1"));
        assert!(!usable_url("http://localhost.evil.com/v1"));
    }

    #[test]
    fn cloud_providers_need_their_environment() {
        let only_region = providers(&env(&[("AWS_REGION", "us-east-1")]));
        assert!(!only_region.iter().any(|(n, _)| n == "bedrock"));
        let all = providers(&env(&[
            ("AWS_REGION", "eu-west-1"),
            ("AWS_BEARER_TOKEN_BEDROCK", "br-key"),
            ("GOOGLE_CLOUD_PROJECT", "my-proj"),
        ]));
        let bedrock = &all.iter().find(|(n, _)| n == "bedrock").unwrap().1;
        assert_eq!(bedrock.region.as_deref(), Some("eu-west-1"));
        assert_eq!(bedrock.api_key.expose(), "br-key");
        let vertex = &all.iter().find(|(n, _)| n == "vertex").unwrap().1;
        assert_eq!(vertex.project.as_deref(), Some("my-proj"));
        assert_eq!(vertex.region.as_deref(), Some("us-central1"));
    }
}
