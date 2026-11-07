# Security policy

AI Gateway sits between your applications and AI providers and holds provider API keys, so we take security reports seriously.

## Reporting a vulnerability

Please **don't open a public issue, discussion or pull request** for a security problem.

Report it privately through GitHub: go to the [Security tab](https://github.com/rockbye-kunal/ai-gateway/security) and choose **Report a vulnerability** ([direct link](https://github.com/rockbye-kunal/ai-gateway/security/advisories/new)). Only the maintainers can see the report.

Please include:

- what the problem is and what an attacker could do with it;
- the steps or a request that reproduces it, and the commit or version you tested;
- the relevant part of your configuration, with keys and secrets removed.

## What to expect

- We confirm we've received your report within 3 working days.
- We tell you whether we can reproduce it, and our plan, within 10 working days.
- We fix confirmed problems as quickly as their severity calls for, and publish a security advisory when the fix is released. We credit you in the advisory unless you'd rather stay anonymous.

Please give us a reasonable chance to fix the problem before you disclose it publicly.

## Supported versions

Fixes go to the latest commit on `main` and the latest release. Older releases don't receive fixes; please upgrade.

## In scope

- Getting past client-key authentication, key permissions (`allowed_models`), limits or budgets.
- Reading another key's data: logs, reports, cache entries or stored provider keys.
- Leaking provider keys, client keys or admin tokens (in responses, logs, errors, exports or redirects).
- Getting past guardrails or zero-data-retention handling in a way that sends or stores data it shouldn't.
- Request smuggling, server-side request forgery through provider or native passthrough URLs, or crashing the gateway with a crafted request.
- Weaknesses in request signing (AWS SigV4), token handling (Google) or the encryption of stored provider keys.

## Out of scope

- Problems in the AI providers themselves, or in what models say.
- Attacks that need access to the config file, the server or Redis.
- Missing hardening on the admin port when it is exposed to the internet; it is meant to stay on a private network (see below).
- Denial of service from sheer request volume; use limits and your load balancer for that.

## Running it safely

- Keep `admin_listen` on a private network or behind your own access control, and use a long random admin token (`ai-gateway generate-key`).
- Terminate TLS at your load balancer, and use `rediss://` for managed Redis.
- Put only key hashes (`key_sha256`) in the config, and load provider keys from environment variables or a secret manager.
- Set `admin.encryption_key` before storing provider keys from the dashboard, and keep it out of version control.
- Leave `logging.capture_bodies` off unless you need prompts and responses in the log, and use zero data retention for sensitive traffic.
