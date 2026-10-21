//! Minimal metrics registry for the gateway.
//!
//! No external dependencies are used; this module provides counter/gauge/histogram
//! types with dynamic labels backed by `DashMap`. Labels are flattened into
//! sorted key vectors to keep deterministic ordering. Histograms count in
//! microseconds (integer atomics) and are exposed in seconds, the
//! Prometheus base unit (M6). Every family has `# HELP` and `# TYPE`.

use dashmap::DashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Sorted owned label key.
fn label_key(labels: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut key: Vec<(String, String)> = labels
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    key.sort();
    key
}

/// The value for `labels`, created if missing. Looks up under the shard
/// *read* lock first, so steady-state updates of the same series don't
/// serialize on a write lock.
fn series<V: Default>(
    map: &DashMap<Vec<(String, String)>, Arc<V>>,
    labels: &[(&str, &str)],
) -> Arc<V> {
    let key = label_key(labels);
    if let Some(v) = map.get(&key) {
        return Arc::clone(v.value());
    }
    Arc::clone(map.entry(key).or_default().value())
}

/// `# HELP` and `# TYPE` of a family.
fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// `k="v",...` (escaped).
fn label_text(key: &[(String, String)]) -> String {
    key.iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, escape_label(v)))
        .collect::<Vec<_>>()
        .join(",")
}

/// One sample line; no braces when there are no labels.
fn sample(out: &mut String, name: &str, labels: &str, value: impl std::fmt::Display) {
    if labels.is_empty() {
        let _ = writeln!(out, "{name} {value}");
    } else {
        let _ = writeln!(out, "{name}{{{labels}}} {value}");
    }
}

/// Helper to escape label values.
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[derive(Default)]
pub struct CounterVec {
    map: DashMap<Vec<(String, String)>, Arc<AtomicU64>>,
}

impl CounterVec {
    /// Increment by 1.
    pub fn inc(&self, labels: &[(&str, &str)]) {
        self.add(labels, 1);
    }

    /// Increment by an arbitrary value.
    pub fn add(&self, labels: &[(&str, &str)], v: u64) {
        series(&self.map, labels).fetch_add(v, Ordering::Relaxed);
    }

    /// Render in Prometheus text exposition format.
    fn render(&self, name: &str, help: &str, out: &mut String) {
        header(out, name, help, "counter");
        for r in self.map.iter() {
            sample(
                out,
                name,
                &label_text(r.key()),
                r.value().load(Ordering::Relaxed),
            );
        }
    }
}

#[derive(Default)]
pub struct GaugeVec {
    map: DashMap<Vec<(String, String)>, Arc<AtomicI64>>,
}

impl GaugeVec {
    /// Increment by 1.
    pub fn inc(&self, labels: &[(&str, &str)]) {
        self.add(labels, 1);
    }
    /// Decrement by 1.
    pub fn dec(&self, labels: &[(&str, &str)]) {
        self.add(labels, -1);
    }

    /// Add an arbitrary signed delta.
    pub fn add(&self, labels: &[(&str, &str)], v: i64) {
        series(&self.map, labels).fetch_add(v, Ordering::Relaxed);
    }

    /// Render in Prometheus text exposition format.
    fn render(&self, name: &str, help: &str, out: &mut String) {
        header(out, name, help, "gauge");
        for r in self.map.iter() {
            sample(
                out,
                name,
                &label_text(r.key()),
                r.value().load(Ordering::Relaxed),
            );
        }
    }
}

// Fixed Buckets in Microseconds (µs)
// 100us, 500us, 1ms, 5ms, 10ms, 50ms, 100ms, 500ms, 1s
const BUCKETS_MICROS: [u64; 9] = [
    100, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000,
];

pub struct AtomicHistogram {
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
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }
}

#[derive(Default)]
pub struct HistogramVec {
    map: DashMap<Vec<(String, String)>, Arc<AtomicHistogram>>,
}

/// One histogram series, resolved once (e.g. per session) so observing it
/// is just atomic adds — no label allocation, no map lookup.
#[derive(Clone)]
pub struct Histogram(Arc<AtomicHistogram>);

impl Histogram {
    /// Observe a duration (microsecond buckets, cumulative).
    pub fn observe(&self, duration: Duration) {
        let hist = &self.0;
        let micros = duration.as_micros() as u64;
        hist.count.fetch_add(1, Ordering::Relaxed);
        hist.sum.fetch_add(micros, Ordering::Relaxed);
        // One bucket per observation (its own range); render() accumulates.
        if let Some(i) = BUCKETS_MICROS.iter().position(|&b| micros <= b) {
            hist.buckets[i].fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl HistogramVec {
    /// The series for `labels`, to observe repeatedly without lookups.
    pub fn handle(&self, labels: &[(&str, &str)]) -> Histogram {
        Histogram(series(&self.map, labels))
    }

    /// Render in Prometheus text exposition format, in seconds (counted
    /// in microseconds).
    fn render(&self, name: &str, help: &str, out: &mut String) {
        header(out, name, help, "histogram");
        for r in self.map.iter() {
            let labels = label_text(r.key());
            let hist = r.value();
            let with = |le: &str| {
                if labels.is_empty() {
                    format!("le=\"{le}\"")
                } else {
                    format!("{labels},le=\"{le}\"")
                }
            };
            let bucket = format!("{name}_bucket");
            let mut cumulative = 0;
            for (i, &le) in BUCKETS_MICROS.iter().enumerate() {
                cumulative += hist.buckets[i].load(Ordering::Relaxed);
                sample(out, &bucket, &with(&seconds(le)), cumulative);
            }
            let count = hist.count.load(Ordering::Relaxed);
            sample(out, &bucket, &with("+Inf"), count);
            let sum = hist.sum.load(Ordering::Relaxed);
            sample(out, &format!("{name}_sum"), &labels, seconds(sum));
            sample(out, &format!("{name}_count"), &labels, count);
        }
    }
}

/// Microseconds as decimal seconds, exactly (`500` -> `0.0005`).
fn seconds(micros: u64) -> String {
    let (whole, frac) = (micros / 1_000_000, micros % 1_000_000);
    if frac == 0 {
        return whole.to_string();
    }
    let digits = format!("{frac:06}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

#[derive(Default)]
pub struct GatewayMetrics {
    pub ws_upgrades: CounterVec,
    pub ws_active_sessions: GaugeVec,
    pub policy_decisions: CounterVec,
    pub handshake_rejections: CounterVec,
    pub dispatch_duration: HistogramVec,
    pub decode_errors: CounterVec,
    pub service_errors: CounterVec,
    pub writer_timeouts: CounterVec,
    draining: std::sync::atomic::AtomicBool,
}

impl GatewayMetrics {
    /// Mark draining state.
    pub fn set_draining(&self) {
        self.draining.store(true, Ordering::Relaxed);
    }
    /// Return whether draining is active.
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    /// Render all registered metrics plus counters kept elsewhere
    /// (`extra`: name, help, value).
    pub fn render(&self, extra: &[(&str, &str, u64)]) -> String {
        let mut out = String::new();
        self.ws_upgrades.render(
            "wsprism_ws_upgrades_total",
            "WebSocket upgrades accepted.",
            &mut out,
        );
        self.ws_active_sessions.render(
            "wsprism_ws_sessions_active",
            "Sessions open now.",
            &mut out,
        );
        self.policy_decisions.render(
            "wsprism_policy_decisions_total",
            "Messages and sessions a tenant policy refused or limited.",
            &mut out,
        );
        self.handshake_rejections.render(
            "wsprism_handshake_rejections_total",
            "Connections refused before the upgrade, by reason.",
            &mut out,
        );
        self.dispatch_duration.render(
            "wsprism_dispatch_duration_seconds",
            "Time to handle one client message, by lane.",
            &mut out,
        );
        self.decode_errors.render(
            "wsprism_decode_errors_total",
            "Client frames that could not be decoded.",
            &mut out,
        );
        self.service_errors.render(
            "wsprism_service_errors_total",
            "Client messages a service answered with an error.",
            &mut out,
        );
        self.writer_timeouts.render(
            "wsprism_writer_timeouts_total",
            "Sessions closed because a write to the socket timed out.",
            &mut out,
        );
        header(
            &mut out,
            "wsprism_draining",
            "1 while the gateway drains for shutdown.",
            "gauge",
        );
        sample(
            &mut out,
            "wsprism_draining",
            "",
            u8::from(self.is_draining()),
        );
        for (name, help, v) in extra {
            header(&mut out, name, help, "counter");
            sample(&mut out, name, "", v);
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn histogram_renders_cumulative_buckets() {
        let h = HistogramVec::default();
        let s = h.handle(&[("lane", "ext")]);
        s.observe(Duration::from_micros(50)); // <= 100
        s.observe(Duration::from_micros(700)); // <= 1000
        s.observe(Duration::from_secs(5)); // +Inf only
        let mut out = String::new();
        h.render("d", "help", &mut out);
        assert!(
            out.contains(r#"d_bucket{lane="ext",le="0.0001"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"d_bucket{lane="ext",le="0.0005"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"d_bucket{lane="ext",le="0.001"} 2"#),
            "{out}"
        );
        assert!(out.contains(r#"d_bucket{lane="ext",le="1"} 2"#), "{out}");
        assert!(out.contains(r#"d_bucket{lane="ext",le="+Inf"} 3"#), "{out}");
        assert!(out.contains(r#"d_sum{lane="ext"} 5.00075"#), "{out}");
        assert!(out.contains(r#"d_count{lane="ext"} 3"#), "{out}");
    }

    /// Every family has HELP and TYPE before its samples; label-less samples
    /// have no braces (M6).
    #[test]
    fn exposition_is_well_formed() {
        let m = GatewayMetrics::default();
        m.ws_upgrades.inc(&[("tenant", "a"), ("status", "ok")]);
        m.writer_timeouts.inc(&[]);
        m.dispatch_duration
            .handle(&[])
            .observe(Duration::from_micros(10));
        let out = m.render(&[("wsprism_x_total", "An extra counter.", 7)]);
        let mut declared = std::collections::HashSet::new();
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                declared.insert(rest.split(' ').next().unwrap().to_string());
                continue;
            }
            if line.starts_with("# TYPE ") {
                continue;
            }
            let name = line.split(['{', ' ']).next().unwrap();
            let family = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|s| name.strip_suffix(s).filter(|f| declared.contains(*f)))
                .unwrap_or(name);
            assert!(declared.contains(family), "no HELP before {line}\n{out}");
            assert!(!line.contains("{}"), "{line}");
        }
        assert!(out.contains("wsprism_writer_timeouts_total 1\n"), "{out}");
        assert!(
            out.contains("# TYPE wsprism_x_total counter\nwsprism_x_total 7\n"),
            "{out}"
        );
        assert!(
            out.contains("wsprism_dispatch_duration_seconds_sum 0.00001\n"),
            "{out}"
        );
    }

    #[test]
    fn seconds_are_exact() {
        assert_eq!(seconds(0), "0");
        assert_eq!(seconds(100), "0.0001");
        assert_eq!(seconds(1_500_000), "1.5");
        assert_eq!(seconds(2_000_000), "2");
    }
}
