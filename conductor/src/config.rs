use std::collections::HashMap;
use crate::protocol::{self, TransportMode, ParsedAddress};

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
    pub worker_ttl: u64,
    pub label_ttl: u64,
    pub ping_interval: u64,
    pub ping_timeout: u64,
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

        Ok(Config {
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
            worker_ttl: parse_uint(get("JULIA_DAEMON_WORKER_TTL"), 7200),
            label_ttl: parse_uint(get("JULIA_DAEMON_LABEL_TTL"), 90),
            ping_interval: parse_uint(get("JULIA_DAEMON_PING_INTERVAL"), 30),
            ping_timeout: parse_uint(get("JULIA_DAEMON_PING_TIMEOUT"), 5),
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
        })
    }
}

fn parse_uint<T: std::str::FromStr>(s: Option<&str>, default: T) -> T {
    s.and_then(|v| v.parse().ok()).unwrap_or(default)
}
