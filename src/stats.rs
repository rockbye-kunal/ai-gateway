//! Recent performance per target (`provider/model`) on this replica:
//! exponentially weighted time to first token, output tokens per second and
//! success rate. Used to sort targets by latency or throughput and to show
//! provider health. Kept in memory on purpose: each replica reacts to what
//! it sees, like the circuit breakers.

use dashmap::DashMap;
use serde::Serialize;

const ALPHA: f64 = 0.2;

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct TargetStat {
    pub ttft_ms: f64,
    pub tokens_per_sec: f64,
    pub success_rate: f64,
    pub samples: u64,
}

#[derive(Default)]
pub struct TargetStats {
    by_target: DashMap<String, TargetStat>,
}

fn ewma(old: f64, new: f64, first: bool) -> f64 {
    if first {
        new
    } else {
        old + ALPHA * (new - old)
    }
}

impl TargetStats {
    pub fn record(&self, provider: &str, model: &str, ok: bool, ttft_ms: Option<u64>, tps: Option<f64>) {
        let mut e = self.by_target.entry(format!("{provider}/{model}")).or_default();
        let first = e.samples == 0;
        e.success_rate = ewma(e.success_rate, if ok { 1.0 } else { 0.0 }, first);
        if let Some(t) = ttft_ms.filter(|_| ok) {
            e.ttft_ms = ewma(e.ttft_ms, t as f64, first || e.ttft_ms == 0.0);
        }
        if let Some(t) = tps.filter(|t| ok && t.is_finite() && *t > 0.0) {
            e.tokens_per_sec = ewma(e.tokens_per_sec, t, first || e.tokens_per_sec == 0.0);
        }
        e.samples += 1;
    }

    pub fn get(&self, provider: &str, model: &str) -> Option<TargetStat> {
        self.by_target.get(&format!("{provider}/{model}")).map(|e| *e.value())
    }

    pub fn snapshot(&self) -> Vec<(String, TargetStat)> {
        let mut v: Vec<_> = self.by_target.iter().map(|e| (e.key().clone(), *e.value())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smooths_and_tracks_success() {
        let s = TargetStats::default();
        s.record("p", "m", true, Some(100), Some(50.0));
        s.record("p", "m", true, Some(200), Some(50.0));
        s.record("p", "m", false, None, None);
        let t = s.get("p", "m").unwrap();
        assert!((t.ttft_ms - 120.0).abs() < 1e-9);
        assert!(t.success_rate < 1.0 && t.success_rate > 0.7);
        assert_eq!(t.samples, 3);
        assert!(s.get("p", "other").is_none());
    }
}
