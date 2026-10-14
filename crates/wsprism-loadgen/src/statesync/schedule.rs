//! When each client sends and renders.
//!
//! Real clients do not share a clock phase. If every client starts its send
//! and render timers at the same instant (and the rates match), each render
//! tick lands at the same offset from a send tick, so AoI collapses onto a
//! staircase (latency + k·period) and any percentile or threshold judged on
//! it depends on where the steps fall. `Random` gives each client its own
//! offsets within one period, from a seeded generator so runs repeat.

use std::time::Duration;

use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Phase {
    /// Per-client random send and render offsets (realistic; default).
    Random,
    /// Everyone starts at the same instant (reproduces pre-fix results).
    Aligned,
}

/// Start instants of one client's send and render timers.
#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    pub send_start: Instant,
    pub render_start: Instant,
}

impl Schedule {
    /// `send_at`/`render_at` are the aligned starts; random offsets are added
    /// within one `send_period`/`render_period`.
    pub fn new(
        phase: Phase,
        seed: u64,
        client: u32,
        (send_at, send_period): (Instant, Duration),
        (render_at, render_period): (Instant, Duration),
    ) -> Self {
        match phase {
            Phase::Aligned => Self {
                send_start: send_at,
                render_start: render_at,
            },
            Phase::Random => {
                let mut state = seed ^ (u64::from(client) << 32 | u64::from(client));
                Self {
                    send_start: send_at + send_period.mul_f64(unit(&mut state)),
                    render_start: render_at + render_period.mul_f64(unit(&mut state)),
                }
            }
        }
    }
}

/// Uniform in [0, 1) from SplitMix64 (no RNG dependency; quality is ample
/// for spreading phases).
fn unit(state: &mut u64) -> f64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Duration = Duration::from_micros(16_667);

    fn offsets(phase: Phase, seed: u64, client: u32) -> (Duration, Duration) {
        let t = Instant::now();
        let s = Schedule::new(phase, seed, client, (t, P), (t, P));
        (s.send_start - t, s.render_start - t)
    }

    #[test]
    fn aligned_has_no_offsets() {
        assert_eq!(
            offsets(Phase::Aligned, 1, 7),
            (Duration::ZERO, Duration::ZERO)
        );
    }

    #[test]
    fn random_is_within_one_period_and_repeatable() {
        for c in 0..1000 {
            let (s, r) = offsets(Phase::Random, 42, c);
            assert!(s < P && r < P);
            assert_eq!((s, r), offsets(Phase::Random, 42, c));
        }
    }

    #[test]
    fn random_spreads_phases() {
        // 1000 clients over 10 equal bins of the period: none should be empty
        // or hold more than twice its share.
        let mut send = [0u32; 10];
        let mut render = [0u32; 10];
        for c in 0..1000 {
            let (s, r) = offsets(Phase::Random, 7, c);
            send[(s.as_secs_f64() / P.as_secs_f64() * 10.0) as usize] += 1;
            render[(r.as_secs_f64() / P.as_secs_f64() * 10.0) as usize] += 1;
        }
        for n in send.iter().chain(&render) {
            assert!((20..=200).contains(n), "bins {send:?} {render:?}");
        }
    }

    #[test]
    fn send_and_render_offsets_are_independent() {
        // Not the same draw: otherwise phase-locking would survive per client.
        let same = (0..100)
            .filter(|&c| {
                let (s, r) = offsets(Phase::Random, 3, c);
                s == r
            })
            .count();
        assert_eq!(same, 0);
    }
}
