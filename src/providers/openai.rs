//! OpenAI-compatible providers (OpenAI, Azure OpenAI, Gemini's OpenAI
//! endpoint, Groq, and similar). Requests pass through with the model
//! rewritten; streams pass through with usage captured.

use serde_json::{json, Value};

use crate::{sse::StreamTranslator, usage::Usage};

/// Copies the client body, sets the upstream model, and on streams asks for a
/// final usage chunk so token budgets can be enforced.
pub fn prepare(body: &Value, model: &str, force_stream_usage: bool) -> Value {
    let mut b = body.clone();
    if let Some(o) = b.as_object_mut() {
        o.insert("model".into(), Value::String(model.to_string()));
        if force_stream_usage {
            let so = o.entry("stream_options").or_insert_with(|| json!({}));
            if !so.is_object() {
                *so = json!({});
            }
            if let Some(so) = so.as_object_mut() {
                so.insert("include_usage".into(), Value::Bool(true));
            }
        }
    }
    b
}

pub struct Passthrough {
    client_wants_usage: bool,
    usage: Usage,
    done: bool,
    chars: u64,
}

impl Passthrough {
    pub fn new(client_wants_usage: bool) -> Self {
        Self { client_wants_usage, usage: Usage::default(), done: false, chars: 0 }
    }
}

impl StreamTranslator for Passthrough {
    fn on_data(&mut self, data: &str, out: &mut Vec<String>) {
        if data.trim() == "[DONE]" {
            self.done = true;
            out.push("[DONE]".into());
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            out.push(data.to_string());
            return;
        };
        if let Some(choices) = v.get("choices").and_then(Value::as_array) {
            for c in choices {
                let d = &c["delta"];
                if let Some(t) = d.get("content").and_then(Value::as_str) {
                    self.chars += t.len() as u64;
                }
                if let Some(calls) = d.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let f = &call["function"];
                        for k in ["name", "arguments"] {
                            if let Some(t) = f.get(k).and_then(Value::as_str) {
                                self.chars += t.len() as u64;
                            }
                        }
                    }
                }
            }
        }
        if let Some(u) = Usage::from_openai(&v) {
            self.usage = u;
            // The gateway asked for this usage-only chunk; hide it from
            // clients that did not ask for it themselves.
            let usage_only = v.get("choices").and_then(Value::as_array).is_some_and(|c| c.is_empty());
            if usage_only && !self.client_wants_usage {
                return;
            }
        }
        out.push(data.to_string());
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
    fn prepare_sets_model_and_usage() {
        let body = json!({"model":"alias","messages":[],"stream":true,"stream_options":null});
        let b = prepare(&body, "gpt-4o", true);
        assert_eq!(b["model"], "gpt-4o");
        assert_eq!(b["stream_options"]["include_usage"], true);
        let b = prepare(&body, "gpt-4o", false);
        assert!(b["stream_options"].is_null());
    }

    #[test]
    fn reads_openrouter_cost() {
        let v = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.000123}});
        let u = Usage::from_openai(&v).unwrap();
        assert_eq!(u.total(), 15);
        assert_eq!(u.cost_microusd, Some(123));
        let v = json!({"usage": {"prompt_tokens": 1, "completion_tokens": 1}});
        assert_eq!(Usage::from_openai(&v).unwrap().cost_microusd, None);
    }

    #[test]
    fn hides_usage_chunk_unless_requested() {
        let usage_chunk = r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4}}"#;
        let mut t = Passthrough::new(false);
        let mut out = vec![];
        t.on_data(r#"{"choices":[{"delta":{"content":"hi"}}]}"#, &mut out);
        t.on_data(usage_chunk, &mut out);
        t.on_data("[DONE]", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(t.usage().total(), 7);
        assert!(t.completed());
        assert_eq!(t.streamed_chars(), 2);

        let mut t = Passthrough::new(true);
        let mut out = vec![];
        t.on_data(usage_chunk, &mut out);
        assert_eq!(out.len(), 1);
    }
}
