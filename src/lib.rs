//! AI gateway: OpenAI-, Anthropic- and provider-native APIs in front of many
//! LLM providers, with authentication, routing, fallbacks, limits, budgets,
//! guardrails, caching, request logs, analytics and an admin dashboard.

pub mod admin;
pub mod alerts;
pub mod app;
pub mod auth;
pub mod aws;
pub mod breaker;
pub mod config;
pub mod error;
pub mod export;
pub mod gcp;
pub mod guardrails;
pub mod limits;
pub mod logs;
pub mod metrics;
pub mod native;
pub mod options;
pub mod pricing;
pub mod providers;
pub mod proxy;
pub mod realtime;
pub mod rules;
pub mod secrets;
pub mod sse;
pub mod stats;
pub mod store;
pub mod usage;

/// rustls 0.23 needs a process-wide crypto provider when more than one is
/// compiled in (reqwest and redis pull in different ones). Installing ring
/// explicitly avoids a panic the first time a TLS connection is made.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
