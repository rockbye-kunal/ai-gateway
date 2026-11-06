# ai-gateway

One OpenAI-compatible API in front of OpenAI, Anthropic, Azure OpenAI, Gemini, Groq and any other OpenAI-compatible provider. Clients use the standard OpenAI SDKs and point `base_url` at the gateway. Built in Rust on tokio, axum and reqwest, and designed to run as several replicas behind a load balancer with Redis holding shared state.

## What it does

| Area | Behaviour |
|---|---|
| **API** | `POST /v1/chat/completions` (streaming and non-streaming), `GET /v1/models` |
| **Providers** | OpenAI-compatible (OpenAI, Gemini, Groq, Together, vLLM…), Azure OpenAI, Anthropic (full translation incl. tools, images, streaming) |
| **Routing** | Named routes with ordered fallback targets, or direct `provider/model` |
| **Reliability** | Retries with exponential backoff + jitter, per-provider timeouts, stream idle timeout, circuit breakers, fallback on 5xx / timeouts / 429 / bad upstream credentials |
| **Security** | Client keys stored as SHA-256 only (with rotation support), provider keys never leave the server or appear in logs, redirects never followed, per-key model allow-lists, authentication before the body is read, body size limit, upstream error details sanitised, tenant-scoped cache |
| **Limits** | Per-key requests/minute and daily token budgets, shared across replicas via Redis; per-key and per-instance concurrency caps with load shedding |
| **Caching** | Optional exact-match cache for non-streaming requests, per key, with `x-gateway-cache: skip` bypass |
| **Observability** | JSON access log per request (tenant, provider, model, tokens, cost, latency, attempts, request id), Prometheus metrics on a separate port |
| **Operations** | `/healthz`, `/readyz` (fails only while draining), graceful shutdown that lets streams finish and flushes usage writes |

## Quick start

```bash
cargo build --release

# 1. Create a client key. Give the key to the client; put only the hash in config.
./target/release/ai-gateway generate-key

# 2. Configure
cp config.example.yaml config.yaml          # edit providers, routes, keys
export OPENAI_API_KEY=... ANTHROPIC_API_KEY=... REDIS_URL=rediss://...
./target/release/ai-gateway check-config config.yaml

# 3. Run
./target/release/ai-gateway serve config.yaml
```

Call it with any OpenAI client:

```python
from openai import OpenAI
client = OpenAI(base_url="https://gateway.example.com/v1", api_key="gw_...")
client.chat.completions.create(model="smart", messages=[{"role": "user", "content": "Hello"}])
```

Response headers tell you what happened: `x-request-id`, `x-gateway-provider`, `x-gateway-model`, and `x-gateway-cache: HIT|MISS` when caching is on.

## How a request flows

1. **Authenticate** — `Authorization: Bearer <key>` or `x-api-key`. The key is hashed and looked up.
2. **Validate** — JSON object, `model`, non-empty `messages`, body under `max_body_bytes`.
3. **Resolve** — `model` is a route name or `provider/model`; the key's `allowed_models` must permit it.
4. **Limit** — requests/minute (fixed one-minute window) and daily token budget (UTC day), in Redis.
5. **Cache** — non-streaming requests are looked up by a hash of key name + full request body.
6. **Call upstream** — for each target in order: retry up to `max_retries` with backoff; skip a provider whose circuit is open.
   - 400/404/413/422 → returned to the client as-is (another provider would reject it too)
   - 401/403 → logged as a gateway config problem, next target tried, client sees a generic 502
   - 429, 5xx, timeouts, connection errors → retried, then next target
7. **Respond** — Anthropic responses and streams are translated to OpenAI format. Token usage is recorded when the response or stream ends. If a stream ends before the provider reports usage (the client disconnected, or the stream broke), the gateway bills an estimate: the provider's numbers where it has them, otherwise about four characters per token of what was sent and received.

The whole request, including retries and fallbacks, must produce response headers within `server.request_timeout_secs`, or the client gets a 504 in the OpenAI error format.

## Running behind a load balancer

- Use `store.backend: redis`. Rate limits, budgets and the cache are then shared by every replica. Use `rediss://` for TLS to managed Redis.
- Terminate TLS at the load balancer. The gateway listens on plain HTTP inside your network.
- Point the load balancer health check at `GET /readyz` on the API port. It does not check Redis on purpose: if it did, a Redis outage would mark every replica unhealthy at once. Watch `gateway_store_errors_total` for Redis health instead.
- Redis must be reachable when an instance starts. After that, a Redis outage is handled per request by `store.on_error`.
- On SIGTERM the gateway fails `/readyz` for `shutdown_drain_secs`, then stops accepting connections, then gives in-flight requests up to `shutdown_grace_secs`. Set your orchestrator's termination grace period above the sum (Kubernetes `terminationGracePeriodSeconds`).
- Disable response buffering and set a long read timeout on the load balancer for SSE (see `deploy/nginx.conf`).
- Keep `admin_listen` (metrics) on a private network.
- Circuit breakers are per replica on purpose: no coordination needed, and each instance reacts to what it sees.

`docker compose up --build` runs two replicas, Redis and nginx locally in that shape.

## Choosing fail-open or fail-closed

`store.on_error` decides what happens when Redis is unreachable:

- `closed` (default): requests from keys with a rate limit or budget get 503. Keys with no limits keep working. Protects spend.
- `open`: requests go through without rate limits or budgets. Protects availability.

The cache always fails open.

## Configuration reference

See `config.example.yaml` for every option with comments. Strings may use `${ENV_VAR}` or `${ENV_VAR:-default}`; substitution happens after parsing, so secrets can't break the YAML.

Key provider settings: `timeout_secs` (headers, and the whole body when not streaming), `stream_idle_timeout_secs` (max gap between stream chunks), `max_retries`, `breaker_failure_threshold`, `breaker_open_secs`, `extra_headers`. Plain `http://` base URLs are rejected unless the host is loopback or `allow_insecure_http: true` is set.

## Rotating a client key

Generate a new key, set its hash as `key_sha256`, and move the old hash into `previous_key_sha256`. Roll the config out, let the client switch, then remove the old hash. Both keys share the tenant's limits, budget and metrics during the overlap.

## Metrics

On `admin_listen` at `/metrics`:

`gateway_requests_total`, `gateway_request_duration_seconds`, `gateway_tokens_total`, `gateway_cost_microdollars_total`, `gateway_upstream_errors_total`, `gateway_upstream_skipped_total`, `gateway_circuit_open`, `gateway_rejected_total`, `gateway_cache_total`, `gateway_store_errors_total`.

Good first alerts: `gateway_circuit_open == 1`, a rising `gateway_store_errors_total`, `gateway_rejected_total{reason="overloaded"}`, and p95 of `gateway_request_duration_seconds`.

## Testing

```bash
cargo test
```

Commit `Cargo.lock` after the first build, so CI and Docker builds use exactly the dependency versions you tested.

Unit tests cover config validation, auth, the circuit breaker, SSE parsing and Anthropic translation. `tests/e2e.rs` starts a real gateway in front of mock OpenAI and Anthropic servers and checks auth, validation, permissions, retries, fallback, circuit breaking, credential fallback, client-error passthrough, OpenAI and Anthropic streaming, tool calls, caching, rate limits and token budgets.

## Known limits

- Token budgets are checked before a request and charged after it, so concurrent requests can overshoot a budget by the size of the requests in flight. `max_concurrent_requests` on the key bounds this.
- When a timeout fires after the provider has already started work, that attempt is retried. The abandoned attempt may still cost provider tokens that the gateway never sees, so it is not charged to the tenant.
- Rate-limit and budget windows use each instance's clock. Keep replicas on NTP.
- Anthropic targets ignore `response_format`. Ask for JSON in the prompt, or route JSON-mode traffic to OpenAI-compatible providers.
- The rate limit is a fixed one-minute window, so a key can burst up to twice its limit across a window boundary.
- Only `/v1/chat/completions` and `/v1/models` are implemented. Embeddings, images, audio and the Responses API are not.
- Config changes need a restart (do a rolling restart across replicas).
