# ai-gateway

A self-hosted AI gateway in Rust: one place in front of OpenAI, Anthropic, Azure OpenAI, AWS Bedrock, Google Vertex AI, Gemini, Groq, OpenRouter and any OpenAI-compatible provider, with the controls Cloudflare AI Gateway and Vercel AI Gateway offer. Clients keep using the OpenAI or Anthropic SDKs and point their base URL at the gateway. It runs as several replicas behind a load balancer with Redis holding shared state, and ships with a web dashboard.

## What it does

| Area | Behaviour |
|---|---|
| **APIs** | OpenAI Chat Completions (streaming), Responses, Embeddings, Images, Moderations, Rerank, Audio speech, Realtime (WebSockets); Anthropic Messages (native); `/native/<provider>/<any path>` passthrough for every other provider endpoint; `/v1/models` |
| **Providers** | OpenAI-compatible (OpenAI, Gemini, Groq, Together, Mistral, vLLM, Ollama, OpenRouter…), Azure OpenAI, Anthropic, AWS Bedrock (Converse API, SigV4 signing, every AWS credential source) and Google Vertex AI (Gemini and partner models, Claude in the Anthropic format, service account or metadata-server auth); Anthropic and Bedrock are translated to the OpenAI format, including tools, images and streaming |
| **Routing** | Named routes with fallbacks, weighted A/B splits with session affinity, fastest-first and cheapest-first strategies, conditional branches on key, user, tags or metadata, request-level fallback models, provider `order`/`only`/`sort`, model rewrite and deny rules editable live |
| **Reliability** | Retries with configurable backoff, per-provider and per-request timeouts, stream idle timeout, circuit breakers with alerts, fallback on 5xx / timeouts / 429 / bad credentials |
| **Pricing** | Costs worked out automatically from a public price list (LiteLLM's, refreshed daily, or a local file), including cache read and write rates; per-target prices and overrides win |
| **Limits and spend** | Per-key requests/minute and token budgets; request, token and dollar limits per minute/hour/day/week/month, split by key, user, model, provider or any metadata field; threshold alerts by signed webhook |
| **Caching** | Exact-match cache for normal and streaming responses, per key, with per-request TTL, skip and custom cache key; automatic Anthropic prompt caching |
| **Security** | Hashed client keys, keys and routing rules managed live, BYOK (request-scoped or stored encrypted), zero data retention and no-training provider filters, guardrails and DLP (regex detectors and moderation models), audit log |
| **Observability** | Request log with prompts/responses (optional), every upstream attempt, TTFT, tokens/sec, cached and reasoning tokens, user/tags/metadata, feedback; analytics; usage reports by key, user, tag, model, provider; NDJSON log export; OpenTelemetry traces; Prometheus metrics |
| **Operations** | Dashboard, admin API, hot config reload (SIGHUP or API), `/healthz`, `/readyz`, graceful shutdown that lets streams finish and flushes usage writes |

## Quick start

```bash
cargo build --release

# 1. Keys. Give api_key to the client; put only key_sha256 in the config.
./target/release/ai-gateway generate-key           # a client key
./target/release/ai-gateway generate-key           # the admin token for the dashboard
./target/release/ai-gateway generate-encryption-key  # optional: lets the dashboard store provider keys

# 2. Configure
cp config.example.yaml config.yaml
export OPENROUTER_API_KEY=... REDIS_URL=rediss://... ADMIN_TOKEN_SHA256=...
./target/release/ai-gateway check-config config.yaml

# 3. Run
./target/release/ai-gateway serve config.yaml
```

```python
from openai import OpenAI
client = OpenAI(base_url="https://gateway.example.com/v1", api_key="gw_...")
client.chat.completions.create(model="smart", messages=[{"role": "user", "content": "Hello"}])
```

```python
from anthropic import Anthropic
client = Anthropic(base_url="https://gateway.example.com", api_key="gw_...")
client.messages.create(model="anthropic/claude-sonnet-4.5", max_tokens=512, messages=[{"role": "user", "content": "Hello"}])
```

Response headers: `x-request-id` (also `x-gateway-generation-id`), `x-gateway-provider`, `x-gateway-model`, `x-gateway-cache: HIT|MISS` (also `cf-aig-cache-status`), and `x-gateway-guardrails` when a rule matched.

Open the dashboard at `http://<admin host>:9090/` and sign in with the admin token.

## AWS Bedrock and Google Vertex AI

```yaml
providers:
  bedrock:
    kind: bedrock
    region: us-east-1            # or AWS_REGION
    # Optional. Without keys the standard AWS chain is used: AWS_ACCESS_KEY_ID/…,
    # EKS web identity (IRSA), ECS task role, then the EC2 instance role (IMDSv2).
    # aws_access_key_id: ${AWS_ACCESS_KEY_ID}
    # aws_secret_access_key: ${AWS_SECRET_ACCESS_KEY}
    # Or a Bedrock API key, sent as a bearer token:
    # api_key: ${AWS_BEARER_TOKEN_BEDROCK}
  vertex:
    kind: vertex
    project: my-gcp-project
    region: us-central1          # or global
    # Optional. Without these, GOOGLE_APPLICATION_CREDENTIALS and then the
    # metadata server (GKE, Cloud Run, GCE) are used.
    # credentials_file: /secrets/vertex-sa.json
```

- **Bedrock** chat goes through the Converse API (`converse` and `converse-stream`), so any Bedrock chat model works: `bedrock/us.anthropic.claude-sonnet-4-5-20250929-v1:0`, `bedrock/amazon.nova-pro-v1:0`, inference profile ARNs. Tools, images (base64), system prompts, stop sequences and prompt caching (`auto_prompt_cache`) are translated; `additionalModelRequestFields` and `guardrailConfig` pass through. Other Bedrock APIs (InvokeModel, embeddings) go through `/native/bedrock/...`, signed for you.
- **Vertex**: Gemini and other publisher models use Vertex's OpenAI-compatible endpoint (`vertex/gemini-2.5-flash` is sent as `google/gemini-2.5-flash`; give other publishers in full, like `vertex/meta/llama-...`). Models starting with `claude-` use Anthropic's format on Vertex (`rawPredict`/`streamRawPredict`), for both `/v1/chat/completions` and `/v1/messages`. Everything else is reachable at `/native/vertex/projects/...`.
- Credentials are cached until shortly before they expire.

## Model prices

Every request gets a cost, used by spend limits, reports and the `cost` routing strategy. The price is the first of:

1. `price_input_per_mtok` / `price_output_per_mtok` on the route target;
2. `pricing.overrides`, keyed `provider/model` or `model`;
3. the price list, matched by provider type, host and model (dated, regional and versioned names fall back to their base model).

```yaml
pricing:
  enabled: true                 # default
  refresh_hours: 24
  # url: <LiteLLM-format JSON>  # default: LiteLLM's public list
  # file: prices.json           # for installs without internet access
  overrides:
    openai/my-fine-tune: {input_per_mtok: 3.0, output_per_mtok: 12.0}
```

Cache reads and writes are priced at their own rates when the list has them. Replicas share the list through Redis, so only one needs to download it. Providers that report their own cost (OpenRouter) keep that cost. The Providers & routes page shows the price used for every route target; `GET /api/pricing` and `POST /api/pricing/refresh` do the same over the API. For an OpenAI-compatible provider the list doesn't recognise by its host, set `pricing_prefix` (for example `openrouter/`).

## Choosing a model

`model` can be:

- a **route name** from the config (`smart`), which brings its fallbacks and strategy;
- `provider/model` for a configured provider (`openai/gpt-4o`, `anthropic/claude-sonnet-4.5`);
- anything else, sent unchanged to `default_provider` (for example any OpenRouter model).

Keys list what they may use in `allowed_models`: route names, `provider/model`, `provider/*`, or `*`.

## APIs

| Endpoint | Served by |
|---|---|
| `POST /v1/chat/completions` | Every provider (Anthropic is translated) |
| `POST /v1/responses` | OpenAI-compatible providers, streams passed through event by event |
| `POST /v1/messages` | Anthropic providers and Claude on Vertex, native format, `anthropic-version`/`anthropic-beta` forwarded |
| `POST /v1/embeddings`, `/v1/images/generations` | OpenAI-compatible and Azure |
| `POST /v1/moderations`, `/v1/rerank`, `/v1/audio/speech` | OpenAI-compatible |
| `GET /v1/realtime?model=…` | WebSocket relay to OpenAI-compatible realtime APIs; browsers can authenticate with the `openai-insecure-api-key.<key>` subprotocol |
| `ANY /native/<provider>/<path>` | Any endpoint of a configured provider, in its own format; the gateway adds the provider key |
| `GET /v1/models` | Routes this key may use, with prices |
| `GET /v1/report`, `GET /v1/generation?id=`, `POST /v1/feedback` | The calling key's own usage report, request lookup and feedback |

All of them get authentication, permissions, limits, guardrails, logging and usage tracking. Native passthrough is not cached and only `deny` rules (and same-provider rewrites) apply to it, because its bodies are in each provider's own format.

## Per-request options

Send options as headers (`x-gateway-<name>` or Cloudflare's `cf-aig-<name>`) or in the body under `providerOptions.gateway` (Vercel's shape). `providerOptions` is removed before the request reaches a provider.

| Header | Meaning |
|---|---|
| `metadata` | JSON object of strings, numbers or booleans (up to 20 keys), stored in the log and usable in routing and limits |
| `cache-ttl` / `skip-cache` / `cache-key` | Cache this response for N seconds / bypass the cache / use your own cache key |
| `collect-log` | `false` keeps the request out of the request log (analytics still count it) |
| `request-timeout` | Milliseconds for the whole request (capped at the server limit) |
| `max-attempts`, `retry-delay`, `backoff` | Attempts per target (1-10), first retry delay in ms, `constant`/`linear`/`exponential` |
| `custom-cost` | `{"per_token_in": .., "per_token_out": ..}` or `{"total_cost": ..}` in USD |
| `event-id` | Your own id, stored and searchable |

Also `x-session-id` (session affinity), `ai-reporting-user`, `ai-reporting-tags` (comma separated), and W3C `traceparent`. The OpenAI `user` field is the reporting user unless a header sets one.

```json
"providerOptions": {"gateway": {
  "order": ["anthropic", "openai"],
  "only": ["anthropic", "openai"],
  "models": ["openai/gpt-4o", "google/gemini-2.5-flash"],
  "sort": "ttft",
  "user": "user-123",
  "tags": ["feature:chat", "env:prod"],
  "metadata": {"plan": "free"},
  "sessionId": "conversation-42",
  "caching": "auto",
  "zeroDataRetention": true,
  "disallowPromptTraining": true,
  "byok": {"openai": [{"apiKey": "sk-..."}]}
}}
```

`models` are fallbacks tried after the requested model. `order` puts those providers first; `only` allows nothing else. `sort` is `cost`, `ttft` or `tps` (from recent measurements on this replica). `caching: auto` adds Anthropic prompt-caching markers.

## Routing

```yaml
routes:
  - name: smart
    strategy: weighted          # fallback (default) | weighted | latency | cost
    targets:
      - provider: anthropic
        model: claude-sonnet-4.5
        weight: 90
        price_input_per_mtok: 3
        price_output_per_mtok: 15
      - provider: openai
        model: gpt-4o
        weight: 10
    branches:                   # first match wins; otherwise the targets above
      - when:
          metadata.plan: free   # or {in: [...]}, {ne: ..}, {not_in: [...]}, {exists: true}
        strategy: cost
        targets:
          - provider: openai
            model: gpt-4o-mini
```

- `weighted` picks the first target by weight; a request with a session id always lands on the same one. Weight 0 makes a target fallback-only.
- `latency` puts the fastest target first by recent time to first token; `cost` the cheapest by configured price. Untried targets are tried first so they get measured, and 5% of requests try the runner-up to keep measurements fresh.
- Branch conditions can use `key`, `user`, `tags` and `metadata.<name>`.

Routing rules rewrite or deny models before routing, and apply to fallbacks too. Put them in the config or manage them live from the dashboard (`/api/rules`):

```yaml
routing_rules:
  - source: openai/gpt-3.5-turbo   # a model, provider/* or *
    action: rewrite
    destination: openai/gpt-4o-mini
  - source: openai/o1-preview
    action: deny
    reason: retired model           # shown to clients in the 403
```

## Limits, budgets and alerts

```yaml
limits:
  - name: per-user-requests
    kind: requests                # requests | tokens | cost (USD)
    limit: 60
    window: minute                # minute | hour | day | week | month (UTC)
    per: user                     # global | key | user | model | provider | metadata.<name>
    sliding: true                 # requests only: smooth the window edge
  - name: monthly-spend-per-key
    kind: cost
    limit: 500
    window: month
    per: key
    keys: [acme-prod]             # optional filter
    models: ["openai/*"]          # optional filter
alerts:
  webhook_url: https://hooks.example.com/ai-gateway
  webhook_secret: ${ALERT_SECRET}   # HMAC-SHA256 of the body in x-gateway-signature
  thresholds: [0.8, 1.0]
```

Over a limit, requests get `429` with code `limit_exceeded` and `Retry-After` until the window resets. Token and cost limits are charged when a request finishes, so concurrent requests can overshoot slightly (the same soft cap as Cloudflare and Vercel). A `per: provider` limit skips that provider and falls back instead of failing. Alerts fire once per limit, window and threshold across all replicas; the webhook also receives `provider.circuit_open` events. Per-key `requests_per_minute` and `daily_token_budget` still work as before.

## Keys, provider keys (BYOK) and data handling

- **Client keys** come from the config file or the dashboard. Dashboard keys are stored as hashes in Redis and reach every replica within `admin.key_refresh_secs`.
- **Provider keys** come from the config file, from the dashboard (encrypted with AES-256-GCM using `admin.encryption_key`, stored in Redis), or per request in `providerOptions.gateway.byok`. A wrong client-supplied key returns `401 provider_key_rejected`; a wrong gateway key falls back to the next target. Set `allow_client_keys: false` on a provider to refuse request keys. The log records which kind was used (`config`, `stored`, `request`).
- **Zero data retention**: mark providers with `zero_data_retention: true` (and `no_prompt_training: true`). A key with `zero_data_retention: true`, or a request with `zeroDataRetention`, only goes to those providers, is never cached, and its prompts and responses are never logged or exported. `disallowPromptTraining` filters on `no_prompt_training`.

## Guardrails

```yaml
guardrails:
  rules:
    - name: no-cards
      builtin: credit_card     # Luhn-checked
      action: block            # block | redact | flag
      apply_to: request        # request | response | both (default)
    - name: emails
      builtin: email
      action: redact           # replaced with [REDACTED:emails]
    - name: injection
      builtin: prompt_injection
      action: flag
    - name: violence
      moderation:              # a moderation model, like Llama Guard categories
        provider: openai       # an openai-kind provider with /moderations
        model: omni-moderation-latest
        categories: [violence, hate, self-harm]   # empty = any flagged category
        threshold: 0.8         # optional; default is the model's own decision
        fail_open: true
      action: block
    - name: codename
      pattern: "(?i)project\\s+falcon"
      action: block
```

Built-in detectors: `email`, `credit_card`, `phone_in`, `phone_intl`, `aadhaar`, `pan`, `us_ssn`, `iban`, `ipv4`, `api_key`, `prompt_injection`. Rules see chat and Messages turns and system prompts, Responses/embeddings/moderation `input`, image `prompt` and rerank `query`/`documents`. Blocked requests get `400 content_blocked` and never reach a provider. Response rules run on non-streaming responses. Matches appear in `x-gateway-guardrails`, the log and `gateway_guardrail_hits_total`.

## Caching

```yaml
cache:
  enabled: true
  ttl_secs: 300
```

Identical requests from the same key, routed to the same targets, are served from the cache, including streams (replayed as the same SSE). Use the `cache-ttl`, `skip-cache` and `cache-key` headers per request. Images and speech are not cached. Anthropic prompt caching is separate: set `auto_prompt_cache: true` on the provider or send `caching: auto`, and cached input tokens show up in the log and reports.

## Dashboard and admin API

The admin port serves the dashboard at `/` and a JSON API under `/api`, both protected by the admin token (`admin.token_sha256`). Pages: Overview (traffic, failures, tokens, spend, latency and time to first token, breakdowns by model with feedback, user, tag, API, key, provider including failed attempts), Requests (search, filters, full detail with every provider attempt, prompts and responses, feedback buttons, delete), Usage reports (by day, model, provider, key, user, tag, credential, ZDR; CSV download), API keys, Routing rules, Limits, Providers & routes (health, live latency and throughput per model, stored provider keys, reload), Audit log.

| Admin API | Purpose |
|---|---|
| `GET /api/overview?range=24h` | Totals, time series, breakdowns |
| `GET /api/logs?status=&tenant=&model=&user=&tag=&endpoint=&metadata=k%3Dv&q=` / `DELETE` with the same filters | Search or delete logged requests |
| `GET /api/logs/{id}`, `POST /api/logs/{id}/feedback` | One request; rate it (`rating` 1/0/-1, `score` 0-100, `comment`) |
| `GET /api/report?start_date=&end_date=&group_by=` | Usage report (same fields as Vercel's reporting API) |
| `GET/POST /api/keys`, `DELETE /api/keys/{name}` | Client keys |
| `GET/POST /api/rules`, `PATCH/DELETE /api/rules/{id}` | Routing rules |
| `GET /api/provider-keys`, `PUT/DELETE /api/provider-keys/{provider}` | Stored provider keys |
| `GET /api/limits`, `GET /api/audit`, `GET /api/config`, `POST /api/reload` | Limit usage, audit log, configuration and health, reload |
| `GET /api/pricing`, `POST /api/pricing/refresh` | Price list status and the price of every route target; download it again now |

## Logs, export and tracing

```yaml
logging:
  enabled: true
  max_entries: 10000          # searchable log size
  capture_bodies: false       # prompts and responses
  detail_ttl_secs: 604800
  report_retention_days: 90
  export:                     # NDJSON batches of log entries ("logpush")
    url: https://logs.example.com/ingest
    headers: {Authorization: "Bearer ${LOG_TOKEN}"}
    include_bodies: false
  otlp:                       # one trace per request, a span per provider attempt
    endpoint: http://otel-collector:4318
    service_name: ai-gateway
```

Each request records the key, user, tags, metadata, model and provider, status, every upstream attempt, prompt/completion/cached/reasoning tokens, cost, latency, time to first token, tokens per second, guardrail matches and feedback. Exports and traces go through bounded queues and are dropped (and counted) rather than slowing requests down. OpenTelemetry spans use the `gen_ai.*` conventions and continue the client's `traceparent`.

## Reloading configuration

Send `SIGHUP` or call `POST /api/reload` to re-read the config file. Providers, routes, guardrails, limits, routing rules, config keys, cache and logging options change immediately; circuit-breaker state and per-key concurrency carry over when a provider or key is unchanged. Listen addresses, the store, the admin token and log export endpoints need a restart. A config that fails validation is rejected and the running one stays.

## How a request flows

1. **Authenticate** the key and claim its concurrency slot, before reading the body.
2. **Options** from headers and `providerOptions.gateway`, which is stripped.
3. **Validate** the body for the API.
4. **Routing rules**, then **targets**: route branches and strategy, fallback models, `only`, `order`, `sort`, ZDR and training filters, the API's supported providers.
5. **Limits**: key rate and budget, then configured limits.
6. **Guardrails**: regex rules (block, redact, flag), then moderation models.
7. **Cache** lookup.
8. **Upstream**: for each target, per-provider limits, circuit breaker, credentials (request, stored or config key), retries with backoff, fallback on failure.
9. **Respond**: translate or pass through, response guardrails, cache, then (after the response) log, usage, limits, alerts and export.

## Running behind a load balancer

- Use `store.backend: redis`. Limits, budgets, cache, logs, keys, rules and stored provider keys are then shared by every replica. Use `rediss://` for TLS to managed Redis.
- Terminate TLS at the load balancer. The gateway listens on plain HTTP inside your network.
- Point the health check at `GET /readyz` on the API port. It does not check Redis on purpose: a Redis outage would otherwise mark every replica unhealthy at once. Watch `gateway_store_errors_total` instead.
- On SIGTERM the gateway fails `/readyz` for `shutdown_drain_secs`, stops accepting connections, gives in-flight requests up to `shutdown_grace_secs`, then flushes usage writes and log exports.
- Disable response buffering and set a long read timeout on the load balancer for SSE and WebSockets (see `deploy/nginx.conf`).
- Keep `admin_listen` on a private network.
- Circuit breakers and latency measurements are per replica on purpose.

`docker compose up --build` runs two replicas, Redis and nginx locally in that shape.

## Choosing fail-open or fail-closed

`store.on_error` decides what happens when Redis is unreachable: `closed` (default) rejects requests that need a limit or budget check with 503; `open` lets them through without limits. The cache always fails open.

## Rotating a client key

Generate a new key, set its hash as `key_sha256`, and move the old hash into `previous_key_sha256`. Reload, let the client switch, then remove the old hash. Both keys share the tenant's limits, budget and metrics during the overlap. Dashboard keys are rotated by creating a new key and revoking the old one.

## Metrics

On `admin_listen` at `/metrics`: `gateway_requests_total`, `gateway_request_duration_seconds`, `gateway_time_to_first_token_seconds`, `gateway_tokens_total`, `gateway_cost_microdollars_total`, `gateway_upstream_errors_total`, `gateway_upstream_skipped_total`, `gateway_circuit_open`, `gateway_rejected_total{reason}`, `gateway_cache_total`, `gateway_store_errors_total`, `gateway_guardrail_hits_total`, `gateway_guardrail_errors_total`, `gateway_alerts_total`, `gateway_export_batches_total`, `gateway_export_dropped_total`, `gateway_config_reloads_total`.

## Testing

```bash
cargo test                                                        # in-memory store
AIGW_TEST_REDIS_URL=redis://127.0.0.1:6379/0 cargo test           # against real Redis
```

`tests/e2e.rs`, `tests/parity.rs` and `tests/cloud.rs` run a real gateway in front of mock OpenAI, Anthropic, Bedrock (checking every SigV4 signature and decoding AWS event streams), Vertex (with a Google token endpoint), moderation, image, speech, rerank and realtime WebSocket servers, plus webhook, log-export and OTLP collectors and a price list, covering every feature above.

## How it compares

Matches Cloudflare AI Gateway and Vercel AI Gateway on: unified OpenAI API, Responses and Anthropic Messages APIs, provider-native endpoints, realtime WebSockets, caching with per-request controls, rate and spend limits with alerts, dynamic and conditional routing, fallbacks and retries, provider ordering and filtering, routing rules, BYOK, ZDR, guardrails and DLP, request logs with metadata and feedback, usage reporting, generation lookup, log export, OpenTelemetry, analytics and a dashboard.

Not included:

- **Unified billing** (one invoice for every provider): it needs a billing relationship with each provider, which a self-hosted gateway doesn't have. Use your own provider accounts.
- **Semantic caching**: the cache is exact-match.
- **Automated evaluations and log classification** with an LLM judge; human feedback and per-model comparisons are included.
- **Translating** the Messages API to non-Anthropic models or the Responses API to non-OpenAI models; each goes to providers that speak it natively.
- Response guardrails on streamed responses, multipart uploads (image edits, transcription) through the unified API (use `/native/...`), and video generation.
