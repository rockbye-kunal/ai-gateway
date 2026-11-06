//! Shipping request logs out of the gateway.
//!
//! - HTTP export ("logpush"): batches of log entries POSTed as NDJSON.
//! - OpenTelemetry: one trace per request (a server span plus a client span
//!   per upstream attempt) POSTed as OTLP/HTTP JSON to `<endpoint>/v1/traces`,
//!   using the GenAI semantic conventions (`gen_ai.*`). A W3C `traceparent`
//!   sent by the client is honoured, so gateway spans join the caller's trace.
//!
//! Entries go through bounded queues; when a destination is slow or down and
//! a queue fills, new entries are dropped and counted rather than slowing
//! requests down.

use std::time::Duration;

use rand::RngCore;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::{
    config::{HttpExportConfig, LoggingConfig, OtlpConfig},
    logs::LogEntry,
};

const QUEUE: usize = 10_000;

enum Msg {
    Entry(Box<LogEntry>),
    Flush(oneshot::Sender<()>),
}

#[derive(Default)]
pub struct Exporter {
    http: Option<(mpsc::Sender<Msg>, bool)>,
    otlp: Option<mpsc::Sender<Msg>>,
}

fn headers(h: &std::collections::HashMap<String, crate::config::Secret>) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in h {
        if let (Ok(n), Ok(mut v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v.expose()))
        {
            v.set_sensitive(true);
            out.insert(n, v);
        }
    }
    out
}

impl Exporter {
    pub fn start(cfg: &LoggingConfig, client: reqwest::Client) -> Self {
        let mut ex = Exporter::default();
        if let Some(h) = &cfg.export {
            let (tx, rx) = mpsc::channel(QUEUE);
            tokio::spawn(run_http(h.clone(), client.clone(), rx));
            ex.http = Some((tx, h.include_bodies));
        }
        if let Some(o) = &cfg.otlp {
            let (tx, rx) = mpsc::channel(QUEUE);
            tokio::spawn(run_otlp(o.clone(), client, rx));
            ex.otlp = Some(tx);
        }
        ex
    }

    pub fn send(&self, e: &LogEntry) {
        if let Some((tx, bodies)) = &self.http {
            let mut e = e.clone();
            if !bodies {
                e.request_body = None;
                e.response_body = None;
            }
            if tx.try_send(Msg::Entry(Box::new(e))).is_err() {
                metrics::counter!("gateway_export_dropped_total", "sink" => "http").increment(1);
            }
        }
        if let Some(tx) = &self.otlp {
            let mut e = e.clone();
            e.request_body = None;
            e.response_body = None;
            if tx.try_send(Msg::Entry(Box::new(e))).is_err() {
                metrics::counter!("gateway_export_dropped_total", "sink" => "otlp").increment(1);
            }
        }
    }

    /// Flushes queued entries, waiting at most `timeout`. Used at shutdown.
    pub async fn drain(&self, timeout: Duration) {
        let mut waits = Vec::new();
        for tx in self.http.iter().map(|(t, _)| t).chain(self.otlp.iter()) {
            let (done, wait) = oneshot::channel();
            if tx.send(Msg::Flush(done)).await.is_ok() {
                waits.push(wait);
            }
        }
        let _ = tokio::time::timeout(timeout, futures::future::join_all(waits)).await;
    }
}

async fn post_with_retry(
    client: &reqwest::Client,
    url: &str,
    hdrs: &HeaderMap,
    ct: &str,
    body: Vec<u8>,
    sink: &'static str,
) {
    let mut delay = Duration::from_millis(500);
    for attempt in 1..=3 {
        let res = client
            .post(url)
            .headers(hdrs.clone())
            .header("content-type", ct)
            .timeout(Duration::from_secs(15))
            .body(body.clone())
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                metrics::counter!("gateway_export_batches_total", "sink" => sink, "result" => "ok")
                    .increment(1);
                return;
            }
            Ok(r) if r.status().is_client_error() && r.status().as_u16() != 429 => {
                tracing::warn!(sink, status = r.status().as_u16(), "log export rejected; dropping batch");
                break;
            }
            Ok(r) if attempt == 3 => tracing::warn!(sink, status = r.status().as_u16(), "log export failed"),
            Err(e) if attempt == 3 => tracing::warn!(sink, error = %e, "log export failed"),
            _ => {}
        }
        tokio::time::sleep(delay).await;
        delay *= 2;
    }
    metrics::counter!("gateway_export_batches_total", "sink" => sink, "result" => "failed").increment(1);
}

async fn batch_loop<F, Fut>(mut rx: mpsc::Receiver<Msg>, batch_size: usize, flush_secs: u64, mut flush: F)
where
    F: FnMut(Vec<LogEntry>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut buf: Vec<LogEntry> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(flush_secs.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Msg::Entry(e)) => {
                    buf.push(*e);
                    if buf.len() >= batch_size {
                        flush(std::mem::take(&mut buf)).await;
                    }
                }
                Some(Msg::Flush(done)) => {
                    if !buf.is_empty() {
                        flush(std::mem::take(&mut buf)).await;
                    }
                    let _ = done.send(());
                }
                None => {
                    if !buf.is_empty() {
                        flush(std::mem::take(&mut buf)).await;
                    }
                    return;
                }
            },
            _ = tick.tick() => {
                if !buf.is_empty() {
                    flush(std::mem::take(&mut buf)).await;
                }
            }
        }
    }
}

async fn run_http(cfg: HttpExportConfig, client: reqwest::Client, rx: mpsc::Receiver<Msg>) {
    let hdrs = headers(&cfg.headers);
    let url = cfg.url.clone();
    batch_loop(rx, cfg.batch_size, cfg.flush_secs, |batch| {
        let mut body = Vec::new();
        for e in &batch {
            if let Ok(line) = serde_json::to_vec(e) {
                body.extend_from_slice(&line);
                body.push(b'\n');
            }
        }
        let (client, url, hdrs) = (client.clone(), url.clone(), hdrs.clone());
        async move { post_with_retry(&client, &url, &hdrs, "application/x-ndjson", body, "http").await }
    })
    .await;
}

async fn run_otlp(cfg: OtlpConfig, client: reqwest::Client, rx: mpsc::Receiver<Msg>) {
    let hdrs = headers(&cfg.headers);
    let url = format!("{}/v1/traces", cfg.endpoint.trim_end_matches('/'));
    let service = cfg.service_name.clone();
    batch_loop(rx, cfg.batch_size, cfg.flush_secs, |batch| {
        let body = serde_json::to_vec(&otlp_payload(&service, &batch)).unwrap_or_default();
        let (client, url, hdrs) = (client.clone(), url.clone(), hdrs.clone());
        async move { post_with_retry(&client, &url, &hdrs, "application/json", body, "otlp").await }
    })
    .await;
}

fn random_span_id() -> String {
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

fn attr_str(k: &str, v: &str) -> Value {
    json!({"key": k, "value": {"stringValue": v}})
}
fn attr_int(k: &str, v: u64) -> Value {
    json!({"key": k, "value": {"intValue": v.to_string()}})
}
fn attr_f64(k: &str, v: f64) -> Value {
    json!({"key": k, "value": {"doubleValue": v}})
}

/// Parses a W3C traceparent: `00-<32 hex trace>-<16 hex span>-<flags>`.
pub fn parse_traceparent(v: &str) -> Option<(String, String)> {
    let mut p = v.trim().split('-');
    let (_, t, s) = (p.next()?, p.next()?, p.next()?);
    let hex = |x: &str, n: usize| {
        x.len() == n && x.chars().all(|c| c.is_ascii_hexdigit()) && x.chars().any(|c| c != '0')
    };
    (hex(t, 32) && hex(s, 16)).then(|| (t.to_lowercase(), s.to_lowercase()))
}

pub fn otlp_payload(service: &str, batch: &[LogEntry]) -> Value {
    let mut spans = Vec::new();
    for e in batch {
        let trace_id = e.trace_id.clone().unwrap_or_else(|| e.id.replace('-', ""));
        let root = random_span_id();
        let start = e.ts_ms * 1_000_000;
        let end = (e.ts_ms + e.latency_ms) * 1_000_000;
        let op = match e.endpoint.as_str() {
            "embeddings" => "embeddings",
            "chat" | "messages" | "responses" => "chat",
            other => other,
        };
        let mut attrs = vec![
            attr_str("gen_ai.operation.name", op),
            attr_str("gateway.request_id", &e.id),
            attr_str("gateway.key", &e.tenant),
            attr_str("gateway.endpoint", &e.endpoint),
            attr_int("http.response.status_code", u64::from(e.status)),
            attr_int("gen_ai.usage.input_tokens", e.prompt_tokens),
            attr_int("gen_ai.usage.output_tokens", e.completion_tokens),
            attr_str("gateway.cache", if e.cache_hit { "hit" } else { "miss" }),
        ];
        if let Some(m) = &e.route {
            attrs.push(attr_str("gen_ai.request.model", m));
        }
        if let Some(m) = &e.model {
            attrs.push(attr_str("gen_ai.response.model", m));
        }
        if let Some(p) = &e.provider {
            attrs.push(attr_str("gen_ai.system", p));
        }
        if let Some(c) = e.cost_usd {
            attrs.push(attr_f64("gateway.cost_usd", c));
        }
        if let Some(t) = e.ttft_ms {
            attrs.push(attr_int("gateway.ttft_ms", t));
        }
        if let Some(u) = &e.user {
            attrs.push(attr_str("enduser.id", u));
        }
        for (k, v) in &e.metadata {
            attrs.push(attr_str(&format!("gateway.metadata.{k}"), v));
        }
        let err = e.is_error();
        let mut root_span = json!({
            "traceId": trace_id, "spanId": root,
            "name": format!("{op} {}", e.route.as_deref().unwrap_or("-")),
            "kind": 2,
            "startTimeUnixNano": start.to_string(), "endTimeUnixNano": end.to_string(),
            "attributes": attrs,
            "status": {"code": if err { 2 } else { 1 }, "message": e.error.clone().unwrap_or_default()},
        });
        if let Some(parent) = &e.parent_span_id {
            root_span["parentSpanId"] = json!(parent);
        }
        spans.push(root_span);
        for a in &e.attempts {
            let s = (e.ts_ms + a.start_ms) * 1_000_000;
            let f = (e.ts_ms + a.start_ms + a.latency_ms) * 1_000_000;
            spans.push(json!({
                "traceId": trace_id, "spanId": random_span_id(), "parentSpanId": root,
                "name": format!("{op} {}/{}", a.provider, a.model),
                "kind": 3,
                "startTimeUnixNano": s.to_string(), "endTimeUnixNano": f.to_string(),
                "attributes": [
                    attr_str("gen_ai.system", &a.provider),
                    attr_str("gen_ai.request.model", &a.model),
                    attr_int("http.response.status_code", u64::from(a.status)),
                ],
                "status": {"code": if a.status >= 400 || a.status == 0 { 2 } else { 1 }, "message": a.error.clone().unwrap_or_default()},
            }));
        }
    }
    json!({"resourceSpans": [{
        "resource": {"attributes": [attr_str("service.name", service)]},
        "scopeSpans": [{"scope": {"name": "ai-gateway", "version": env!("CARGO_PKG_VERSION")}, "spans": spans}]
    }]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::AttemptLog;

    #[test]
    fn traceparent() {
        let (t, s) = parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert_eq!(t, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(s, "00f067aa0ba902b7");
        assert!(parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none());
        assert!(parse_traceparent("garbage").is_none());
    }

    #[test]
    fn otlp_has_root_and_attempt_spans() {
        let e = LogEntry {
            id: "11111111-2222-3333-4444-555555555555".into(),
            ts_ms: 1000,
            latency_ms: 50,
            tenant: "acme".into(),
            endpoint: "chat".into(),
            status: 200,
            route: Some("smart".into()),
            attempts: vec![AttemptLog {
                provider: "p".into(),
                model: "m".into(),
                status: 500,
                start_ms: 1,
                latency_ms: 10,
                error: Some("boom".into()),
            }],
            ..Default::default()
        };
        let v = otlp_payload("svc", &[e]);
        let spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["traceId"], "11111111222233334444555555555555");
        assert_eq!(spans[1]["parentSpanId"], spans[0]["spanId"]);
        assert_eq!(spans[1]["status"]["code"], 2);
    }
}
