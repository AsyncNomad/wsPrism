//! Minimal metrics registry for the gateway.
//!
//! No external dependencies are used; this module provides counter/gauge/histogram
//! types with dynamic labels backed by `DashMap`. Labels are flattened into
//! sorted key vectors to keep deterministic ordering. Histogram buckets are
//! fixed in microseconds to avoid floating point math.

use dashmap::DashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

/// Helper to escape label values.
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[derive(Default)]
pub struct CounterVec {
    map: DashMap<Vec<(String, String)>, AtomicU64>,
}

impl CounterVec {
    /// Increment by 1.
    pub fn inc(&self, labels: &[(&str, &str)]) {
        self.add(labels, 1);
    }

    /// Increment by an arbitrary value.
    pub fn add(&self, labels: &[(&str, &str)], v: u64) {
        let mut key: Vec<(String, String)> = labels.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        key.sort();
        
        let counter = self.map.entry(key).or_insert_with(|| AtomicU64::new(0));
        counter.fetch_add(v, Ordering::Relaxed);
    }

    /// Render in Prometheus text exposition format.
    fn render(&self, name: &str, out: &mut String) {
        let _ = writeln!(out, "# TYPE {} counter", name);
        for r in self.map.iter() {
            let key = r.key();
            let val = r.value().load(Ordering::Relaxed);
            let label_str = key.iter()
                .map(|(k, v)| format!("{}=\"{}\"", k, escape_label(v)))
                .collect::<Vec<_>>().join(",");
            let _ = writeln!(out, "{}{{{}}} {}", name, label_str, val);
        }
    }
}

#[derive(Default)]
pub struct GaugeVec {
    map: DashMap<Vec<(String, String)>, AtomicI64>,
}

impl GaugeVec {
    /// Increment by 1.
    pub fn inc(&self, labels: &[(&str, &str)]) { self.add(labels, 1); }
    /// Decrement by 1.
    pub fn dec(&self, labels: &[(&str, &str)]) { self.add(labels, -1); }

    /// Add an arbitrary signed delta.
    pub fn add(&self, labels: &[(&str, &str)], v: i64) {
        let mut key: Vec<(String, String)> = labels.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        key.sort();
        let gauge = self.map.entry(key).or_insert_with(|| AtomicI64::new(0));
        gauge.fetch_add(v, Ordering::Relaxed);
    }

    /// Render in Prometheus text exposition format.
    fn render(&self, name: &str, out: &mut String) {
        let _ = writeln!(out, "# TYPE {} gauge", name);
        for r in self.map.iter() {
            let key = r.key();
            let val = r.value().load(Ordering::Relaxed);
            let label_str = key.iter()
                .map(|(k, v)| format!("{}=\"{}\"", k, escape_label(v)))
                .collect::<Vec<_>>().join(",");
            let _ = writeln!(out, "{}{{{}}} {}", name, label_str, val);
        }
    }
}

// Fixed Buckets in Microseconds (µs)
// 100us, 500us, 1ms, 5ms, 10ms, 50ms, 100ms, 500ms, 1s
const BUCKETS_MICROS: [u64; 9] = [100, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000];

struct AtomicHistogram {
    count: AtomicU64,
    sum: AtomicU64,
    buckets: [AtomicU64; 9],
}

impl Default for AtomicHistogram {
    fn default() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            buckets: [
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)
            ],
        }
    }
}

#[derive(Default)]
pub struct HistogramVec {
    map: DashMap<Vec<(String, String)>, AtomicHistogram>,
}

impl HistogramVec {
    /// Observe a duration and increment cumulative buckets (microsecond scale).
    pub fn observe(&self, labels: &[(&str, &str)], duration: Duration) {
        let mut key: Vec<(String, String)> = labels.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        key.sort();

        let hist = self.map.entry(key).or_insert_with(AtomicHistogram::default);
        let micros = duration.as_micros() as u64;

        hist.count.fetch_add(1, Ordering::Relaxed);
        hist.sum.fetch_add(micros, Ordering::Relaxed); // Record sum in micros

        // Cumulative Buckets: Increment ALL buckets larger than value
        for (i, &b) in BUCKETS_MICROS.iter().enumerate() {
            if micros <= b {
                hist.buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Render in Prometheus text exposition format (unit: microseconds).
    fn render(&self, name: &str, out: &mut String) {
        let _ = writeln!(out, "# TYPE {} histogram", name);
        for r in self.map.iter() {
            let key = r.key();
            let hist = r.value();
            
            let label_str = key.iter()
                .map(|(k, v)| format!("{}=\"{}\"", k, escape_label(v)))
                .collect::<Vec<_>>().join(",");
            let prefix = if label_str.is_empty() { String::new() } else { format!("{},", label_str) };

            for (i, &le) in BUCKETS_MICROS.iter().enumerate() {
                let count = hist.buckets[i].load(Ordering::Relaxed);
                // Convert bucket le from micros to seconds string for standard Prometheus display? 
                // Or just keep int? Standard is seconds (float).
                // For simplicity in this int-based impl, we output micros but should ideally label unit.
                // Let's output integer micros for 'le'.
                let _ = writeln!(out, "{}_bucket{{{}le=\"{}\"}} {}", name, prefix, le, count);
            }
            let count = hist.count.load(Ordering::Relaxed);
            let _ = writeln!(out, "{}_bucket{{{}le=\"+Inf\"}} {}", name, prefix, count);
            
            let sum = hist.sum.load(Ordering::Relaxed);
            let _ = writeln!(out, "{}_sum{{{}}} {}", name, label_str, sum);
            let _ = writeln!(out, "{}_count{{{}}} {}", name, label_str, count);
        }
    }
}

#[derive(Default)]
pub struct GatewayMetrics {
    pub ws_upgrades: CounterVec,
    pub ws_active_sessions: GaugeVec,
    pub policy_decisions: CounterVec,
    pub handshake_rejections: CounterVec,
    pub dispatch_duration: HistogramVec, // In Microseconds
    pub decode_errors: CounterVec,
    pub service_errors: CounterVec,
    pub writer_timeouts: CounterVec,
    pub unknown_service_errors: CounterVec,
    draining: std::sync::atomic::AtomicBool,
}

impl GatewayMetrics {
    /// Mark draining state.
    pub fn set_draining(&self) { self.draining.store(true, Ordering::Relaxed); }
    /// Return whether draining is active.
    pub fn is_draining(&self) -> bool { self.draining.load(Ordering::Relaxed) }

    /// Render all registered metrics plus any extra lines provided by callers.
    pub fn render(&self, extra: &[(&str, u64)]) -> String {
        let mut out = String::new();
        self.ws_upgrades.render("wsprism_ws_upgrades_total", &mut out);
        self.ws_active_sessions.render("wsprism_ws_sessions_active", &mut out);
        self.policy_decisions.render("wsprism_policy_decisions_total", &mut out);
        self.handshake_rejections.render("wsprism_handshake_rejections_total", &mut out);
        self.dispatch_duration.render("wsprism_dispatch_duration_micros", &mut out); // Explicit unit
        self.decode_errors.render("wsprism_decode_errors_total", &mut out);
        self.service_errors.render("wsprism_service_errors_total", &mut out);
        self.writer_timeouts.render("wsprism_writer_timeouts_total", &mut out);
        self.unknown_service_errors.render("wsprism_unknown_service_total", &mut out);
        
        let _ = writeln!(out, "# TYPE wsprism_draining gauge\nwsprism_draining {}", if self.is_draining() { 1 } else { 0 });
        for (k, v) in extra { let _ = writeln!(out, "{} {}", k, v); }
        out
    }
}
