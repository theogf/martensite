use std::collections::HashMap;
use crate::protocol::{self, TransportMode};

/// A free-memory threshold, either a fraction of total or an absolute byte count.
#[derive(Clone, Copy, Debug)]
pub enum MemThreshold {
    Fraction(f64), // 0..1
    Bytes(u64),
}

impl MemThreshold {
    pub fn satisfied(&self, avail: u64, total: u64) -> bool {
        match self {
            MemThreshold::Fraction(f) => (avail as f64) < f * (total as f64),
            MemThreshold::Bytes(b) => avail < *b,
        }
    }

    fn below(&self, other: &MemThreshold) -> bool {
        match (self, other) {
            (MemThreshold::Fraction(lf), MemThreshold::Fraction(hf)) => lf < hf,
            (MemThreshold::Bytes(lb), MemThreshold::Bytes(hb)) => lb < hb,
            // Mixed %/bytes units can't be compared without total memory; trust the operator.
            _ => true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub socket_path: String,
    pub runtime_dir: String,
    pub transport: TransportMode,
    pub bind_address: String,
    pub worker_executable: String,
    pub worker_args: String,
    pub worker_project: String,
    pub worker_maxclients: u32,
    pub min_ttl: u64, // seconds - protected floor: idle workers younger than this are never culled under pressure
    pub max_ttl: u64, // seconds - idle deadline: workers idle longer are always culled (supersedes WORKER_TTL)
    pub label_ttl: u64,
    pub ping_interval: u64,
    pub ping_timeout: u64,
    pub memory_pressure: bool, // master switch for pressure-reactive eviction
    pub psi_threshold: f64, // PSI some-avg10 % for moderate pressure (when PSI is the active source)
    pub memfree_low: MemThreshold, // free-memory enter threshold (when level path is active)
    pub memfree_high: MemThreshold, // free-memory exit threshold (must exceed memfree_low)
    pub port_range: Option<(u16, u16)>,
    pub host_home: String,
    pub sandbox_remote_clients: bool,
    pub sandbox_empty_environment: bool,
    pub sandbox_max_memory: Option<String>,
    pub sandbox_max_cpu: Option<u32>,
    pub sandbox_session_bypass: bool,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let env: HashMap<String, String> = std::env::vars().collect();
        let get = |k: &str| env.get(k).map(|s| s.as_str());

        let worker_project = env.get("JULIA_DAEMON_WORKER_PROJECT")
            .cloned()
            .ok_or_else(|| {
                eprintln!("Error: JULIA_DAEMON_WORKER_PROJECT environment variable is not set.");
                eprintln!("This should point to the DaemonWorker project directory.");
                eprintln!("Run DaemonicCabal.install() to set up the daemon correctly.");
                "MissingWorkerProject".to_string()
            })?;

        let runtime_dir = if let Some(r) = env.get("JULIA_DAEMON_RUNTIME") {
            r.clone()
        } else {
            protocol::default_runtime_dir(
                env.get("XDG_RUNTIME_DIR").map(|s| s.as_str()),
                env.get("HOME").map(|s| s.as_str()),
            )
        };

        let server_env = env.get("JULIA_DAEMON_SERVER");
        let raw_addr = server_env.cloned()
            .unwrap_or_else(|| format!("{}/conductor.sock", runtime_dir));
        let parsed = protocol::parse_address(&raw_addr)
            .map_err(|e| format!("Error in JULIA_DAEMON_SERVER: {}", e))?;

        let transport = parsed.mode;
        let socket_path = parsed.addr.clone();

        let bind_address = if let Some(b) = env.get("JULIA_DAEMON_BIND") {
            b.clone()
        } else if transport == TransportMode::Tcp {
            if let Some(pos) = socket_path.rfind(':') {
                socket_path[..pos].to_string()
            } else {
                "0.0.0.0".to_string()
            }
        } else {
            String::new()
        };

        let port_range = if transport == TransportMode::Tcp {
            env.get("JULIA_DAEMON_PORTS")
                .and_then(|s| protocol::parse_port_range(s))
        } else {
            None
        };

        let cfg = Config {
            socket_path,
            runtime_dir,
            transport,
            bind_address,
            worker_executable: get("JULIA_DAEMON_WORKER_EXECUTABLE")
                .unwrap_or("julia").to_string(),
            worker_args: get("JULIA_DAEMON_WORKER_ARGS")
                .unwrap_or("--startup-file=no").to_string(),
            worker_project,
            worker_maxclients: parse_uint(get("JULIA_DAEMON_WORKER_MAXCLIENTS"), 1),
            min_ttl: parse_uint_strict(get("JULIA_DAEMON_MIN_TTL"), 120)?,
            // max_ttl supersedes WORKER_TTL; fall back to it so existing service files keep working.
            max_ttl: parse_uint_strict(
                get("JULIA_DAEMON_MAX_TTL"),
                parse_uint(get("JULIA_DAEMON_WORKER_TTL"), 7200),
            )?,
            label_ttl: parse_uint(get("JULIA_DAEMON_LABEL_TTL"), 90),
            ping_interval: parse_uint(get("JULIA_DAEMON_PING_INTERVAL"), 30),
            ping_timeout: parse_uint(get("JULIA_DAEMON_PING_TIMEOUT"), 5),
            // On by default; set JULIA_DAEMON_MEMORY_PRESSURE=0 to opt out.
            memory_pressure: get("JULIA_DAEMON_MEMORY_PRESSURE").unwrap_or("1") != "0",
            psi_threshold: parse_float_strict(get("JULIA_DAEMON_PSI_THRESHOLD"), 10.0)?,
            memfree_low: parse_mem_threshold(get("JULIA_DAEMON_MEMFREE_LOW"), MemThreshold::Fraction(0.10))?,
            memfree_high: parse_mem_threshold(get("JULIA_DAEMON_MEMFREE_HIGH"), MemThreshold::Fraction(0.15))?,
            port_range,
            host_home: get("HOME").unwrap_or("").to_string(),
            sandbox_remote_clients: get("JULIA_DAEMON_SANDBOX_REMOTE_CLIENTS")
                .unwrap_or("1") != "0",
            sandbox_empty_environment: get("JULIA_DAEMON_SANDBOX_EMPTY_ENVIRONMENT")
                .unwrap_or("1") != "0",
            sandbox_max_memory: env.get("JULIA_DAEMON_SANDBOX_MAX_MEMORY").cloned(),
            sandbox_max_cpu: env.get("JULIA_DAEMON_SANDBOX_MAX_CPU")
                .and_then(|s| s.parse().ok()),
            sandbox_session_bypass: get("JULIA_DAEMON_SANDBOX_SESSION_BYPASS")
                .unwrap_or("0") == "1",
        };

        if cfg.min_ttl == 0 || cfg.min_ttl >= cfg.max_ttl {
            return Err(format!(
                "JULIA_DAEMON_MIN_TTL ({}) must be > 0 and < MAX_TTL ({}).",
                cfg.min_ttl, cfg.max_ttl
            ));
        }
        if !cfg.memfree_low.below(&cfg.memfree_high) {
            return Err("JULIA_DAEMON_MEMFREE_LOW must be < MEMFREE_HIGH.".to_string());
        }

        Ok(cfg)
    }
}

fn parse_uint<T: std::str::FromStr>(s: Option<&str>, default: T) -> T {
    s.and_then(|v| v.parse().ok()).unwrap_or(default)
}

// Strict variants abort on a malformed value (vs parse_uint's silent default) —
// for eviction knobs where a typo shouldn't quietly pass.
fn parse_uint_strict<T: std::str::FromStr>(s: Option<&str>, default: T) -> Result<T, String> {
    match s {
        None => Ok(default),
        Some(str) => str.parse().map_err(|_| format!("invalid integer config value '{}'.", str)),
    }
}

fn parse_float_strict(s: Option<&str>, default: f64) -> Result<f64, String> {
    match s {
        None => Ok(default),
        Some(str) => str.parse().map_err(|_| format!("invalid float config value '{}'.", str)),
    }
}

// A memory threshold is "<n>%" (fraction of total) or a byte count with an
// optional K/M/G suffix (e.g. "2G", "512M").
fn parse_mem_threshold(s: Option<&str>, default: MemThreshold) -> Result<MemThreshold, String> {
    let str = match s {
        None => return Ok(default),
        Some(s) => s,
    };
    if let Some(pct) = str.strip_suffix('%') {
        let pct: f64 = pct.parse().map_err(|_| format!("invalid memory threshold '{}'.", str))?;
        return Ok(MemThreshold::Fraction(pct / 100.0));
    }
    let (num_str, mult) = match str.chars().last() {
        Some('G') | Some('g') => (&str[..str.len() - 1], 1u64 << 30),
        Some('M') | Some('m') => (&str[..str.len() - 1], 1u64 << 20),
        Some('K') | Some('k') => (&str[..str.len() - 1], 1u64 << 10),
        _ => (str, 1u64),
    };
    let n: u64 = num_str.parse().map_err(|_| format!("invalid memory threshold '{}'.", str))?;
    Ok(MemThreshold::Bytes(n * mult))
}
