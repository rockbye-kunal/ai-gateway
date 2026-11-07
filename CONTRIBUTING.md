# Contributing to AI Gateway

Thanks for helping. Bug reports, fixes, new providers, documentation and ideas are all welcome. This guide covers how to get set up and what a good pull request looks like.

By taking part you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Ways to help

- **Report a bug** with the bug report form under [Issues](https://github.com/rockbye-kunal/ai-gateway/issues/new/choose). Include the gateway version or commit, the relevant config (with secrets removed), the request you sent and what came back.
- **Suggest a feature** with the feature request form. Say what you are trying to do; there may already be a way.
- **Report a security problem privately.** Don't open a public issue; follow [SECURITY.md](SECURITY.md).
- **Send a pull request.** For anything larger than a small fix, open an issue first so we can agree on the approach before you spend time on it.

## Getting set up

You need Rust 1.89 or newer. Redis is optional, and only needed to run the Redis tests.

```bash
git clone https://github.com/rockbye-kunal/ai-gateway
cd ai-gateway
cargo build
cargo test
```

To run the gateway locally, copy `config.example.yaml` to `config.yaml`, follow the [quick start](README.md#quick-start), and run `cargo run -- serve config.yaml`. `config.yaml` and `.env` are ignored by git, so your keys stay on your machine.

## Before you open a pull request

Run the same checks CI runs:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
AIGW_TEST_REDIS_URL=redis://127.0.0.1:6379/0 cargo test -- --test-threads=4   # with Redis running locally
```

Then check that:

- **Behaviour changes come with tests.** Unit tests sit next to the code (`#[cfg(test)]`); end-to-end tests in `tests/` start a real gateway in front of mock providers. Add to the file that covers the area: `tests/e2e.rs` for the core API, `tests/parity.rs` for gateway features, `tests/cloud.rs` for Bedrock, Vertex and pricing.
- **Tests don't need the internet or real API keys.** Use the mock servers in the test files. The `live` workflow covers the real providers.
- **Settings are documented.** A new config field goes in `config.example.yaml` (commented) and in the README section for that area.
- **Nothing secret is logged.** Provider keys, client keys and request bodies must never appear in logs or errors unless body capture is turned on.
- **The change is focused.** One pull request per change makes review faster.

## Where things live

[README.md › Project layout](README.md#project-layout) lists every module. The request pipeline is in `src/proxy.rs` (`handle` → `plan_targets` → `run_targets` → `try_target`), provider adapters are in `src/providers/`, and every config field is defined and validated in `src/config.rs`.

### Adding a provider

Most providers speak the OpenAI API and need no code: add them to the config with `kind: openai`. A provider with its own API format needs:

1. a `ProviderKind` variant in `src/config.rs`, with validation for its settings;
2. a `Dialect` and request building in `src/providers/mod.rs` (`build`, `authed`, and `send` if requests must be signed);
3. a translator file in `src/providers/` with `to_request`, `from_response` and a `StreamTx` that implements `StreamTranslator` (see `bedrock.rs`);
4. price-list matching in `src/pricing.rs` (`candidates`);
5. unit tests for the translation and an end-to-end test against a mock server;
6. a README section and a commented example in `config.example.yaml`.

## Commit messages

Write the first line as a short summary in the imperative ("Add Vertex embeddings", "Fix cache key for BYOK requests"), under about 72 characters. Use the body to explain why the change is needed and anything a reviewer should know.

## Review

A maintainer will review your pull request, usually within a week. CI must pass before merging. We may ask for changes; that's a normal part of the process.

## Licence

AI Gateway is licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in the work, as defined in the Apache-2.0 licence, shall be dual licensed as above, without any additional terms or conditions.
