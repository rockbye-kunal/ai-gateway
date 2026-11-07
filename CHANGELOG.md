# Changelog

All notable changes to AI Gateway are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

The first public version. Everything below is new.

### APIs
- One OpenAI-compatible API: Chat Completions (with streaming), Responses, Embeddings, Images, Moderations, Rerank, Audio speech and Realtime over WebSockets.
- Anthropic Messages API, and `/native/<provider>/<path>` passthrough to any provider endpoint.
- `/v1/models`, plus per-key usage reports, generation lookup and feedback (`/v1/report`, `/v1/generation`, `/v1/feedback`).

### Providers
- Any OpenAI-compatible provider (OpenAI, OpenRouter, Gemini, Groq, Together, Mistral, vLLM, Ollama…), Azure OpenAI and Anthropic, with full OpenAI ⇄ Anthropic translation including tools, images and streaming.
- AWS Bedrock through the Converse API, with SigV4 signing, the full AWS credential chain, Bedrock API keys and AWS event-stream decoding.
- Google Vertex AI: Gemini and partner models through the OpenAI-compatible endpoint, Claude in Anthropic's format, with service-account, gcloud and metadata-server credentials.

### Routing and reliability
- Named routes with fallback, weighted (with session affinity), latency and cost strategies, and conditional branches on key, user, tags or metadata.
- Per-request fallback models and provider `order`, `only` and `sort`; model rewrite and deny rules editable live.
- Retries with configurable backoff, timeouts, stream idle timeout and per-provider circuit breakers.

### Limits, cost and caching
- Per-key rate limits, token budgets and concurrency limits; request, token and dollar limits by key, user, model, provider or metadata over any window; signed webhook alerts.
- Automatic model pricing from a public price list, with prompt-cache rates and config overrides.
- Exact-match response cache for normal and streaming responses, with per-request controls; automatic Anthropic prompt caching.

### Security and data handling
- Hashed client keys managed from the config or the dashboard; bring-your-own provider keys per request or stored encrypted (AES-256-GCM).
- Zero data retention and no-training provider filters.
- Guardrails: built-in detectors, custom patterns and moderation models, with block, redact and flag actions.

### Observability and operations
- Request log with every provider attempt, tokens, cost, latency, time to first token, metadata and feedback; usage reports; NDJSON log export; OpenTelemetry traces; Prometheus metrics.
- Web dashboard and admin API for overview, requests, reports, keys, rules, limits, providers, prices and the audit log.
- Hot config reload, health and readiness checks, graceful shutdown, Docker image and a docker-compose setup with nginx and Redis.

[Unreleased]: https://github.com/rockbye-kunal/ai-gateway/commits/main
