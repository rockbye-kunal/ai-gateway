//! AWS Bedrock Converse API: request and response translation.
//!
//! The OpenAI request is first turned into Anthropic Messages form (which
//! already handles system prompts, tool calls, tool results and images) and
//! then into Converse, which has the same structure with different names.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::anthropic;
use crate::{error::ApiError, sse::StreamTranslator, usage::Usage};

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::bad_request(msg)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn image_format(media_type: &str) -> Result<&'static str, ApiError> {
    match media_type {
        "image/png" => Ok("png"),
        "image/jpeg" | "image/jpg" => Ok("jpeg"),
        "image/gif" => Ok("gif"),
        "image/webp" => Ok("webp"),
        other => Err(bad(format!("image type '{other}' is not supported by Bedrock"))),
    }
}

fn block(b: &Value) -> Result<Value, ApiError> {
    match b.get("type").and_then(Value::as_str) {
        Some("text") => Ok(json!({"text": b["text"]})),
        Some("image") => {
            let src = &b["source"];
            if src["type"] != "base64" {
                return Err(bad("Bedrock only accepts images as base64 data URLs"));
            }
            let format = image_format(src["media_type"].as_str().unwrap_or(""))?;
            Ok(json!({"image": {"format": format, "source": {"bytes": src["data"]}}}))
        }
        Some("tool_use") => {
            Ok(json!({"toolUse": {"toolUseId": b["id"], "name": b["name"], "input": b["input"]}}))
        }
        Some("tool_result") => {
            let text = b["content"].as_str().unwrap_or_default();
            Ok(json!({"toolResult": {"toolUseId": b["tool_use_id"], "content": [{"text": text}]}}))
        }
        _ => Err(bad("unsupported content for Bedrock")),
    }
}

/// Builds a Converse request body from an OpenAI chat completion request.
pub fn to_request(body: &Value, default_max_tokens: u64, cache: bool) -> Result<Value, ApiError> {
    let a = anthropic::to_request(body, "", false, default_max_tokens)?;
    let mut req = Map::new();

    let mut messages = Vec::new();
    for m in a["messages"].as_array().into_iter().flatten() {
        let mut content = Vec::new();
        for b in m["content"].as_array().into_iter().flatten() {
            content.push(block(b)?);
        }
        messages.push(json!({"role": m["role"], "content": content}));
    }
    if cache {
        if let Some(c) = messages.last_mut().and_then(|m| m["content"].as_array_mut()) {
            c.push(json!({"cachePoint": {"type": "default"}}));
        }
    }
    req.insert("messages".into(), Value::Array(messages));

    if let Some(system) = a.get("system").and_then(Value::as_str) {
        let mut s = vec![json!({"text": system})];
        if cache {
            s.push(json!({"cachePoint": {"type": "default"}}));
        }
        req.insert("system".into(), Value::Array(s));
    }

    let mut inference = Map::new();
    inference.insert("maxTokens".into(), a["max_tokens"].clone());
    // Converse accepts the OpenAI range for models that support it; clamp
    // like Anthropic only matters for Claude, which Bedrock validates itself.
    if let Some(t) = body.get("temperature").and_then(Value::as_f64) {
        inference.insert("temperature".into(), json!(t.clamp(0.0, 1.0)));
    }
    if let Some(p) = a.get("top_p") {
        inference.insert("topP".into(), p.clone());
    }
    if let Some(s) = a.get("stop_sequences") {
        inference.insert("stopSequences".into(), s.clone());
    }
    req.insert("inferenceConfig".into(), Value::Object(inference));

    if let Some(tools) = a.get("tools").and_then(Value::as_array) {
        let specs: Vec<Value> = tools
            .iter()
            .map(|t| {
                let mut spec = json!({"name": t["name"], "inputSchema": {"json": t["input_schema"]}});
                if let Some(d) = t.get("description") {
                    spec["description"] = d.clone();
                }
                json!({"toolSpec": spec})
            })
            .collect();
        let mut tc = json!({"tools": specs});
        match a.get("tool_choice").and_then(|c| c.get("type")).and_then(Value::as_str) {
            Some("any") => tc["toolChoice"] = json!({"any": {}}),
            Some("tool") => tc["toolChoice"] = json!({"tool": {"name": a["tool_choice"]["name"]}}),
            Some("auto") => tc["toolChoice"] = json!({"auto": {}}),
            _ => {}
        }
        req.insert("toolConfig".into(), tc);
    }

    // Model-specific fields (e.g. Claude's `thinking`) go through untouched.
    if let Some(extra) = body.get("additionalModelRequestFields") {
        req.insert("additionalModelRequestFields".into(), extra.clone());
    }
    if let Some(g) = body.get("guardrailConfig") {
        req.insert("guardrailConfig".into(), g.clone());
    }
    Ok(Value::Object(req))
}

pub fn map_stop(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens") | Some("model_context_window_exceeded") => "length",
        Some("tool_use") => "tool_calls",
        Some("guardrail_intervened") | Some("content_filtered") => "content_filter",
        _ => "stop",
    }
}

pub fn usage_from(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let (read, write) = (n("cacheReadInputTokens"), n("cacheWriteInputTokens"));
    Usage {
        prompt_tokens: n("inputTokens") + read + write,
        completion_tokens: n("outputTokens"),
        cached_tokens: read,
        cache_write_tokens: write,
        ..Default::default()
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "prompt_tokens": u.prompt_tokens,
        "completion_tokens": u.completion_tokens,
        "total_tokens": u.total(),
        "prompt_tokens_details": {"cached_tokens": u.cached_tokens},
    })
}

/// Converts a Converse response to an OpenAI chat completion.
pub fn from_response(v: &Value, model: &str) -> (Value, Usage) {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for b in v["output"]["message"]["content"].as_array().into_iter().flatten() {
        if let Some(t) = b.get("text").and_then(Value::as_str) {
            text.push_str(t);
        } else if let Some(tu) = b.get("toolUse") {
            calls.push(json!({
                "id": tu["toolUseId"],
                "type": "function",
                "function": {"name": tu["name"], "arguments": tu.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into())}
            }));
        } else if let Some(t) = b["reasoningContent"]["reasoningText"]["text"].as_str() {
            reasoning.push_str(t);
        }
    }
    let usage = usage_from(&v["usage"]);
    let content = if text.is_empty() && !calls.is_empty() { Value::Null } else { Value::String(text) };
    let mut message = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    let out = json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": now_secs(),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": map_stop(v["stopReason"].as_str())}],
        "usage": usage_json(&usage),
    });
    (out, usage)
}

/// Translates Converse stream events (decoded from the AWS event stream)
/// into OpenAI chat completion chunks.
pub struct StreamTx {
    id: String,
    model: String,
    created: u64,
    client_wants_usage: bool,
    usage: Usage,
    tool_index: HashMap<u64, usize>,
    next_tool: usize,
    finish: Option<&'static str>,
    done: bool,
    chars: u64,
}

impl StreamTx {
    pub fn new(model: &str, client_wants_usage: bool) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            created: now_secs(),
            client_wants_usage,
            usage: Usage::default(),
            tool_index: HashMap::new(),
            next_tool: 0,
            finish: None,
            done: false,
            chars: 0,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
        .to_string()
    }

    fn finish(&mut self, out: &mut Vec<String>) {
        out.push(self.chunk(json!({}), Some(self.finish.unwrap_or("stop"))));
        if self.client_wants_usage {
            out.push(
                json!({
                    "id": self.id, "object": "chat.completion.chunk", "created": self.created,
                    "model": self.model, "choices": [], "usage": usage_json(&self.usage)
                })
                .to_string(),
            );
        }
        out.push("[DONE]".into());
        self.done = true;
    }
}

impl StreamTranslator for StreamTx {
    fn on_data(&mut self, data: &str, out: &mut Vec<String>) {
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "messageStart" => out.push(self.chunk(json!({"role": "assistant", "content": ""}), None)),
            "contentBlockStart" => {
                if let Some(tu) = v["start"].get("toolUse") {
                    let idx = v["contentBlockIndex"].as_u64().unwrap_or(0);
                    let ti = self.next_tool;
                    self.next_tool += 1;
                    self.tool_index.insert(idx, ti);
                    out.push(self.chunk(
                        json!({"tool_calls": [{
                            "index": ti, "id": tu["toolUseId"], "type": "function",
                            "function": {"name": tu["name"], "arguments": ""}
                        }]}),
                        None,
                    ));
                }
            }
            "contentBlockDelta" => {
                let d = &v["delta"];
                if let Some(t) = d.get("text").and_then(Value::as_str) {
                    self.chars += t.len() as u64;
                    out.push(self.chunk(json!({"content": t}), None));
                } else if let Some(input) = d["toolUse"].get("input").and_then(Value::as_str) {
                    let idx = v["contentBlockIndex"].as_u64().unwrap_or(0);
                    if let Some(ti) = self.tool_index.get(&idx).copied() {
                        self.chars += input.len() as u64;
                        out.push(self.chunk(
                            json!({"tool_calls": [{"index": ti, "function": {"arguments": input}}]}),
                            None,
                        ));
                    }
                } else if let Some(t) = d["reasoningContent"].get("text").and_then(Value::as_str) {
                    self.chars += t.len() as u64;
                    out.push(self.chunk(json!({"reasoning_content": t}), None));
                }
            }
            "messageStop" => {
                self.finish = Some(map_stop(v["stopReason"].as_str()));
            }
            "metadata" => {
                self.usage = usage_from(&v["usage"]);
                self.finish(out);
            }
            k if k.starts_with("exception:") => {
                let message = v.get("message").and_then(Value::as_str).unwrap_or("upstream error");
                out.push(
                    json!({"error": {"message": message, "type": &k["exception:".len()..]}}).to_string(),
                );
            }
            _ => {}
        }
    }

    fn usage(&self) -> Usage {
        self.usage
    }

    fn completed(&self) -> bool {
        self.done
    }

    fn streamed_chars(&self) -> u64 {
        self.chars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_request() {
        let body = json!({
            "temperature": 0.3,
            "max_tokens": 100,
            "stop": ["END"],
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [
                    {"type": "text", "text": "Weather in Pune?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "t1", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Pune\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "t1", "content": "31C"}
            ],
            "tools": [{"type": "function", "function": {"name": "weather", "description": "Get weather", "parameters": {"type": "object"}}}],
            "tool_choice": "required"
        });
        let r = to_request(&body, 4096, true).unwrap();
        assert_eq!(r["system"][0]["text"], "Be brief.");
        assert_eq!(r["system"][1]["cachePoint"]["type"], "default");
        assert_eq!(r["inferenceConfig"]["maxTokens"], 100);
        assert_eq!(r["inferenceConfig"]["temperature"], 0.3);
        assert_eq!(r["inferenceConfig"]["stopSequences"], json!(["END"]));
        let m = r["messages"].as_array().unwrap();
        assert_eq!(m[0]["content"][1]["image"]["format"], "png");
        assert_eq!(m[0]["content"][1]["image"]["source"]["bytes"], "AAAA");
        assert_eq!(m[1]["content"][0]["toolUse"]["input"]["city"], "Pune");
        assert_eq!(m[2]["content"][0]["toolResult"]["toolUseId"], "t1");
        assert_eq!(m[2]["content"][1]["cachePoint"]["type"], "default");
        assert_eq!(r["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"], "object");
        assert_eq!(r["toolConfig"]["toolChoice"], json!({"any": {}}));

        let url_image = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}]}]});
        assert!(to_request(&url_image, 4096, false).is_err());
    }

    #[test]
    fn translates_response() {
        let v = json!({
            "output": {"message": {"role": "assistant", "content": [
                {"text": "Calling."},
                {"toolUse": {"toolUseId": "t1", "name": "weather", "input": {"city": "Pune"}}}
            ]}},
            "stopReason": "tool_use",
            "usage": {"inputTokens": 10, "outputTokens": 5, "cacheReadInputTokens": 90, "cacheWriteInputTokens": 0}
        });
        let (out, usage) = from_response(&v, "claude");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"Pune\"}"
        );
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.cached_tokens, 90);
        assert_eq!(out["usage"]["total_tokens"], 105);
    }

    #[test]
    fn translates_stream() {
        let mut tx = StreamTx::new("m", true);
        let mut out = Vec::new();
        for e in [
            json!({"type": "messageStart", "role": "assistant"}),
            json!({"type": "contentBlockDelta", "contentBlockIndex": 0, "delta": {"text": "Hi"}}),
            json!({"type": "contentBlockStart", "contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "t", "name": "f"}}}),
            json!({"type": "contentBlockDelta", "contentBlockIndex": 1, "delta": {"toolUse": {"input": "{}"}}}),
            json!({"type": "messageStop", "stopReason": "tool_use"}),
            json!({"type": "metadata", "usage": {"inputTokens": 3, "outputTokens": 2}}),
        ] {
            tx.on_data(&e.to_string(), &mut out);
        }
        assert!(tx.completed());
        assert_eq!(tx.usage().total(), 5);
        assert_eq!(out.last().unwrap(), "[DONE]");
        let finish: Value = serde_json::from_str(&out[out.len() - 3]).unwrap();
        assert_eq!(finish["choices"][0]["finish_reason"], "tool_calls");
        let tool: Value = serde_json::from_str(&out[2]).unwrap();
        assert_eq!(tool["choices"][0]["delta"]["tool_calls"][0]["id"], "t");
    }
}
