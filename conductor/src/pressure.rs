// Memory-pressure detection driving pressure-reactive worker eviction.
//
// Source resolved once at startup: prefer /proc/pressure/memory (PSI, Linux
// kernel >= 4.20 with CONFIG_PSI); fall back to /proc/meminfo's MemAvailable
// if PSI isn't present; otherwise pressure eviction is inactive and only the
// flat min_ttl/max_ttl idle-cull applies.
use crate::config::{Config, MemThreshold};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Source {
    Psi,
    MemFree,
    None,
}

pub struct PressureMonitor {
    source: Source,
    psi_threshold: f64,
    memfree_low: MemThreshold,
    memfree_high: MemThreshold,
    under_pressure: bool,
}

impl PressureMonitor {
    pub fn new(cfg: &Config) -> Self {
        let source = if !cfg.memory_pressure {
            Source::None
        } else if read_psi_avg10().is_some() {
            Source::Psi
        } else if read_meminfo().is_some() {
            Source::MemFree
        } else {
            Source::None
        };
        PressureMonitor {
            source,
            psi_threshold: cfg.psi_threshold,
            memfree_low: cfg.memfree_low,
            memfree_high: cfg.memfree_high,
            under_pressure: false,
        }
    }

    pub fn active(&self) -> bool {
        self.source != Source::None
    }

    /// Poll current pressure state; returns whether the system is currently
    /// considered under memory pressure. PSI has no hysteresis (the 10s
    /// average is already smooth); the free-memory fallback uses a two-band
    /// hysteresis (enter below memfree_low, exit only once past memfree_high)
    /// to avoid flapping evictions near a single threshold.
    pub fn poll(&mut self) -> bool {
        match self.source {
            Source::None => false,
            Source::Psi => {
                if let Some(avg10) = read_psi_avg10() {
                    self.under_pressure = avg10 >= self.psi_threshold;
                }
                self.under_pressure
            }
            Source::MemFree => {
                if let Some((avail, total)) = read_meminfo() {
                    if self.under_pressure {
                        if !self.memfree_high.satisfied(avail, total) { self.under_pressure = false; }
                    } else if self.memfree_low.satisfied(avail, total) {
                        self.under_pressure = true;
                    }
                }
                self.under_pressure
            }
        }
    }

    pub fn source_name(&self) -> &'static str {
        match self.source {
            Source::Psi => "psi",
            Source::MemFree => "memfree",
            Source::None => "none",
        }
    }
}

fn read_psi_avg10() -> Option<f64> {
    let content = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    let some_line = content.lines().find(|l| l.starts_with("some "))?;
    for tok in some_line.split_whitespace() {
        if let Some(v) = tok.strip_prefix("avg10=") {
            return v.parse().ok();
        }
    }
    None
}

/// Returns (available_bytes, total_bytes) from /proc/meminfo.
pub fn read_meminfo() -> Option<(u64, u64)> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut avail = None;
    let mut total = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb(rest);
        }
    }
    Some((avail?, total?))
}

fn parse_kb(s: &str) -> Option<u64> {
    let n: u64 = s.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(n * 1024)
}
