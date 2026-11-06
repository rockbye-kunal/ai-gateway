//! AI gateway: a single OpenAI-compatible endpoint that routes requests to
//! multiple LLM providers with authentication, rate limiting, token budgets,
//! caching, retries, fallbacks, circuit breaking and metrics.

pub mod admin;
pub mod app;
pub mod auth;
pub mod breaker;
pub mod config;
pub mod error;
pub mod guardrails;
pub mod logs;
pub mod metrics;
pub mod providers;
pub mod proxy;
pub mod sse;
pub mod store;
pub mod usage;

/// rustls 0.23 needs a process-wide crypto provider when more than one is
/// compiled in (reqwest and redis pull in different ones). Installing ring
/// explicitly avoids a panic the first time a TLS connection is made.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
