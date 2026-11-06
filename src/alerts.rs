//! Webhook alerts: limit thresholds and provider circuit-breaker changes.
//!
//! Each alert is a JSON POST to `alerts.webhook_url`. With a
//! `webhook_secret`, the body's HMAC-SHA256 is sent hex-encoded in
//! `x-gateway-signature` so receivers can verify it. Sending never blocks a
//! request; failures are logged and counted.

use std::time::Duration;

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::{app::AppState, config::LimitKind, limits::Applied, proxy::unix_secs};

pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Sends `event` in the background if a webhook is configured.
pub fn send(app: &AppState, event: Value) {
    let rt = app.rt();
    let Some(url) = rt.cfg.alerts.webhook_url.clone() else { return };
    let secret = rt.cfg.alerts.webhook_secret.as_ref().map(|s| s.expose().to_string());
    let http = app.http.clone();
    let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
    handle.spawn(async move {
        let body = serde_json::to_vec(&event).unwrap_or_default();
        let mut rb = http
            .post(&url)
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(10))
            .body(body.clone());
        if let Some(s) = secret {
            rb = rb.header("x-gateway-signature", sign(&s, &body));
        }
        match rb.send().await {
            Ok(r) if r.status().is_success() => {
                metrics::counter!("gateway_alerts_total", "result" => "sent").increment(1);
            }
            Ok(r) => {
                metrics::counter!("gateway_alerts_total", "result" => "failed").increment(1);
                tracing::warn!(status = r.status().as_u16(), "alert webhook rejected the alert");
            }
            Err(e) => {
                metrics::counter!("gateway_alerts_total", "result" => "failed").increment(1);
                tracing::warn!(error = %e, "alert webhook unreachable");
            }
        }
    });
}

/// Fires threshold alerts for limits whose usage crossed a configured
/// fraction, at most once per limit bucket and threshold.
pub async fn limit_crossings(app: &AppState, changes: &[(Applied, u64, u64)]) {
    let rt = app.rt();
    if rt.cfg.alerts.webhook_url.is_none() {
        return;
    }
    for (a, before, after) in changes {
        for t in &rt.cfg.alerts.thresholds {
            let line = (a.limit_units as f64 * t).ceil() as u64;
            if *before < line && *after >= line {
                let marker = format!("alert:{}:{}", a.bucket, (t * 100.0).round() as u64);
                // Only the first replica to cross it sends the alert.
                if app.store.incr(&marker, 1, a.ttl).await.unwrap_or(0) != 1 {
                    continue;
                }
                let scale =
                    |v: u64| if a.kind == LimitKind::Cost { v as f64 / 1_000_000.0 } else { v as f64 };
                send(
                    app,
                    json!({
                        "type": "limit.threshold",
                        "time": unix_secs(),
                        "limit": a.name,
                        "kind": a.kind,
                        "window": a.window,
                        "scope": a.scope,
                        "threshold": t,
                        "used": scale(*after),
                        "limit_value": scale(a.limit_units),
                    }),
                );
            }
        }
    }
}

pub fn circuit_opened(app: &AppState, provider: &str) {
    send(app, json!({"type": "provider.circuit_open", "time": unix_secs(), "provider": provider}));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_stable_hmac() {
        // RFC 4231 test case 2.
        assert_eq!(
            sign("Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
