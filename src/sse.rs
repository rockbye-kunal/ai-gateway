//! Server-sent events: parsing upstream streams and re-emitting them in the
//! OpenAI chat completion chunk format.

use std::time::Duration;

use axum::body::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::usage::{RequestGuard, Usage};

/// Converts upstream SSE `data:` payloads into OpenAI-format `data:` payloads.
pub trait StreamTranslator: Send {
    /// Handles one upstream event payload, pushing zero or more output
    /// payloads (without the `data: ` prefix) onto `out`.
    fn on_data(&mut self, data: &str, out: &mut Vec<String>);
    /// Tokens seen so far.
    fn usage(&self) -> Usage;
    /// True once the upstream signalled a clean end of stream.
    fn completed(&self) -> bool;
    /// Characters of generated text and tool arguments forwarded so far.
    /// Used to estimate billing when a stream ends before upstream usage.
    fn streamed_chars(&self) -> u64;
}

/// Roughly four characters per token for English text and JSON.
const CHARS_PER_TOKEN: u64 = 4;

/// Usage to bill. Exact once the upstream reported it at a clean end of
/// stream; otherwise the larger of what upstream reported so far and an
/// estimate, so a client that disconnects early is still charged for what it
/// received.
pub fn effective_usage(t: &dyn StreamTranslator, prompt_estimate: u64) -> Usage {
    let reported = t.usage();
    if t.completed() && reported.total() > 0 {
        return reported;
    }
    Usage {
        // Anthropic reports exact prompt tokens at the start of a stream;
        // only estimate when nothing was reported.
        prompt_tokens: if reported.prompt_tokens > 0 { reported.prompt_tokens } else { prompt_estimate },
        completion_tokens: reported.completion_tokens.max(t.streamed_chars().div_ceil(CHARS_PER_TOKEN)),
        cached_tokens: reported.cached_tokens,
        reasoning_tokens: reported.reasoning_tokens,
        // An estimate has no provider-reported cost; configured prices apply.
        cost_microusd: None,
    }
}

/// Flat charge per image when upstream reports no usage. Roughly what
/// providers bill for a typical high-detail image.
const TOKENS_PER_IMAGE: u64 = 1_000;

/// Prompt token estimate from the request's text (messages, tool calls,
/// tool definitions) plus a flat amount per image. Base64 image data is not
/// counted as text, which would wildly over-bill vision requests.
pub fn estimate_prompt_tokens(req: &Value) -> u64 {
    let mut chars: u64 = 0;
    let mut images: u64 = 0;
    if let Some(messages) = req.get("messages").and_then(Value::as_array) {
        for m in messages {
            match m.get("content") {
                Some(Value::String(s)) => chars += s.len() as u64,
                Some(Value::Array(parts)) => {
                    for p in parts {
                        match p.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                chars += p.get("text").and_then(Value::as_str).map_or(0, |t| t.len() as u64)
                            }
                            Some("image_url") | Some("image") | Some("input_image") => images += 1,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if let Some(calls) = m.get("tool_calls") {
                chars += calls.to_string().len() as u64;
            }
        }
    }
    if let Some(tools) = req.get("tools") {
        chars += tools.to_string().len() as u64;
    }
    chars.div_ceil(CHARS_PER_TOKEN) + images * TOKENS_PER_IMAGE
}

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Incremental SSE parser. Handles events split across network chunks and
/// both `\n` and `\r\n` line endings.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// Bytes of `buf` already searched for an event boundary.
    scanned: usize,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, &'static str> {
        self.buf.extend(chunk.iter().copied().filter(|b| *b != b'\r'));
        let mut events = Vec::new();
        loop {
            // Resume one byte early in case a boundary straddles two chunks.
            let start = self.scanned.saturating_sub(1);
            let Some(rel) = find(&self.buf[start..], b"\n\n") else {
                self.scanned = self.buf.len();
                break;
            };
            let pos = start + rel;
            let raw: Vec<u8> = self.buf.drain(..pos + 2).collect();
            self.scanned = 0;
            let text = String::from_utf8_lossy(&raw[..pos]);
            let mut data = String::new();
            let mut has_data = false;
            for line in text.split('\n') {
                if let Some(rest) = line.strip_prefix("data:") {
                    if has_data {
                        data.push('\n');
                    }
                    data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                    has_data = true;
                }
            }
            if has_data {
                events.push(data);
            }
        }
        if self.buf.len() > MAX_EVENT_BYTES {
            return Err("upstream stream event exceeded the size limit");
        }
        Ok(events)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn error_event(message: &str) -> Bytes {
    let payload = json!({ "error": { "message": message, "type": "upstream_error" } });
    Bytes::from(format!("data: {payload}\n\n"))
}

/// Wraps an upstream streaming response. Enforces an idle timeout between
/// chunks, translates events, and keeps the request guard updated with usage.
/// Errors after the stream has started are sent to the client as an SSE error
/// event, since the HTTP status has already gone out.
/// Where a completed stream is saved for replay from the response cache.
pub struct CacheSink {
    pub key: String,
    pub ttl_secs: u64,
    pub max_bytes: usize,
    pub provider: String,
    pub model: String,
}

impl CacheSink {
    fn save(self, guard: &RequestGuard, body: Vec<u8>) {
        if body.len() > self.max_bytes {
            return;
        }
        let Ok(body) = String::from_utf8(body) else { return };
        let entry = json!({"provider": self.provider, "model": self.model, "body": body, "stream": true});
        let app = guard.app().clone();
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move {
                let raw = serde_json::to_vec(&entry).unwrap_or_default();
                if let Err(e) = app.store.cache_set(&self.key, &raw, self.ttl_secs).await {
                    metrics::counter!("gateway_store_errors_total", "op" => "cache_set").increment(1);
                    tracing::warn!(error = %e, "failed to cache stream");
                }
            });
        }
    }
}

pub fn event_stream(
    resp: reqwest::Response,
    mut translator: Box<dyn StreamTranslator>,
    idle_timeout: Duration,
    prompt_estimate: u64,
    mut cancel: watch::Receiver<bool>,
    guard: RequestGuard,
    cache: Option<CacheSink>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    async_stream::stream! {
        let mut upstream = Box::pin(resp.bytes_stream());
        let mut parser = SseParser::default();
        let mut out: Vec<String> = Vec::new();
        let mut cached: Option<Vec<u8>> = cache.as_ref().map(|_| Vec::new());
        let mut first = true;
        loop {
            // Ends the stream when the gateway's shutdown grace period runs
            // out, so the guard drops and usage is recorded before exit.
            let cancelled = async {
                // Evaluate to a bool so the non-Send watch::Ref is dropped
                // before any further await.
                let fired = cancel.wait_for(|v| *v).await.is_ok();
                if !fired {
                    std::future::pending::<()>().await;
                }
            };
            let next = tokio::select! {
                biased;
                _ = cancelled => None,
                n = tokio::time::timeout(idle_timeout, upstream.next()) => Some(n),
            };
            let Some(next) = next else {
                guard.update(|s| s.error = Some("gateway shutting down".into()));
                yield Ok(error_event("gateway is shutting down"));
                break;
            };
            match next {
                Err(_) => {
                    guard.update(|s| s.error = Some("upstream stream idle timeout".into()));
                    yield Ok(error_event("upstream stream timed out"));
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    tracing::warn!(request_id = guard.request_id(), error = %e, "upstream stream read failed");
                    guard.update(|s| s.error = Some("upstream stream interrupted".into()));
                    yield Ok(error_event("upstream stream interrupted"));
                    break;
                }
                Ok(Some(Ok(chunk))) => {
                    let parsed = parser.push(&chunk);
                    let events = match parsed {
                        Ok(events) => events,
                        Err(msg) => {
                            guard.update(|s| s.error = Some(msg.into()));
                            yield Ok(error_event(msg));
                            break;
                        }
                    };
                    for data in &events {
                        translator.on_data(data, &mut out);
                    }
                    // Updated on every chunk: if the client disconnects, this
                    // generator is dropped and nothing after the loop runs.
                    let usage = effective_usage(translator.as_ref(), prompt_estimate);
                    guard.update(|s| s.usage = usage);
                    for payload in std::mem::take(&mut out) {
                        if first {
                            first = false;
                            guard.mark_first_byte();
                        }
                        guard.capture_stream_chunk(&payload);
                        let frame = format!("data: {payload}\n\n");
                        if let Some(buf) = cached.as_mut() {
                            buf.extend_from_slice(frame.as_bytes());
                        }
                        yield Ok(Bytes::from(frame));
                    }
                    if translator.completed() {
                        break;
                    }
                }
            }
        }
        let usage = effective_usage(translator.as_ref(), prompt_estimate);
        let completed = translator.completed();
        guard.update(|s| {
            s.usage = usage;
            if !completed && s.error.is_none() {
                s.error = Some("stream ended before completion".into());
            }
        });
        if let (true, Some(sink), Some(buf)) = (completed, cache, cached) {
            sink.save(&guard, buf);
        }
    }
}

/// Watches a provider-native stream (Responses API, Anthropic Messages, or
/// anything through the native passthrough) without changing it: usage,
/// completion and generated text are read from a copy of each event.
#[derive(Default)]
pub struct Sniffer {
    usage: Usage,
    done: bool,
    chars: u64,
}

impl Sniffer {
    /// Returns generated text found in the event, if any.
    fn on_data(&mut self, data: &str) -> Option<String> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return None;
        }
        let v: Value = serde_json::from_str(data).ok()?;
        if let Some(u) = Usage::sniff(&v) {
            self.usage.merge(u);
        }
        let t = v.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(t, "message_stop" | "response.completed" | "response.failed" | "response.incomplete") {
            self.done = true;
        }
        let text = match t {
            "response.output_text.delta" | "response.function_call_arguments.delta" => {
                v.get("delta").and_then(Value::as_str).map(str::to_string)
            }
            "content_block_delta" => {
                let d = &v["delta"];
                d.get("text").or_else(|| d.get("partial_json")).and_then(Value::as_str).map(str::to_string)
            }
            _ => v["choices"][0]["delta"]["content"].as_str().map(str::to_string),
        };
        if let Some(t) = &text {
            self.chars += t.len() as u64;
        }
        text
    }

    fn effective(&self, prompt_estimate: u64) -> Usage {
        if self.done && self.usage.total() > 0 {
            return self.usage;
        }
        Usage {
            prompt_tokens: if self.usage.prompt_tokens > 0 {
                self.usage.prompt_tokens
            } else {
                prompt_estimate
            },
            completion_tokens: self.usage.completion_tokens.max(self.chars.div_ceil(CHARS_PER_TOKEN)),
            cost_microusd: None,
            ..self.usage
        }
    }
}

/// Passes an upstream stream through byte for byte (event names included),
/// with the same idle timeout, shutdown and billing guarantees as
/// `event_stream`.
pub fn raw_stream(
    resp: reqwest::Response,
    idle_timeout: Duration,
    prompt_estimate: u64,
    mut cancel: watch::Receiver<bool>,
    guard: RequestGuard,
    cache: Option<CacheSink>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    async_stream::stream! {
        let mut upstream = Box::pin(resp.bytes_stream());
        let mut parser = SseParser::default();
        let mut sniff = Sniffer::default();
        let mut cached: Option<Vec<u8>> = cache.as_ref().map(|_| Vec::new());
        let mut first = true;
        loop {
            let cancelled = async {
                let fired = cancel.wait_for(|v| *v).await.is_ok();
                if !fired {
                    std::future::pending::<()>().await;
                }
            };
            let next = tokio::select! {
                biased;
                _ = cancelled => None,
                n = tokio::time::timeout(idle_timeout, upstream.next()) => Some(n),
            };
            let Some(next) = next else {
                guard.update(|s| s.error = Some("gateway shutting down".into()));
                break;
            };
            match next {
                Err(_) => {
                    guard.update(|s| s.error = Some("upstream stream idle timeout".into()));
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    tracing::warn!(request_id = guard.request_id(), error = %e, "upstream stream read failed");
                    guard.update(|s| s.error = Some("upstream stream interrupted".into()));
                    break;
                }
                Ok(Some(Ok(chunk))) => {
                    if first {
                        first = false;
                        guard.mark_first_byte();
                    }
                    // Parse a copy for usage; a parse error only stops sniffing.
                    if let Ok(events) = parser.push(&chunk) {
                        for data in &events {
                            if let Some(t) = sniff.on_data(data) {
                                guard.capture_append(&t);
                            }
                        }
                    }
                    let usage = sniff.effective(prompt_estimate);
                    guard.update(|s| s.usage = usage);
                    if let Some(buf) = cached.as_mut() {
                        buf.extend_from_slice(&chunk);
                    }
                    yield Ok(chunk);
                }
            }
        }
        let usage = sniff.effective(prompt_estimate);
        let completed = sniff.done;
        guard.update(|s| {
            s.usage = usage;
            if !completed && s.error.is_none() {
                s.error = Some("stream ended before completion".into());
            }
        });
        if let (true, Some(sink), Some(buf)) = (completed, cache, cached) {
            sink.save(&guard, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.push(b"event: x\ndata: {\"a\"").unwrap().is_empty());
        let ev = p.push(b":1}\n\ndata: [DONE]\n\n").unwrap();
        assert_eq!(ev, vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut p = SseParser::default();
        let ev = p.push(b": keep-alive\r\n\r\ndata: one\r\n\r\n").unwrap();
        assert_eq!(ev, vec!["one".to_string()]);
    }

    #[test]
    fn prompt_estimate_ignores_image_bytes() {
        let big = format!("data:image/png;base64,{}", "A".repeat(4_000_000));
        let req = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "12345678"},
            {"type": "image_url", "image_url": {"url": big}}
        ]}]});
        assert_eq!(estimate_prompt_tokens(&req), 2 + TOKENS_PER_IMAGE);
    }

    #[test]
    fn joins_multiline_data() {
        let mut p = SseParser::default();
        let ev = p.push(b"data: a\ndata: b\n\n").unwrap();
        assert_eq!(ev, vec!["a\nb".to_string()]);
    }
}
