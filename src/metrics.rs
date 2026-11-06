//! Prometheus metrics.
//!
//! - gateway_requests_total{tenant,provider,status,cache}
//! - gateway_request_duration_seconds{provider,stream}   (histogram)
//! - gateway_tokens_total{tenant,provider,model,type}
//! - gateway_cost_microdollars_total{tenant}
//! - gateway_upstream_errors_total{provider,kind}
//! - gateway_upstream_skipped_total{provider}            (circuit open)
//! - gateway_circuit_open{provider}                      (gauge, 1 = open)
//! - gateway_rejected_total{reason}                      (overloaded, rate_limit, budget)
//! - gateway_cache_total{result}
//! - gateway_store_errors_total{op}

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

pub fn install() -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("gateway_request_duration_seconds".to_string()),
            &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0],
        )?
        .install_recorder()?;
    Ok(handle)
}
