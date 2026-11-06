# ai-gateway

One OpenAI-compatible API in front of OpenAI, Anthropic, Azure OpenAI, Gemini, Groq and any other OpenAI-compatible provider. Clients use the standard OpenAI SDKs and point `base_url` at the gateway. Built in Rust on tokio, axum and reqwest, and designed to run as several replicas behind a load balancer with Redis holding shared state.

## What it does

| Area | Behaviour |
|---|---|
| **API** | `POST /v1/chat/completions` (streaming and non-streaming), `POST /v1/embeddings`, `GET /v1/models` |
| **Providers** | OpenAI-compatible (OpenAI, Gemini, Groq, Together, vLLM…), Azure OpenAI, Anthropic (full translation incl. tools, images, streaming) |
| **Routing** | Named routes with ordered fallback targets or weighted A/B splits, or direct `provider/model` |
| **Reliability** | Retries with exponential backoff + jitter, per-provider timeouts, stream idle timeout, circuit breakers, fallback on 5xx / timeouts / 429 / bad upstream credentials |
| **Security** | Client keys stored as SHA-256 only (with rotation support), provider keys never leave the server or appear in logs, redirects never followed, per-key model allow-lists, authentication before the body is read, body size limit, upstream error details sanitised, tenant-scoped cache |
| **Limits** | Per-key requests/minute and daily token budgets, shared across replicas via Redis; per-key and per-instance concurrency caps with load shedding |
| **Caching** | Optional exact-match cache for non-streaming requests, per key, with `x-gateway-cache: skip` bypass |
| **Observability** | Searchable request log shared by all replicas (optional prompt/response capture), per-minute and per-hour analytics, JSON access log, Prometheus metrics on a separate port |
| **Dashboard** | Built-in web dashboard and admin API on the admin port: traffic, spend, errors and latency charts, request explorer, API key management, provider health |
| **Guardrails** | Block, redact or flag emails, card numbers, phone numbers, Aadhaar, PAN, SSNs, IBANs, IPs, secret keys, prompt-injection phrases or your own regex, in prompts and responses |
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

## Using OpenRouter

With `default_provider: openrouter` (as in `config.example.yaml`), every request goes through OpenRouter on your OpenRouter credits, and each request can pick any model it offers:

```python
client.chat.completions.create(model="anthropic/claude-haiku-4.5", messages=[...])
client.chat.completions.create(model="google/gemini-2.5-flash", messages=[...])
client.chat.completions.create(model="smart", messages=[...])   # a route with fallbacks
```

- A `model` that matches a route name uses that route's targets and fallbacks.
- A `model` starting with a configured provider name (`openrouter/...`, or `anthropic/...` if you also configured a direct `anthropic` provider) goes to that provider.
- Anything else is sent to OpenRouter unchanged.
- Give keys `"openrouter/*"` in `allowed_models` to allow every OpenRouter model, or list specific ones as `"openrouter/anthropic/claude-haiku-4.5"`.
- OpenRouter-only request fields (such as `models` for OpenRouter-side fallback, or `provider` for provider preferences) are passed through untouched.
- The actual cost OpenRouter reports for each request is used in the access log and in `gateway_cost_microdollars_total`. No price configuration is needed.

## Dashboard and admin API

The admin port (`server.admin_listen`) serves a dashboard at `/` and a JSON API under `/api`. Both stay off until you set an admin token:

```bash
./target/release/ai-gateway generate-key      # use the printed key as the admin token
```

```yaml
admin:
  token_sha256: "<key_sha256 from generate-key>"
  key_refresh_secs: 5
```

Open `http://<admin host>:9090/` and sign in with the token. Keep the admin port on a private network; the token is a second lock, not the only one.

| Page | What it shows |
|---|---|
| Overview | Requests and failures over time, tokens, spend, cache hit rate, p50/p95/p99 latency, guardrail hits, and breakdowns by model, API key, provider and status code. Ranges from 15 minutes to 30 days. |
| Requests | Every request, filterable by status, key, model and free text. Click one for its full detail, plus prompt and response when capture is on. Can refresh every 5 seconds. |
| API keys | Config and dashboard keys, their limits, and tokens used today against the budget. Create keys (shown once) and revoke them without a restart. |
| Providers & routes | Circuit-breaker state per provider, routes and weights, guardrail rules. |

The same data is available to scripts:

| Method and path | Purpose |
|---|---|
| `GET /api/overview?range=24h` | Totals, time series and breakdowns (`15m` to `35d`) |
| `GET /api/logs?status=error&tenant=acme&model=gpt&q=timeout&limit=100&offset=0` | Request log, newest first |
| `GET /api/logs/{request_id}` | One request, with prompt and response when captured |
| `GET /api/keys`, `POST /api/keys`, `DELETE /api/keys/{name}` | List, create and revoke API keys |
| `GET /api/config` | Providers, circuit state, routes and guardrails |

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  -d '{"name":"mobile-app","allowed_models":["smart"],"requests_per_minute":300,"daily_token_budget":5000000}' \
  http://127.0.0.1:9090/api/keys
```

Keys created this way are stored (hash only) in Redis and picked up by every replica within `admin.key_refresh_secs`, so a revoked key stops working everywhere within that time. With the memory store they last until the process restarts. Keys in the config file can't be changed from the API, and their names are reserved.

### Request log and analytics

```yaml
logging:
  enabled: true
  max_entries: 10000         # most recent requests kept in the searchable log
  capture_bodies: false      # store prompts and responses (they may hold personal data)
  max_body_bytes: 32768      # longer prompts and responses are truncated
  detail_ttl_secs: 604800    # full detail kept 7 days
```

Each finished request is written after the response is sent, so a slow store never delays clients. Analytics are counters in per-minute buckets (kept 2 days) and per-hour buckets (kept 35 days). Unauthenticated requests are counted in metrics but not logged, so junk traffic can't push real entries out of the log. Guardrail redactions happen before capture, so redacted text never reaches the log.

## Guardrails

```yaml
guardrails:
  rules:
    - name: no-cards
      builtin: credit_card     # Luhn-checked, so order numbers don't trip it
      action: block            # block | redact | flag
      apply_to: request        # request | response | both (default)
    - name: emails
      builtin: email
      action: redact           # replaced with [REDACTED:emails]
    - name: secrets
      builtin: api_key
      action: redact
    - name: injection
      builtin: prompt_injection
      action: flag             # passes through, recorded in the log
    - name: project-codename
      pattern: "(?i)project\\s+falcon"
      action: block
```

Built-in detectors: `email`, `credit_card`, `phone_in`, `phone_intl`, `aadhaar`, `pan`, `us_ssn`, `iban`, `ipv4`, `api_key` (OpenAI, Anthropic, AWS, GitHub, Google, Slack and gateway keys) and `prompt_injection` (a phrase heuristic; use it with `flag`). Rules check message text, text parts and embeddings `input`. A blocked request gets `400` with code `content_blocked` and never reaches a provider. Response rules run on non-streaming chat responses; streamed responses are not checked. Matches are counted in `gateway_guardrail_hits_total{rule,stage}`.

## Weighted routes

```yaml
routes:
  - name: smart
    strategy: weighted         # default is fallback
    targets:
      - provider: openrouter
        model: anthropic/claude-sonnet-4.5
        weight: 90
      - provider: openrouter
        model: openai/gpt-4o
        weight: 10
      - provider: openai
        model: gpt-4o-mini
        weight: 0              # never picked first, still a fallback
```

Each request picks its first target by weight; the other targets stay fallbacks in the order listed. Use it for A/B tests, gradual model rollouts or spreading load across accounts. The dashboard shows each model's traffic, errors and spend side by side.

## Embeddings

`POST /v1/embeddings` takes the standard OpenAI body and goes through the same keys, permissions, limits, budgets, cache, retries and fallbacks as chat. It works with OpenAI-compatible and Azure providers; Anthropic targets are skipped because Anthropic has no embeddings API.

## How a request flows

1. **Authenticate** — `Authorization: Bearer <key>` or `x-api-key`. The key is hashed and looked up.
2. **Validate** — JSON object, `model`, non-empty `messages`, body under `max_body_bytes`.
3. **Resolve** — `model` is a route name, `provider/model`, or (with `default_provider` set) any other model name, passed to the default provider; the key's `allowed_models` must permit it.
4. **Limit** — requests/minute (fixed one-minute window) and daily token budget (UTC day), in Redis.
5. **Guardrails** — request rules block, redact or flag; redaction happens before caching, logging and the upstream call.
6. **Cache** — non-streaming requests are looked up by a hash of key name + full request body.
7. **Call upstream** — for each target in order: retry up to `max_retries` with backoff; skip a provider whose circuit is open.
   - 400/404/413/422 → returned to the client as-is (another provider would reject it too)
   - 401/403 → logged as a gateway config problem, next target tried, client sees a generic 502
   - 429, 5xx, timeouts, connection errors → retried, then next target
8. **Respond** — Response guardrails run on non-streaming chat responses. Anthropic responses and streams are translated to OpenAI format. Token usage is recorded when the response or stream ends. If a stream ends before the provider reports usage (the client disconnected, or the stream broke), the gateway bills an estimate: the provider's numbers where it has them, otherwise about four characters per token of what was sent and received.

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

`gateway_requests_total`, `gateway_request_duration_seconds`, `gateway_tokens_total`, `gateway_cost_microdollars_total`, `gateway_upstream_errors_total`, `gateway_upstream_skipped_total`, `gateway_circuit_open`, `gateway_rejected_total`, `gateway_cache_total`, `gateway_store_errors_total`, `gateway_guardrail_hits_total`.

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
- Implemented: `/v1/chat/completions`, `/v1/embeddings` and `/v1/models`. Images, audio, the Responses API and Anthropic's Messages API as an inbound format are not.
- Response guardrails don't check streamed responses.
- The response cache is exact-match only; there is no semantic (similar-prompt) cache.
- Log search scans the most recent `logging.max_entries` requests. For long-term retention, ship the JSON access log to your log platform.
- API keys can be created and revoked live from the dashboard. Other config changes (providers, routes, guardrails) need a rolling restart.
