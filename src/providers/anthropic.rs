//! Anthropic Messages API adapter. Translates OpenAI chat completion requests
//! (including tools and images) to Anthropic format, and responses and
//! streams back to OpenAI format, so clients only ever speak one API.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::{error::ApiError, sse::StreamTranslator, usage::Usage};

pub const API_VERSION: &str = "2023-06-01";

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::bad_request(msg)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn to_request(
    body: &Value,
    model: &str,
    stream: bool,
    default_max_tokens: u64,
) -> Result<Value, ApiError> {
    let obj = body.as_object().ok_or_else(|| bad("request body must be a JSON object"))?;
    let msgs =
        obj.get("messages").and_then(Value::as_array).ok_or_else(|| bad("'messages' must be an array"))?;

    let mut system: Vec<String> = Vec::new();
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();

    for (i, m) in msgs.iter().enumerate() {
        let role = m
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| bad(format!("messages[{i}].role is required")))?;
        match role {
            "system" | "developer" => {
                let t = content_text(m.get("content"))?;
                if !t.is_empty() {
                    system.push(t);
                }
            }
            "user" => push_turn(&mut turns, "user", content_blocks(m.get("content"))?),
            "assistant" => {
                let mut blocks = content_blocks(m.get("content"))?;
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let id = c
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| bad(format!("messages[{i}].tool_calls[].id is required")))?;
                        let f = c
                            .get("function")
                            .ok_or_else(|| bad(format!("messages[{i}].tool_calls[].function is required")))?;
                        let name = f.get("name").and_then(Value::as_str).ok_or_else(|| {
                            bad(format!("messages[{i}].tool_calls[].function.name is required"))
                        })?;
                        let args = f.get("arguments").and_then(Value::as_str).unwrap_or("");
                        let input: Value = if args.trim().is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(args).map_err(|_| {
                                bad(format!("tool call '{name}' has arguments that are not valid JSON"))
                            })?
                        };
                        blocks.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
                    }
                }
                push_turn(&mut turns, "assistant", blocks);
            }
            "tool" => {
                let id = m
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad(format!("messages[{i}].tool_call_id is required")))?;
                let text = content_text(m.get("content"))?;
                push_turn(
                    &mut turns,
                    "user",
                    vec![json!({"type": "tool_result", "tool_use_id": id, "content": text})],
                );
            }
            other => return Err(bad(format!("unsupported message role '{other}'"))),
        }
    }

    if turns.is_empty() {
        return Err(bad("at least one user message is required"));
    }

    let messages: Vec<Value> =
        turns.into_iter().map(|(role, content)| json!({"role": role, "content": content})).collect();
    let max_tokens = obj
        .get("max_completion_tokens")
        .or_else(|| obj.get("max_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(default_max_tokens);

    let mut req = Map::new();
    req.insert("model".into(), json!(model));
    req.insert("messages".into(), Value::Array(messages));
    req.insert("max_tokens".into(), json!(max_tokens));
    if !system.is_empty() {
        req.insert("system".into(), json!(system.join("\n\n")));
    }
    if let Some(t) = obj.get("temperature").and_then(Value::as_f64) {
        // OpenAI accepts 0..2, Anthropic 0..1.
        req.insert("temperature".into(), json!(t.clamp(0.0, 1.0)));
    }
    if let Some(p) = obj.get("top_p").and_then(Value::as_f64) {
        req.insert("top_p".into(), json!(p));
    }
    match obj.get("stop") {
        Some(Value::String(s)) => {
            req.insert("stop_sequences".into(), json!([s]));
        }
        Some(Value::Array(a)) => {
            let seqs: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            if !seqs.is_empty() {
                req.insert("stop_sequences".into(), json!(seqs));
            }
        }
        _ => {}
    }
    if let Some(user) = obj.get("user").and_then(Value::as_str) {
        req.insert("metadata".into(), json!({"user_id": user}));
    }

    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let mut out = Vec::with_capacity(tools.len());
        for t in tools {
            let f = t.get("function").ok_or_else(|| bad("tools[].function is required"))?;
            let name = f
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("tools[].function.name is required"))?;
            let schema =
                f.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            let mut tool = json!({"name": name, "input_schema": schema});
            if let Some(d) = f.get("description").and_then(Value::as_str) {
                tool["description"] = json!(d);
            }
            out.push(tool);
        }
        if !out.is_empty() {
            req.insert("tools".into(), Value::Array(out));
        }
    }
    if req.contains_key("tools") {
        let mut choice = match obj.get("tool_choice") {
            Some(Value::String(s)) => match s.as_str() {
                "auto" => Some(json!({"type": "auto"})),
                "required" => Some(json!({"type": "any"})),
                "none" => Some(json!({"type": "none"})),
                other => return Err(bad(format!("unsupported tool_choice '{other}'"))),
            },
            Some(Value::Object(o)) => {
                let name = o
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad("tool_choice.function.name is required"))?;
                Some(json!({"type": "tool", "name": name}))
            }
            _ => None,
        };
        if obj.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false) {
            let c = choice.get_or_insert_with(|| json!({"type": "auto"}));
            if c["type"] != "none" {
                c["disable_parallel_tool_use"] = json!(true);
            }
        }
        if let Some(c) = choice {
            req.insert("tool_choice".into(), c);
        }
    }

    if stream {
        req.insert("stream".into(), json!(true));
    }
    Ok(Value::Object(req))
}

/// Anthropic requires alternating roles; consecutive messages with the same
/// role (e.g. several tool results) are merged into one turn.
fn push_turn(turns: &mut Vec<(&'static str, Vec<Value>)>, role: &'static str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = turns.last_mut() {
        if last.0 == role {
            last.1.extend(blocks);
            return;
        }
    }
    turns.push((role, blocks));
}

fn content_blocks(content: Option<&Value>) -> Result<Vec<Value>, ApiError> {
    match content {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) if s.is_empty() => Ok(vec![]),
        Some(Value::String(s)) => Ok(vec![json!({"type": "text", "text": s})]),
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let t = p.get("text").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            blocks.push(json!({"type": "text", "text": t}));
                        }
                    }
                    Some("image_url") => {
                        let url = p
                            .get("image_url")
                            .and_then(|u| u.get("url").or(Some(u)))
                            .and_then(Value::as_str)
                            .ok_or_else(|| bad("image_url.url is required"))?;
                        blocks.push(image_block(url)?);
                    }
                    Some(other) => {
                        return Err(bad(format!(
                            "content part type '{other}' is not supported for this model"
                        )))
                    }
                    None => return Err(bad("content part is missing 'type'")),
                }
            }
            Ok(blocks)
        }
        Some(_) => Err(bad("message content must be a string or an array of parts")),
    }
}

fn content_text(content: Option<&Value>) -> Result<String, ApiError> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => Ok(parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")),
        Some(_) => Err(bad("message content must be a string or an array of parts")),
    }
}

fn image_block(url: &str) -> Result<Value, ApiError> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or_else(|| bad("malformed data URL"))?;
        let media_type =
            meta.strip_suffix(";base64").ok_or_else(|| bad("only base64 data URLs are supported"))?;
        Ok(json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}))
    } else if url.starts_with("https://") || url.starts_with("http://") {
        Ok(json!({"type": "image", "source": {"type": "url", "url": url}}))
    } else {
        Err(bad("image URLs must be http(s) or base64 data URLs"))
    }
}

fn map_stop(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "stop",
    }
}

fn usage_from(u: Option<&Value>) -> Usage {
    u.filter(|u| u.is_object()).map(Usage::from_usage_object).unwrap_or_default()
}

/// Adds Anthropic prompt-caching markers: on the system prompt and on the
/// last content block of the latest message, so the conversation prefix is
/// cached between turns. Works on a native Messages API body.
pub fn add_cache_markers(body: &mut Value) {
    let marker = json!({"type": "ephemeral"});
    let Some(obj) = body.as_object_mut() else { return };
    if let Some(system) = obj.get_mut("system") {
        if let Some(text) = system.as_str().map(str::to_string) {
            if !text.is_empty() {
                *system = json!([{"type": "text", "text": text, "cache_control": marker}]);
            }
        } else if let Some(last) = system.as_array_mut().and_then(|a| a.last_mut()) {
            if last.get("cache_control").is_none() {
                last["cache_control"] = marker.clone();
            }
        }
    }
    let Some(last_msg) = obj.get_mut("messages").and_then(Value::as_array_mut).and_then(|m| m.last_mut())
    else {
        return;
    };
    let Some(content) = last_msg.get_mut("content") else { return };
    if let Some(text) = content.as_str().map(str::to_string) {
        *content = json!([{"type": "text", "text": text, "cache_control": marker}]);
    } else if let Some(block) = content.as_array_mut().and_then(|a| a.last_mut()) {
        if block.is_object() && block.get("cache_control").is_none() {
            block["cache_control"] = marker;
        }
    }
}

pub fn from_response(v: &Value, fallback_model: &str) -> (Value, Usage) {
    let mut text = String::new();
    let mut calls = Vec::new();
    if let Some(blocks) = v.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("tool_use") => {
                    let args = b.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into());
                    calls.push(json!({
                        "id": b.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {"name": b.get("name").cloned().unwrap_or(Value::Null), "arguments": args}
                    }));
                }
                _ => {}
            }
        }
    }
    let usage = usage_from(v.get("usage"));
    let content = if text.is_empty() && !calls.is_empty() { Value::Null } else { Value::String(text) };
    let mut message = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    let out = json!({
        "id": v.get("id").and_then(Value::as_str).unwrap_or("msg"),
        "object": "chat.completion",
        "created": now_secs(),
        "model": v.get("model").and_then(Value::as_str).unwrap_or(fallback_model),
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": map_stop(v.get("stop_reason").and_then(Value::as_str)),
        }],
        "usage": {
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total(),
        }
    });
    (out, usage)
}

/// Translates Anthropic stream events into OpenAI chat completion chunks.
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
            id: "msg".into(),
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
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
        .to_string()
    }
}

impl StreamTranslator for StreamTx {
    fn on_data(&mut self, data: &str, out: &mut Vec<String>) {
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let msg = &v["message"];
                if let Some(id) = msg.get("id").and_then(Value::as_str) {
                    self.id = id.to_string();
                }
                if let Some(m) = msg.get("model").and_then(Value::as_str) {
                    self.model = m.to_string();
                }
                self.usage = usage_from(msg.get("usage"));
                out.push(self.chunk(json!({"role": "assistant", "content": ""}), None));
            }
            Some("content_block_start") => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = &v["content_block"];
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let ti = self.next_tool;
                        self.next_tool += 1;
                        self.tool_index.insert(idx, ti);
                        out.push(self.chunk(
                            json!({"tool_calls": [{
                                "index": ti,
                                "id": block.get("id").cloned().unwrap_or(Value::Null),
                                "type": "function",
                                "function": {"name": block.get("name").cloned().unwrap_or(Value::Null), "arguments": ""}
                            }]}),
                            None,
                        ));
                    }
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            out.push(self.chunk(json!({"content": t}), None));
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let d = &v["delta"];
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let t = d.get("text").and_then(Value::as_str).unwrap_or("");
                        self.chars += t.len() as u64;
                        out.push(self.chunk(json!({"content": t}), None));
                    }
                    Some("input_json_delta") => {
                        if let Some(ti) = self.tool_index.get(&idx).copied() {
                            let partial = d.get("partial_json").and_then(Value::as_str).unwrap_or("");
                            self.chars += partial.len() as u64;
                            out.push(self.chunk(
                                json!({"tool_calls": [{"index": ti, "function": {"arguments": partial}}]}),
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(r) = v["delta"].get("stop_reason").and_then(Value::as_str) {
                    self.finish = Some(map_stop(Some(r)));
                }
                if let Some(o) = v["usage"].get("output_tokens").and_then(Value::as_u64) {
                    self.usage.completion_tokens = o;
                }
                if let Some(i) = v["usage"].get("input_tokens").and_then(Value::as_u64) {
                    if i > 0 {
                        self.usage.prompt_tokens = i
                            + v["usage"]
                                .get("cache_creation_input_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0)
                            + v["usage"].get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    }
                }
            }
            Some("message_stop") => {
                out.push(self.chunk(json!({}), Some(self.finish.unwrap_or("stop"))));
                if self.client_wants_usage {
                    out.push(
                        json!({
                            "id": self.id,
                            "object": "chat.completion.chunk",
                            "created": self.created,
                            "model": self.model,
                            "choices": [],
                            "usage": {
                                "prompt_tokens": self.usage.prompt_tokens,
                                "completion_tokens": self.usage.completion_tokens,
                                "total_tokens": self.usage.total(),
                            }
                        })
                        .to_string(),
                    );
                }
                out.push("[DONE]".into());
                self.done = true;
            }
            Some("error") => {
                let e = &v["error"];
                out.push(
                    json!({"error": {
                        "message": e.get("message").and_then(Value::as_str).unwrap_or("upstream error"),
                        "type": e.get("type").and_then(Value::as_str).unwrap_or("upstream_error"),
                    }})
                    .to_string(),
                );
                // Not `done`: the guard records this as an incomplete stream.
            }
            _ => {} // ping, content_block_stop
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
    fn translates_system_tools_and_tool_results() {
        let body = json!({
            "model": "smart",
            "temperature": 1.5,
            "stop": "END",
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "Weather in Pune?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Pune\"}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "get_time", "arguments": ""}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "31C"},
                {"role": "tool", "tool_call_id": "call_2", "content": "10:00"}
            ],
            "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}],
            "tool_choice": "required",
            "parallel_tool_calls": false
        });
        let r = to_request(&body, "claude-x", true, 4096).unwrap();
        assert_eq!(r["model"], "claude-x");
        assert_eq!(r["system"], "Be brief.");
        assert_eq!(r["temperature"], 1.0);
        assert_eq!(r["stop_sequences"], json!(["END"]));
        assert_eq!(r["stream"], true);
        assert_eq!(r["max_tokens"], 4096);
        let msgs = r["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "two tool results merge into one user turn");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["input"]["city"], "Pune");
        assert_eq!(msgs[1]["content"][1]["input"], json!({}));
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(r["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(r["tool_choice"]["type"], "any");
        assert_eq!(r["tool_choice"]["disable_parallel_tool_use"], true);
    }

    #[test]
    fn translates_images() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
        ]}]});
        let r = to_request(&body, "m", false, 4096).unwrap();
        let c = &r["messages"][0]["content"];
        assert_eq!(c[1]["source"]["media_type"], "image/png");
        assert_eq!(c[2]["source"]["type"], "url");
        assert!(r.get("stream").is_none());
    }

    #[test]
    fn rejects_bad_tool_arguments() {
        let body = json!({"messages": [{"role": "assistant", "tool_calls": [
            {"id": "x", "function": {"name": "f", "arguments": "{not json"}}]}]});
        assert!(to_request(&body, "m", false, 4096).is_err());
    }

    #[test]
    fn translates_response() {
        let v = json!({
            "id": "msg_1", "model": "claude-x", "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "Checking."},
                {"type": "tool_use", "id": "tu_1", "name": "get_weather", "input": {"city": "Pune"}}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 2}
        });
        let (out, usage) = from_response(&v, "fallback");
        assert_eq!(
            usage,
            Usage { prompt_tokens: 12, completion_tokens: 5, cached_tokens: 2, ..Default::default() }
        );
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(out["choices"][0]["message"]["content"], "Checking.");
        let args = out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(args).unwrap()["city"], "Pune");
        assert_eq!(out["usage"]["total_tokens"], 17);
    }

    #[test]
    fn translates_stream() {
        let mut t = StreamTx::new("claude-x", true);
        let mut out = vec![];
        for e in [
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-x","usage":{"input_tokens":9,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu","name":"f","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            t.on_data(e, &mut out);
        }
        assert!(t.completed());
        assert_eq!(t.streamed_chars(), 7);
        assert_eq!(t.usage(), Usage { prompt_tokens: 9, completion_tokens: 7, ..Default::default() });
        assert_eq!(out.last().unwrap(), "[DONE]");
        let parsed: Vec<Value> =
            out[..out.len() - 1].iter().map(|s| serde_json::from_str(s).unwrap()).collect();
        assert_eq!(parsed[1]["choices"][0]["delta"]["content"], "Hi");
        assert_eq!(parsed[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(parsed[3]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(parsed[4]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(parsed[5]["usage"]["total_tokens"], 16);
    }
}
