//! Per-provider circuit breaker.
//!
//! Closed: traffic flows, consecutive failures are counted.
//! Open: traffic is rejected until `open_for` passes.
//! Half-open: one probe request is let through; its outcome closes or
//! re-opens the circuit. If the probe never reports back (the client
//! disconnected, the request was cancelled), the slot expires after
//! `probe_timeout` so a provider can never be locked out permanently.
//!
//! Breakers are per instance on purpose: each replica judges provider health
//! from its own traffic, which needs no coordination and fails safe.

use std::time::{Duration, Instant};

use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed { failures: u32 },
    Open { until: Instant },
    HalfOpen { probe_started: Option<Instant> },
}

pub struct CircuitBreaker {
    name: String,
    threshold: u32,
    open_for: Duration,
    probe_timeout: Duration,
    state: Mutex<State>,
}

impl CircuitBreaker {
    pub fn new(name: &str, threshold: u32, open_for: Duration, probe_timeout: Duration) -> Self {
        metrics::gauge!("gateway_circuit_open", "provider" => name.to_string()).set(0.0);
        Self {
            name: name.to_string(),
            threshold: threshold.max(1),
            open_for,
            probe_timeout,
            state: Mutex::new(State::Closed { failures: 0 }),
        }
    }

    /// Whether a request may be sent now. In half-open state only one probe
    /// is in flight at a time; every caller that gets `true` should report an
    /// outcome, but a lost report only delays recovery by `probe_timeout`.
    pub fn allow(&self) -> bool {
        let now = Instant::now();
        let mut s = self.state.lock();
        match *s {
            State::Closed { .. } => true,
            State::Open { until } => {
                if now >= until {
                    *s = State::HalfOpen { probe_started: Some(now) };
                    tracing::info!(provider = %self.name, "circuit half-open, sending probe");
                    true
                } else {
                    false
                }
            }
            State::HalfOpen { probe_started } => match probe_started {
                Some(started) if now.duration_since(started) < self.probe_timeout => false,
                _ => {
                    *s = State::HalfOpen { probe_started: Some(now) };
                    true
                }
            },
        }
    }

    pub fn record_success(&self) {
        let mut s = self.state.lock();
        if !matches!(*s, State::Closed { .. }) {
            tracing::info!(provider = %self.name, "circuit closed");
            metrics::gauge!("gateway_circuit_open", "provider" => self.name.clone()).set(0.0);
        }
        *s = State::Closed { failures: 0 };
    }

    pub fn record_failure(&self) {
        let mut s = self.state.lock();
        let open = match *s {
            State::Closed { failures } => {
                let failures = failures + 1;
                if failures >= self.threshold {
                    true
                } else {
                    *s = State::Closed { failures };
                    false
                }
            }
            State::HalfOpen { .. } => true,
            State::Open { .. } => false,
        };
        if open {
            *s = State::Open { until: Instant::now() + self.open_for };
            tracing::warn!(provider = %self.name, open_secs = self.open_for.as_secs(), "circuit opened");
            metrics::gauge!("gateway_circuit_open", "provider" => self.name.clone()).set(1.0);
        }
    }

    /// An outcome that says nothing about provider health (client error,
    /// 401, 429, a request we could not build). Leaves the failure count
    /// alone and frees a half-open probe slot.
    pub fn record_neutral(&self) {
        let mut s = self.state.lock();
        if let State::HalfOpen { .. } = *s {
            *s = State::HalfOpen { probe_started: None };
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(*self.state.lock(), State::Open { until } if Instant::now() < until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker(threshold: u32, open_ms: u64) -> CircuitBreaker {
        CircuitBreaker::new("t", threshold, Duration::from_millis(open_ms), Duration::from_secs(60))
    }

    #[test]
    fn opens_after_threshold_and_recovers() {
        let b = breaker(2, 30);
        assert!(b.allow());
        b.record_failure();
        assert!(b.allow());
        b.record_failure();
        assert!(!b.allow());
        assert!(b.is_open());

        std::thread::sleep(Duration::from_millis(40));
        assert!(b.allow(), "probe allowed after open period");
        assert!(!b.allow(), "only one probe at a time");
        b.record_success();
        assert!(b.allow());
        assert!(b.allow());
    }

    #[test]
    fn failed_probe_reopens() {
        let b = breaker(1, 20);
        b.record_failure();
        std::thread::sleep(Duration::from_millis(25));
        assert!(b.allow());
        b.record_failure();
        assert!(!b.allow());
    }

    #[test]
    fn neutral_releases_probe() {
        let b = breaker(1, 10);
        b.record_failure();
        std::thread::sleep(Duration::from_millis(15));
        assert!(b.allow());
        b.record_neutral();
        assert!(b.allow());
    }

    #[test]
    fn lost_probe_expires() {
        let b = CircuitBreaker::new("t", 1, Duration::from_millis(10), Duration::from_millis(30));
        b.record_failure();
        std::thread::sleep(Duration::from_millis(15));
        assert!(b.allow()); // probe sent, outcome never reported
        assert!(!b.allow());
        std::thread::sleep(Duration::from_millis(35));
        assert!(b.allow(), "abandoned probe slot is reclaimed");
    }

    #[test]
    fn neutral_does_not_reset_failures() {
        let b = breaker(2, 10_000);
        b.record_failure();
        b.record_neutral();
        b.record_failure();
        assert!(!b.allow());
    }

    #[test]
    fn success_resets_failure_count() {
        let b = breaker(2, 10_000);
        b.record_failure();
        b.record_success();
        b.record_failure();
        assert!(b.allow());
    }
}
