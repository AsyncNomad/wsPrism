//! Age of Information: how old is the newest state a receiver holds?
//!
//! Sampled at a fixed render rate per tracked sender. Losses make the age
//! grow, so — unlike per-message latency, which only sees what arrived — the
//! metric is fair between reliable and unreliable transports.

use std::collections::HashMap;

use hdrhistogram::Histogram;

pub struct Aoi {
    /// Newest state timestamp (ns) received per sender.
    newest: HashMap<u32, u64>,
    /// AoI samples in microseconds.
    pub hist: Histogram<u64>,
    pub samples: u64,
    pub stalls: u64,
    stall_us: u64,
}

impl Aoi {
    pub fn new(stall_us: u64) -> Self {
        Self {
            newest: HashMap::new(),
            hist: new_hist(),
            samples: 0,
            stalls: 0,
            stall_us,
        }
    }

    /// A state from `sender` created at `ts_ns` arrived (out of order is fine).
    pub fn observe(&mut self, sender: u32, ts_ns: u64) {
        let e = self.newest.entry(sender).or_insert(0);
        *e = (*e).max(ts_ns);
    }

    /// Render tick at `now_ns`: record the age of every sender seen so far.
    pub fn sample(&mut self, now_ns: u64) {
        for &ts in self.newest.values() {
            let age_us = now_ns.saturating_sub(ts) / 1_000;
            let _ = self.hist.record(age_us.max(1));
            self.samples += 1;
            if age_us > self.stall_us {
                self.stalls += 1;
            }
        }
    }
}

pub fn new_hist() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).expect("histogram bounds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_updates_grow_the_age() {
        let mut a = Aoi::new(100_000);
        a.observe(1, 1_000_000_000); // t = 1.0 s
        a.sample(1_010_000_000); // 10 ms old
        a.sample(1_200_000_000); // nothing new for 200 ms: a stall
        assert_eq!(a.samples, 2);
        assert_eq!(a.stalls, 1);
        assert!(a.hist.max() >= 199_000);
    }

    #[test]
    fn reordered_older_state_does_not_rewind() {
        let mut a = Aoi::new(100_000);
        a.observe(1, 2_000_000_000);
        a.observe(1, 1_000_000_000); // late, older datagram
        a.sample(2_001_000_000);
        assert!(a.hist.max() < 2_000, "age must use the newest state");
    }
}
