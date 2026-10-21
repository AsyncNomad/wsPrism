//! Server-side cost sampling from /proc (CPU time, resident memory).

/// Linux USER_HZ: `/proc/<pid>/stat` CPU times are in these ticks.
const CLK_TCK: f64 = 100.0;

#[derive(Debug, Clone, Copy)]
pub struct ProcSample {
    /// User + system CPU seconds consumed by the whole process so far.
    pub cpu_secs: f64,
    pub rss_kb: u64,
}

/// The sum over several processes (gateway nodes); `None` if any is gone
/// or `pids` is empty.
pub fn sample_all(pids: &[u32]) -> Option<ProcSample> {
    let mut total: Option<ProcSample> = None;
    for &pid in pids {
        let s = sample(pid)?;
        total = Some(match total {
            Some(t) => ProcSample {
                cpu_secs: t.cpu_secs + s.cpu_secs,
                rss_kb: t.rss_kb + s.rss_kb,
            },
            None => s,
        });
    }
    total
}

pub fn sample(pid: u32) -> Option<ProcSample> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name; utime/stime are fields 14/15.
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss_kb = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())?;
    Some(ProcSample {
        cpu_secs: (utime + stime) / CLK_TCK,
        rss_kb,
    })
}
