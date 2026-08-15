use std::io;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixListener;
use std::net::TcpListener;
use std::time::Duration;

use crate::args;
use crate::config::Config;
use crate::env_cache::{EnvCache, EnvVar};
use crate::project;
use crate::protocol::{self, TransportMode, PORT_POOL_NONE, PortPool};
use crate::status;
use crate::worker::{ClientInfo, Worker, SocketPaths, unix_time, unix_time_us};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn version_string() -> String { format!("juliaclient {}\n", VERSION) }

fn client_help() -> String {
    let daemon_mgmt = if cfg!(target_os = "linux") {
        "Daemon management (systemd):\n\n systemctl --user {start | stop | restart | status} julia-daemon\n"
    } else {
        "Daemon management:\n\n pgrep -f julia-conductor   (status)\n pkill -f julia-conductor   (stop)\n"
    };
    format!(
        "\n    juliaclient [switches] -- [programfile] [args...]\n\n\
        Switches (a '*' marks the default value, if applicable):\n\n\
         -v, --version              Display version information\n\
         -h, --help                 Print this message\n\
         --project[=<dir>|@.]       Set <dir> as the home project/environment\n\
         -e, --eval <expr>          Evaluate <expr>\n\
         -E, --print <expr>         Evaluate <expr> and display the result\n\
         -L, --load <file>          Load <file> immediately on all processors\n\
         -i                         Interactive mode; REPL runs and `isinteractive()` is true\n\
         --banner={{yes|no|auto*}}    Enable or disable startup banner\n\
         --color={{yes|no|auto*}}     Enable or disable color text\n\
         --history-file={{yes*|no}}   Load or save history\n\n\
        Client-specific switches:\n\n\
         -a, --address <addr>       Connect to conductor at <addr> instead of default\n\
         --session[=<label>]        Reuse worker state in Main module.\n\
         --sync                     Attach to shared REPL (requires --session=<label>)\n\
         --revise[=yes|no*]         Enable or disable Revise.jl integration\n\
         --restart                  Kill workers for the project and exit\n\
         --sandbox                  Run in an isolated sandbox (Linux only)\n\
         -t, --threads <spec>       Worker thread count (N, auto, or N,M for default,interactive)\n\
         --status[=live|json]       Show conductor/worker status; live redraws in place\n\n\
        {}\n",
        daemon_mgmt
    )
}

// --- Active client tracking ---

pub(crate) struct ActiveClientInfo {
    pub(crate) worker_id: u32,
    pub(crate) client_num: u32,
    pub(crate) start_time_us: i64,
    pub(crate) port_set: u16,
}

// --- Parsed client request (module-level struct, not inside impl) ---

struct ClientRequest {
    flags: u8,
    pid: u32,
    ppid: u32,
    cwd: String,
    env: Vec<EnvVar>,
    parsed: args::ParsedArgs,
    project: Option<String>,
}

// --- Sandbox mode ---

#[derive(Clone, Copy, PartialEq)]
enum SandboxMode { None, Remote, Local }

// --- Server socket abstraction ---

pub enum Server {
    Unix(UnixListener),
    Tcp(TcpListener),
}

pub enum IncomingConn {
    Unix(std::os::unix::net::UnixStream),
    Tcp(std::net::TcpStream, std::net::SocketAddr),
}

impl Server {
    pub fn accept(&self) -> io::Result<IncomingConn> {
        match self {
            Server::Unix(l) => {
                let (s, _) = l.accept()?;
                Ok(IncomingConn::Unix(s))
            }
            Server::Tcp(l) => {
                let (s, addr) = l.accept()?;
                Ok(IncomingConn::Tcp(s, addr))
            }
        }
    }
}

impl IncomingConn {
    fn as_raw_fd(&self) -> RawFd {
        use std::os::unix::io::AsRawFd;
        match self {
            IncomingConn::Unix(s) => s.as_raw_fd(),
            IncomingConn::Tcp(s, _) => s.as_raw_fd(),
        }
    }

    fn is_remote(&self, transport: TransportMode) -> bool {
        match self {
            IncomingConn::Tcp(_, addr) if transport == TransportMode::Tcp =>
                !protocol::is_loopback(addr),
            _ => false,
        }
    }
}

// --- Main conductor state ---

pub struct Conductor {
    pub config: Config,
    pub(crate) workers: std::collections::HashMap<String, Vec<Worker>>,
    pub(crate) active_clients: std::collections::HashMap<u32, ActiveClientInfo>,
    port_pool: Option<PortPool>,
    pub(crate) reserve: Option<Worker>,
    next_worker_id: u32,
    client_counter: u32,
    env_cache: EnvCache,
    pub socket_path: String,
    // --- pressure eviction / idle-budget state (see idle_budget, pressure.rs) ---
    pub(crate) crf: std::collections::HashMap<String, crate::worker::Crf>,
    pending_kills: Vec<PendingKill>,
    pub pressure: crate::pressure::PressureMonitor,
    // pid -> stop flag, for status-dashboard live subscribers that aren't
    // real assigned clients (see status.rs); set + checked by the background
    // thread `serve_status` spawns for `--status=live`.
    pub live_status_clients: std::collections::HashMap<u32, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    // Weak self-reference so a live-status background thread can re-lock the
    // conductor periodically without main.rs threading an Arc through every call.
    pub self_ref: Option<std::sync::Weak<std::sync::Mutex<Conductor>>>,
}

struct PendingKill {
    key: String,
    worker: Worker,
}

impl Conductor {
    pub fn new(config: Config) -> Self {
        let port_pool = config.port_range.map(|(base, count)| PortPool::new(base, count));
        let socket_path = config.socket_path.clone();
        let pressure = crate::pressure::PressureMonitor::new(&config);
        Conductor {
            config,
            workers: std::collections::HashMap::new(),
            active_clients: std::collections::HashMap::new(),
            port_pool,
            reserve: None,
            next_worker_id: 0,
            client_counter: 0,
            env_cache: EnvCache::new(),
            socket_path,
            crf: std::collections::HashMap::new(),
            pending_kills: Vec::new(),
            pressure,
            live_status_clients: std::collections::HashMap::new(),
            self_ref: None,
        }
    }

    // --- CRF (recency-frequency) tracking, keyed by worker pool key ---

    fn bump_crf(&mut self, key: &str, now: i64) {
        let half_life = self.crf_half_life();
        self.crf.entry(key.to_string()).or_default().bump(now, half_life);
    }

    fn crf_half_life(&self) -> f64 {
        ((self.config.max_ttl - self.config.min_ttl) as f64 / 4.0).max(1.0)
    }

    /// Adaptive idle-keep-alive budget for a worker: how long it may sit idle
    /// before `enforce_max_ttl` culls it unconditionally, and the upper bound
    /// pressure-eviction respects as "still worth keeping". Combines a
    /// recency/frequency-weighted expected-return-time estimate for the pool
    /// key (Crf) with a decayed busy-fraction estimate for the worker itself
    /// (Occupancy), so hot/regularly-summoned keys and heavily-used workers
    /// earn a longer runway than cold, rarely-touched ones — bounded to
    /// [min_ttl, max_ttl] (or a raised floor for labeled sessions, since a
    /// user is expected to return to a named REPL less predictably).
    pub(crate) fn idle_budget(&self, key: &str, w: &Worker) -> f64 {
        let max_ttl = self.config.max_ttl as f64;
        let min_ttl = if w.session_label.is_some() {
            (self.config.min_ttl as f64 * max_ttl).sqrt()
        } else {
            self.config.min_ttl as f64
        };

        let mut cadence = 0.0;
        if let Some(crf) = self.crf.get(key) {
            let crf_half_life = self.crf_half_life();
            let crf_val = crf.read(w.last_active, crf_half_life);
            let mult = 1.0 + (1.0 + (crf_val - 1.0).max(0.0) / 2.0).log2();
            cadence = mult * min_ttl.max(crf.interval_budget());
        }

        let budget_occ_half_life = ((max_ttl - min_ttl) / 2.0).max(1.0);
        let occ = w.occ_slow.peek(w.last_active, budget_occ_half_life);
        let idle_budget_bias = 0.25f64;
        let idle_budget_log_span = ((1.0 + idle_budget_bias) / idle_budget_bias).log2();
        let occ_budget = min_ttl
            + (max_ttl - min_ttl) * ((occ + idle_budget_bias) / idle_budget_bias).log2()
                / idle_budget_log_span;

        cadence.max(occ_budget).clamp(min_ttl, max_ttl)
    }

    pub fn create_server(&self) -> io::Result<Server> {
        match self.config.transport {
            TransportMode::Unix => {
                let l = UnixListener::bind(&self.config.socket_path)?;
                Ok(Server::Unix(l))
            }
            TransportMode::Tcp => {
                let addr = protocol::parse_host_port(&self.config.socket_path)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                let l = TcpListener::bind(addr)?;
                Ok(Server::Tcp(l))
            }
        }
    }

    pub fn write_pid_file(&self) {
        if self.config.transport != TransportMode::Unix { return; }
        let pid_path = format!("{}/conductor.pid", self.config.runtime_dir);
        if let Err(e) = std::fs::write(&pid_path, format!("{}", std::process::id())) {
            eprintln!("Warning: failed to write PID file: {}", e);
        }
    }

    pub fn cleanup_pid_file(&self) {
        if self.config.transport != TransportMode::Unix { return; }
        let _ = std::fs::remove_file(format!("{}/conductor.pid", self.config.runtime_dir));
    }

    pub fn cleanup_socket(&self) {
        if self.config.transport == TransportMode::Unix {
            let _ = std::fs::remove_file(&self.config.socket_path);
        }
    }

    pub fn cleanup_runtime_dir(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.config.runtime_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() { let _ = std::fs::remove_dir_all(&p); }
                else { let _ = std::fs::remove_file(&p); }
            }
        }
    }

    pub fn create_reserve_worker(&mut self, julia_channel: Option<&str>) -> io::Result<()> {
        let id = self.next_worker_id;
        self.next_worker_id += 1;
        // The reserve has no client request to read --threads from; fall back to
        // the conductor's own JULIA_NUM_THREADS, matching a plain worker's default.
        let threads = args::parse_threads(
            std::env::var("JULIA_NUM_THREADS").as_deref().unwrap_or(""),
        );
        let w = Worker::spawn(&self.config, id, &self.config.runtime_dir, julia_channel, false, threads)?;
        eprintln!("Reserve worker {} created (pid {})", w.id, w.process.pid());
        let _ = w.ping();
        self.reserve = Some(w);
        Ok(())
    }

    // --- Connection dispatch ---

    pub fn handle_connection(&mut self, conn: IncomingConn) {
        let fd = conn.as_raw_fd();
        let is_remote = conn.is_remote(self.config.transport);
        // Keep conn alive so fd stays valid
        let _conn_guard = conn;

        if self.config.transport == TransportMode::Tcp {
            protocol::set_tcp_nodelay_raw(fd);
        }

        let mut magic_buf = [0u8; 4];
        if protocol::read_exact_fd(fd, &mut magic_buf).is_err() { return; }
        let magic = u32::from_le_bytes(magic_buf);

        if magic == protocol::client::MAGIC {
            if let Err(e) = self.handle_client(fd, is_remote) {
                eprintln!("Client handling error: {}", e);
            }
        } else if magic == protocol::notification::MAGIC {
            self.handle_notification(fd);
        } else {
            eprintln!("Invalid magic: {:x}", magic);
        }
    }

    fn handle_notification(&mut self, fd: RawFd) {
        let mut buf = [0u8; 5];
        if protocol::read_exact_fd(fd, &mut buf).is_err() { return; }
        let pid = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
        match protocol::notification::Type::from_u8(buf[0]) {
            Some(protocol::notification::Type::ClientDone) => { self.client_done(pid); }
            Some(protocol::notification::Type::ClientExit) => {
                if self.drop_live_client(pid) { return; }
                if let Some(w_id) = self.client_done(pid) {
                    self.maybe_health_check_worker(w_id);
                }
            }
            Some(protocol::notification::Type::ClientInterrupt) => {
                if self.drop_live_client(pid) { return; }
                self.handle_interrupt(pid);
            }
            Some(protocol::notification::Type::WorkerExit) => {
                eprintln!("Worker (pid {}) exiting (TTL expired)", pid);
            }
            _ => {}
        }
    }

    fn handle_client(&mut self, fd: RawFd, is_remote: bool) -> io::Result<()> {
        let request = self.read_client_request(fd, is_remote)?;
        self.client_counter += 1;
        let client_num = self.client_counter;

        if request.parsed.has_switch("--help") || request.parsed.has_switch("-h") {
            return self.serve_string(fd, &client_help());
        }
        if request.parsed.has_switch("--version") || request.parsed.has_switch("-v") {
            return self.serve_string(fd, &version_string());
        }
        if request.parsed.has_switch("--status") {
            let format = request.parsed.get_switch("--status").unwrap_or("");
            let tty = request.flags & 1 != 0;
            return self.serve_status(fd, format, tty, request.pid);
        }

        let sandbox = if is_remote && (self.config.sandbox_remote_clients || request.parsed.has_switch("--sandbox")) {
            SandboxMode::Remote
        } else if request.parsed.has_switch("--sandbox") {
            SandboxMode::Local
        } else {
            SandboxMode::None
        };

        if sandbox != SandboxMode::None && !cfg!(target_os = "linux") {
            let msg = if sandbox == SandboxMode::Remote {
                "Sandboxed workers are only available on Linux. Remote TCP clients are rejected.\n"
            } else {
                "--sandbox requires Linux (user namespaces).\n"
            };
            return self.serve_string(fd, msg);
        }

        // Session bypass for remote clients
        if sandbox == SandboxMode::Remote {
            if let Some(label) = request.parsed.get_switch("--session") {
                if !label.is_empty() && self.config.sandbox_session_bypass {
                    if let Some(w_id) = self.find_worker_by_label_global(label) {
                        eprintln!("Client {}: session bypass → worker {} (label '{}')", client_num, w_id, label);
                        return self.assign_client_by_worker_id(fd, &request, w_id);
                    }
                }
            }
        }

        // Worker key. --threads is folded in: workers spawned with a different
        // thread count are a different pool of processes (Julia fixes thread
        // counts at startup, so a mismatched worker can never be reused).
        let project_path = request.project.as_deref().unwrap_or("").to_string();
        let julia_channel = request.parsed.julia_channel.clone();
        let threads = resolve_threads(&request);
        let tkey = args::pack_threads(threads);
        let worker_key = match sandbox {
            SandboxMode::None => make_worker_key(&project_path, julia_channel.as_deref(), tkey),
            SandboxMode::Remote => julia_channel.as_ref()
                .map(|ch| format!("__sandbox__\x00{}\x00{}", ch, tkey))
                .unwrap_or_else(|| format!("__sandbox__\x00\x00{}", tkey)),
            SandboxMode::Local => {
                let cwd = trim_trailing_slashes(&request.cwd).to_string();
                let proj = trim_trailing_slashes(&project_path).to_string();
                let is_named = !proj.is_empty() && proj.starts_with('@');
                let has_local = !proj.is_empty() && !is_named;
                let rw_mount = if has_local && path_covered_by(&cwd, &[&proj]) { proj.clone() } else { cwd.clone() };
                if let Some(ch) = &julia_channel {
                    format!("__lsandbox__\x00{}\x00{}\x00{}\x00{}", rw_mount, proj, ch, tkey)
                } else {
                    format!("__lsandbox__\x00{}\x00{}\x00\x00{}", rw_mount, proj, tkey)
                }
            }
        };

        if request.parsed.has_switch("--sync") {
            let session = request.parsed.get_switch("--session");
            if session.is_none() || session.unwrap().is_empty() {
                return self.serve_string(fd, "--sync requires --session=<label>\n");
            }
        }

        if request.parsed.has_switch("--restart") {
            let n = self.kill_workers_for_project(&worker_key);
            return self.serve_string(fd, &format!("Reset: killed {} worker(s) for project\n", n));
        }

        self.assign_client_to_worker(fd, &request, &worker_key, sandbox, client_num, threads)
    }

    // --- Client request reading ---

    fn read_client_request(&mut self, fd: RawFd, is_remote: bool) -> io::Result<ClientRequest> {
        let mut hdr = [0u8; 12];
        protocol::read_exact_fd(fd, &mut hdr)?;
        let flags = hdr[0];
        let pid  = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let ppid = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);

        let cwd = read_len_prefixed_str(fd)?;

        let mut fp_buf = [0u8; 8];
        protocol::read_exact_fd(fd, &mut fp_buf)?;
        let fingerprint = u64::from_le_bytes(fp_buf);

        let raw_args = self.read_client_args(fd)?;

        let env = self.resolve_env(fd, fingerprint, is_remote)?;
        let julia_project = self.env_cache.lookup(fingerprint)
            .and_then(|(_, jp)| jp)
            .map(|s| s.to_string());

        let raw_strings: Vec<String> = raw_args.iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        let parsed = args::parse(&raw_strings);

        let project = if is_remote { None } else {
            let home = self.config.host_home.clone();
            let cwd_clone = cwd.clone();
            project::resolve(&parsed, julia_project.as_deref(), &home, &cwd_clone)
        };

        Ok(ClientRequest { flags, pid, ppid, cwd, env, parsed, project })
    }

    fn resolve_env(&mut self, fd: RawFd, fingerprint: u64, force_fresh: bool) -> io::Result<Vec<EnvVar>> {
        if !force_fresh {
            if let Some((cached, _)) = self.env_cache.lookup(fingerprint) {
                return Ok(cached.to_vec());
            }
        }
        protocol::write_all_fd(fd, &[protocol::client::ENV_REQUEST]);
        let fresh = read_full_env(fd)?;
        let (stored, _) = self.env_cache.insert(fingerprint, fresh);
        Ok(stored.to_vec())
    }

    fn read_client_args(&self, fd: RawFd) -> io::Result<Vec<Vec<u8>>> {
        let mut count_buf = [0u8; 2];
        protocol::read_exact_fd(fd, &mut count_buf)?;
        let count = u16::from_le_bytes(count_buf) as usize;
        let mut args = Vec::with_capacity(count);
        for _ in 0..count {
            args.push(read_len_prefixed_bytes(fd)?);
        }
        Ok(args)
    }

    // --- Worker assignment ---

    fn assign_client_to_worker(
        &mut self, fd: RawFd, request: &ClientRequest,
        worker_key: &str, sandbox: SandboxMode, client_num: u32, threads: args::Threads,
    ) -> io::Result<()> {
        let session_label = request.parsed.get_switch("--session").map(|s| s.to_string());
        let is_labeled = session_label.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        let want_interactive = request.parsed.has_switch("-i");
        let now = unix_time();
        self.bump_crf(worker_key, now);

        eprintln!("Client {}; pid: {}{}{}{}{}, project: {}{}",
            client_num, request.pid,
            request.parsed.julia_channel.as_deref().map(|_| ", julia: ").unwrap_or(""),
            request.parsed.julia_channel.as_deref().unwrap_or(""),
            session_label.as_deref().map(|_| ", session: ").unwrap_or(""),
            session_label.as_deref().unwrap_or(""),
            request.project.as_deref().unwrap_or("(default)"),
            if sandbox != SandboxMode::None { " [sandboxed]" } else { "" },
        );

        let port_set = self.allocate_port_set()?;

        // Try to find existing worker
        let w_id = self.select_existing_worker(
            worker_key, session_label.as_deref(), is_labeled, want_interactive, request.ppid, sandbox, now
        );

        let w_id = if let Some(id) = w_id {
            // Existing worker reuse: is_worker_available lets a worker through
            // once its own label has expired (see is_label_expired), so it may
            // still be holding that expired session's REPL/Main-module state.
            // Tear that down before potentially handing it a different session.
            if let Some(w) = self.get_worker_mut(id) {
                if let Some(old_label) = w.session_label.clone() {
                    if session_label.as_deref() != Some(old_label.as_str()) {
                        eprintln!("Worker {}: dropping expired session '{}'", id, old_label);
                        let _ = w.drop_session(&old_label);
                        w.session_label = None;
                    }
                }
            }
            id
        } else {
            // Spawn new worker
            let project_path = request.project.as_deref().unwrap_or("").to_string();
            let julia_channel = request.parsed.julia_channel.as_deref().map(|s| s.to_string());
            self.spawn_worker_for_key(worker_key, &project_path, julia_channel.as_deref(), want_interactive, threads, sandbox)?
        };

        if is_labeled {
            if let Some(label) = &session_label {
                if let Some(w) = self.get_worker_mut(w_id) {
                    if w.session_label.is_none() {
                        eprintln!("Worker {}: assigning label '{}'", w_id, label);
                        w.session_label = Some(label.clone());
                    }
                }
            }
        }

        // Update worker state (extract config before mutable borrow)
        let maxclients = self.config.worker_maxclients;
        let ppid = request.ppid;
        if let Some(w) = self.get_worker_mut(w_id) {
            w.last_pinged = now;
            w.record_ppid(ppid, maxclients);
        }

        // Build client info and call run_client
        let sandbox_env = if sandbox == SandboxMode::Remote {
            Some(build_sandbox_env(&request.env))
        } else {
            None
        };

        let was_idle = self.get_worker_mut(w_id).map(|w| w.active_clients == 0).unwrap_or(true);
        let paths = {
            let w = self.get_worker_mut(w_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "worker not found"))?;
            let client_info = ClientInfo {
                tty: request.flags & 1 != 0,
                force: is_labeled,
                pid: request.pid,
                ppid: request.ppid,
                cwd: if sandbox == SandboxMode::Remote { "/home/sandbox" } else { &request.cwd },
                env: sandbox_env.as_deref().unwrap_or(&request.env),
                switches: &request.parsed.switches,
                program_file: request.parsed.program_file.as_deref(),
                program_args: &request.parsed.program_args,
                port_set,
            };
            w.run_client(&client_info)?
        };

        self.register_client(request.pid, client_num, w_id, port_set, was_idle);
        eprintln!("Client {}: done (assigned to worker {})", client_num, w_id);
        self.send_socket_paths(fd, &paths);
        Ok(())
    }

    fn assign_client_by_worker_id(&mut self, fd: RawFd, request: &ClientRequest, w_id: u32) -> io::Result<()> {
        let port_set = self.allocate_port_set()?;
        let is_labeled = request.parsed.get_switch("--session").map(|s| !s.is_empty()).unwrap_or(false);
        let cwd = if !self.config.host_home.is_empty() { self.config.host_home.clone() } else { "/".to_string() };

        let maxclients = self.config.worker_maxclients;
        let was_idle = self.get_worker_mut(w_id).map(|w| w.active_clients == 0).unwrap_or(true);
        let paths = {
            let w = self.get_worker_mut(w_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "worker not found"))?;
            let client_info = ClientInfo {
                tty: request.flags & 1 != 0,
                force: is_labeled,
                pid: request.pid,
                ppid: request.ppid,
                cwd: &cwd,
                env: &request.env,
                switches: &request.parsed.switches,
                program_file: request.parsed.program_file.as_deref(),
                program_args: &request.parsed.program_args,
                port_set,
            };
            w.last_pinged = unix_time();
            w.record_ppid(request.ppid, maxclients);
            w.run_client(&client_info)?
        };

        self.register_client(request.pid, self.client_counter, w_id, port_set, was_idle);
        self.send_socket_paths(fd, &paths);
        Ok(())
    }

    fn select_existing_worker(
        &self, key: &str, session_label: Option<&str>, is_labeled: bool,
        want_interactive: bool, ppid: u32, sandbox: SandboxMode, now: i64,
    ) -> Option<u32> {
        // 1. Session label match
        if is_labeled {
            if let Some(label) = session_label {
                if let Some(id) = self.find_worker_in_list(key, |w| {
                    w.session_label.as_deref() == Some(label)
                }) { return Some(id); }
            }
        }

        if sandbox == SandboxMode::Remote { return None; }

        // 2. PPID affinity
        if let Some(id) = self.find_worker_in_list(key, |w| {
            self.is_worker_available(w, want_interactive, now) && w.recent_ppids.contains(&ppid)
        }) { return Some(id); }

        // 3. Second-most-recent available worker
        self.try_existing_workers(key, want_interactive, now)
    }

    // --- Worker pool management ---

    fn allocate_port_set(&mut self) -> io::Result<u16> {
        if let Some(pool) = &mut self.port_pool {
            pool.allocate().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "port pool exhausted"))
        } else {
            Ok(PORT_POOL_NONE)
        }
    }

    fn spawn_worker_for_key(
        &mut self, key: &str, project: &str, julia_channel: Option<&str>,
        interactive: bool, threads: args::Threads, sandbox: SandboxMode,
    ) -> io::Result<u32> {
        match sandbox {
            SandboxMode::None => self.add_worker_to_pool(key, project, julia_channel, interactive, threads),
            #[cfg(target_os = "linux")]
            SandboxMode::Remote => self.add_sandboxed_worker(key, project, julia_channel, threads, &[], &[]),
            #[cfg(target_os = "linux")]
            SandboxMode::Local => {
                let rw = vec![project.to_string()];
                self.add_sandboxed_worker(key, project, julia_channel, threads, &[], &rw)
            }
            _ => Err(io::Error::new(io::ErrorKind::Unsupported, "sandbox requires Linux")),
        }
    }

    fn add_worker_to_pool(
        &mut self, key: &str, project: &str, julia_channel: Option<&str>, interactive: bool,
        threads: args::Threads,
    ) -> io::Result<u32> {
        let can_use_reserve = !interactive && self.reserve.as_ref().map_or(false, |r| {
            r.julia_channel.as_deref() == julia_channel && r.threads == threads
        });

        let id = if can_use_reserve {
            let reserve = self.reserve.take().unwrap();
            let id = reserve.id;
            eprintln!("Assigning reserve worker {} to project {}", id, project);
            let list = self.workers.entry(key.to_string()).or_default();
            list.push(reserve);
            id
        } else {
            let id = self.next_worker_id;
            self.next_worker_id += 1;
            let w = Worker::spawn(&self.config, id, &self.config.runtime_dir, julia_channel, interactive, threads)?;
            eprintln!("Spawning {}worker {} (pid {}) for project {}",
                if interactive { "interactive " } else { "" }, id, w.process.pid(), project);
            let list = self.workers.entry(key.to_string()).or_default();
            list.push(w);
            id
        };

        if !project.is_empty() {
            let proj = project.to_string();
            if let Some(w) = self.get_worker_mut(id) {
                let _ = w.set_project(proj);
            }
        }

        if can_use_reserve && self.reserve.is_none() {
            let _ = self.create_reserve_worker(None);
        }
        Ok(id)
    }

    #[cfg(target_os = "linux")]
    fn add_sandboxed_worker(
        &mut self, key: &str, project: &str, julia_channel: Option<&str>, threads: args::Threads,
        extra_ro: &[String], extra_rw: &[String],
    ) -> io::Result<u32> {
        let id = self.next_worker_id;
        self.next_worker_id += 1;
        let environ: std::collections::HashMap<String, String> = std::env::vars().collect();
        let mut ro = vec![self.config.worker_project.clone()];
        ro.extend_from_slice(extra_ro);
        let w = crate::worker::Worker::spawn_sandboxed(
            &self.config, id, &self.config.runtime_dir,
            julia_channel, threads, &environ, &ro, extra_rw,
        )?;
        eprintln!("Spawning sandboxed worker {} (pid {}){}{}", id, w.process.pid(),
            if !project.is_empty() { " for project " } else { "" }, project);
        let list = self.workers.entry(key.to_string()).or_default();
        list.push(w);
        if !project.is_empty() {
            let proj = project.to_string();
            if let Some(w) = self.get_worker_mut(id) { let _ = w.set_project(proj); }
        }
        Ok(id)
    }

    fn kill_workers_for_project(&mut self, key: &str) -> usize {
        self.crf.remove(key);
        if let Some(list) = self.workers.remove(key) {
            let count = list.len();
            for w in list {
                self.remove_active_clients_for_worker(w.id);
                w.soft_exit();
                if w.sandboxed { self.remove_sandbox_dir(w.id); }
            }
            count
        } else { 0 }
    }

    pub fn kill_unresponsive_worker(&mut self, id: u32) {
        eprintln!("Killing unresponsive worker {}", id);
        if self.reserve.as_ref().map_or(false, |r| r.id == id) {
            self.reserve = None;
            return;
        }
        self.remove_active_clients_for_worker(id);
        for list in self.workers.values_mut() {
            if let Some(pos) = list.iter().position(|w| w.id == id) {
                let mut w = list.swap_remove(pos);
                w.process.kill(libc::SIGKILL);
                if w.sandboxed { self.remove_sandbox_dir(w.id); }
                return;
            }
        }
    }

    fn remove_sandbox_dir(&self, id: u32) {
        let _ = std::fs::remove_dir_all(format!("{}/sandbox-{}", self.config.runtime_dir, id));
    }

    // --- Worker selection helpers ---

    fn is_worker_available(&self, w: &Worker, interactive: bool, now: i64) -> bool {
        let max = self.config.worker_maxclients;
        if max != 0 && w.active_clients >= max { return false; }
        if w.session_label.is_some() && !self.is_label_expired(w, now) { return false; }
        w.interactive == interactive
    }

    fn is_label_expired(&self, w: &Worker, now: i64) -> bool {
        if w.session_label.is_none() || w.active_clients > 0 { return false; }
        (now - w.last_active) as u64 >= self.config.label_ttl
    }

    fn find_worker_in_list<F>(&self, key: &str, pred: F) -> Option<u32>
    where F: Fn(&Worker) -> bool
    {
        self.workers.get(key)?.iter().find(|w| pred(w)).map(|w| w.id)
    }

    fn try_existing_workers(&self, key: &str, interactive: bool, now: i64) -> Option<u32> {
        let list = self.workers.get(key)?;
        let mut best: Option<&Worker> = None;
        let mut second: Option<&Worker> = None;
        for w in list {
            if !self.is_worker_available(w, interactive, now) { continue; }
            if best.map_or(true, |b| w.last_active > b.last_active) {
                second = best;
                best = Some(w);
            } else if second.map_or(true, |s| w.last_active > s.last_active) {
                second = Some(w);
            }
        }
        second.or(best).map(|w| w.id)
    }

    fn find_worker_by_label_global(&self, label: &str) -> Option<u32> {
        for list in self.workers.values() {
            if let Some(w) = list.iter().find(|w| w.session_label.as_deref() == Some(label)) {
                return Some(w.id);
            }
        }
        None
    }

    fn get_worker_mut(&mut self, id: u32) -> Option<&mut Worker> {
        if self.reserve.as_ref().map_or(false, |r| r.id == id) {
            return self.reserve.as_mut();
        }
        for list in self.workers.values_mut() {
            if let Some(w) = list.iter_mut().find(|w| w.id == id) {
                return Some(w);
            }
        }
        None
    }

    // --- Client tracking ---

    // `was_idle` is whether the worker had zero active clients right before
    // `run_client` was called for this assignment. active_clients itself is
    // NOT touched here: `Worker::run_client` already set it from the wire —
    // the worker's own authoritative post-assignment client count — so
    // incrementing it again here would double-count.
    fn register_client(&mut self, pid: u32, client_num: u32, worker_id: u32, port_set: u16, was_idle: bool) {
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        self.active_clients.insert(pid, ActiveClientInfo { worker_id, client_num, start_time_us: now_us, port_set });
        let activity_hl = self.activity_half_life();
        let budget_hl = self.budget_occ_half_life();
        if let Some(w) = self.get_worker_mut(worker_id) {
            // Only the 0->1 transition marks the worker as "busy" for occupancy —
            // a second concurrent client doesn't make it any busier by this measure.
            if was_idle && w.active_clients > 0 {
                let now = unix_time();
                w.occ_fast.attach(now, activity_hl);
                w.occ_slow.attach(now, budget_hl);
            }
        }
    }

    pub(crate) fn activity_half_life(&self) -> f64 { self.config.min_ttl as f64 }
    fn budget_occ_half_life(&self) -> f64 { ((self.config.max_ttl - self.config.min_ttl) as f64 / 2.0).max(1.0) }

    fn client_done(&mut self, pid: u32) -> Option<u32> {
        let info = self.active_clients.remove(&pid)?;
        self.release_port_set(info.port_set);
        let w_id = info.worker_id;
        let activity_hl = self.activity_half_life();
        let budget_hl = self.budget_occ_half_life();
        let mut retire_interactive = false;
        if let Some(w) = self.get_worker_mut(w_id) {
            if w.active_clients > 0 { w.active_clients -= 1; }
            w.last_active = unix_time();
            if w.active_clients == 0 {
                w.occ_fast.detach(w.last_active, activity_hl);
                w.occ_slow.detach(w.last_active, budget_hl);
                // Interactive sessions are ephemeral by design: once the last
                // attached client leaves, the worker goes with it rather than
                // sitting in the pool waiting for a future --session reattach
                // (unlike non-interactive workers, which are meant to be reused).
                retire_interactive = w.interactive;
            }
        }
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let dur_us = now_us - info.start_time_us;
        eprintln!("Client {} disconnected; worker: {}, duration: {}.{:03}s",
            info.client_num, w_id, dur_us / 1_000_000, (dur_us % 1_000_000) / 1_000);

        if retire_interactive {
            if let Some(key) = self.find_worker_key(w_id) {
                eprintln!("Worker {}: interactive session ended, retiring", w_id);
                self.retire_worker(&key, w_id);
            }
            return None;
        }

        let active = self.get_worker_mut(w_id).map(|w| w.active_clients).unwrap_or(1);
        if active == 0 { Some(w_id) } else { None }
    }

    fn find_worker_key(&self, id: u32) -> Option<String> {
        self.workers.iter()
            .find(|(_, list)| list.iter().any(|w| w.id == id))
            .map(|(k, _)| k.clone())
    }

    fn remove_active_clients_for_worker(&mut self, w_id: u32) {
        let pids: Vec<u32> = self.active_clients.iter()
            .filter(|(_, v)| v.worker_id == w_id)
            .map(|(&k, _)| k)
            .collect();
        for pid in pids {
            if let Some(info) = self.active_clients.remove(&pid) {
                self.release_port_set(info.port_set);
            }
        }
    }

    fn release_port_set(&mut self, port_set: u16) {
        if port_set != PORT_POOL_NONE {
            if let Some(pool) = &mut self.port_pool { pool.release(port_set); }
        }
    }

    // --- Health checking ---

    pub fn check_workers(&mut self) {
        let now = unix_time();
        let interval = self.config.ping_interval;
        let timeout = self.config.ping_timeout;

        let ids: Vec<u32> = {
            let mut ids = Vec::new();
            if let Some(r) = &self.reserve {
                if r.should_ping(now, interval) { ids.push(r.id); }
            }
            for list in self.workers.values() {
                for w in list {
                    if w.should_ping(now, interval) { ids.push(w.id); }
                }
            }
            ids
        };

        let mut to_kill = Vec::new();
        for id in ids {
            if !self.ping_worker(id, timeout) {
                to_kill.push(id);
            }
        }
        for id in to_kill {
            self.kill_unresponsive_worker(id);
        }
    }

    fn ping_worker(&mut self, id: u32, timeout_secs: u64) -> bool {
        // Phase 1: do the ping (borrow ends when block closes)
        let ping_result = {
            let Some(w) = self.get_worker_mut(id) else { return false };
            let _ = w.socket.set_read_timeout(Some(Duration::from_secs(timeout_secs)));
            w.ping()
        };

        match ping_result {
            Err(_) => false,
            Ok(pong_buf) => {
                // Phase 2: check for mismatch without holding worker borrow
                let worker_count = u16::from_le_bytes([pong_buf[3], pong_buf[4]]);
                let conductor_count = self.get_worker_mut(id).map(|w| w.active_clients).unwrap_or(0);
                if worker_count as u32 != conductor_count {
                    eprintln!("Worker {}: client count mismatch (worker={}, conductor={}), syncing",
                        id, worker_count, conductor_count);
                    self.sync_worker_clients(id);
                }
                // Phase 3: update ping timestamp
                let timeout = self.config.ping_timeout;
                if let Some(w) = self.get_worker_mut(id) {
                    w.last_pinged = unix_time();
                    let _ = w.socket.set_read_timeout(Some(Duration::from_secs(timeout)));
                }
                true
            }
        }
    }

    fn maybe_health_check_worker(&mut self, w_id: u32) {
        let timeout = self.config.ping_timeout;
        if !self.ping_worker(w_id, timeout) {
            self.kill_unresponsive_worker(w_id);
        }
    }

    pub fn sync_worker_clients(&mut self, w_id: u32) {
        let pids: Vec<u32> = self.active_clients.iter()
            .filter(|(_, v)| v.worker_id == w_id)
            .map(|(&k, _)| k)
            .collect();
        let result = {
            let Some(w) = self.get_worker_mut(w_id) else { return };
            w.sync_clients(&pids)
        };
        match result {
            Ok(remaining) => {
                if let Some(w) = self.get_worker_mut(w_id) {
                    w.active_clients = remaining as u32;
                }
                eprintln!("Worker {}: sync complete, {} active clients", w_id, remaining);
                self.reconcile_client_map(w_id);
            }
            Err(_) => self.kill_unresponsive_worker(w_id),
        }
    }

    /// Pull the worker's actual client PID set and drop any `active_clients`
    /// entries pointing at this worker that it no longer reports — repairs a
    /// lost `client_done` notification that the count-only `sync_clients`
    /// push can't fix on its own.
    fn reconcile_client_map(&mut self, w_id: u32) {
        let Some(w) = self.get_worker_mut(w_id) else { return };
        let Ok(actual) = w.query_clients() else { return };
        let stale: Vec<u32> = self.active_clients.iter()
            .filter(|(&pid, v)| v.worker_id == w_id && !actual.contains(&pid))
            .map(|(&pid, _)| pid)
            .collect();
        for pid in stale {
            eprintln!("Worker {}: reconcile dropped stale client pid {}", w_id, pid);
            self.client_done(pid);
        }
    }

    // --- Idle culling / pressure eviction ---

    pub(crate) fn cullable(&self, w: &Worker, now: i64) -> bool {
        w.active_clients == 0 && (w.session_label.is_none() || self.is_label_expired(w, now))
    }

    /// Unconditional idle cull: any cullable worker past its adaptive idle
    /// budget goes, regardless of memory pressure. Runs every ping_interval.
    pub fn enforce_max_ttl(&mut self) {
        let now = unix_time();
        let mut to_retire: Vec<(String, u32)> = Vec::new();
        for (key, list) in self.workers.iter() {
            for w in list {
                if !self.cullable(w, now) { continue; }
                let budget = self.idle_budget(key, w);
                if (now - w.last_active) as f64 >= budget {
                    to_retire.push((key.clone(), w.id));
                }
            }
        }
        for (key, id) in to_retire {
            self.retire_worker(&key, id);
        }
        // Drop CRF history for keys that no longer have any worker — lets a
        // cold key's next summon start fresh instead of reusing a stale score.
        let empty_keys: Vec<String> = self.workers.iter()
            .filter(|(_, list)| list.is_empty())
            .map(|(k, _)| k.clone())
            .collect();
        for key in empty_keys {
            self.workers.remove(&key);
            self.crf.remove(&key);
        }
    }

    const MAX_EVICT_PER_EPISODE: usize = 4;

    /// Pressure-reactive eviction: when `pressure.poll()` reports the system
    /// under memory pressure, cull up to MAX_EVICT_PER_EPISODE cullable
    /// workers that are past min_ttl but haven't yet hit their full idle
    /// budget, cheapest-and-least-active first. Runs every
    /// min(5s, ping_interval), only while pressure is active.
    pub fn run_eviction_episode(&mut self) {
        if !self.pressure.poll() { return; }
        self.refresh_all_stats();
        let now = unix_time();
        let activity_hl = self.activity_half_life();

        // The reserve worker is always the cheapest candidate: it's holding
        // no state worth keeping and is trivially regenerated.
        let mut candidates: Vec<(f64, String, u32)> = Vec::new();
        if let Some(r) = &self.reserve {
            candidates.push((f64::MIN, String::new(), r.id));
        }
        for (key, list) in self.workers.iter() {
            for w in list {
                if !self.cullable(w, now) { continue; }
                let idle_age = (now - w.last_active) as f64;
                let budget = self.idle_budget(key, w);
                if idle_age < self.config.min_ttl as f64 || idle_age >= budget { continue; }
                let activity = w.occ_fast.peek(now, activity_hl);
                let size_mib = (w.mem_bytes as f64 / (1024.0 * 1024.0)).max(1.0);
                candidates.push((activity / size_mib, key.clone(), w.id));
            }
        }
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, key, id) in candidates.into_iter().take(Self::MAX_EVICT_PER_EPISODE) {
            eprintln!("Worker {}: evicting under memory pressure ({})", id, self.pressure.source_name());
            self.retire_worker(&key, id);
        }
    }

    /// Sample RSS/CPU for every live worker (pooled + reserve) in one pass —
    /// feeds both eviction sizing and the --status dashboard.
    pub fn refresh_all_stats(&mut self) {
        let now_us = unix_time_us();
        for list in self.workers.values_mut() {
            for w in list { w.refresh_stats(now_us); }
        }
        if let Some(r) = &mut self.reserve { r.refresh_stats(now_us); }
    }

    /// Move a worker out of the active pool into staged retirement: soft_exit
    /// now, SIGTERM after a grace period, SIGKILL after another. Staging
    /// (rather than killing in place) gives the worker's Julia side a real
    /// chance to flush/exit cleanly before anything more forceful.
    fn retire_worker(&mut self, key: &str, id: u32) {
        let now = unix_time();
        let worker = if key.is_empty() && self.reserve.as_ref().map_or(false, |r| r.id == id) {
            self.reserve.take()
        } else if let Some(list) = self.workers.get_mut(key) {
            list.iter().position(|w| w.id == id).map(|pos| list.remove(pos))
        } else {
            None
        };
        let Some(mut w) = worker else { return };
        eprintln!("Worker {}: retiring (key '{}')", id, key);
        self.remove_active_clients_for_worker(id);
        w.soft_exit();
        w.retire_stage = crate::worker::RetireStage::Soft;
        w.retire_since = now;
        self.pending_kills.push(PendingKill { key: key.to_string(), worker: w });
    }

    const RETIRE_GRACE_SECS: i64 = 5;

    /// Advance staged retirements: Soft -> (grace) -> SIGTERM -> (grace) -> SIGKILL+drop.
    pub fn sweep_pending_kills(&mut self) {
        let now = unix_time();
        let mut done = Vec::new();
        let mut to_clean_sandbox = Vec::new();
        for (i, pk) in self.pending_kills.iter_mut().enumerate() {
            match pk.worker.retire_stage {
                crate::worker::RetireStage::Soft => {
                    if now - pk.worker.retire_since >= Self::RETIRE_GRACE_SECS {
                        eprintln!("Worker {} (key '{}'): soft_exit grace elapsed, sending SIGTERM", pk.worker.id, pk.key);
                        pk.worker.process.kill(libc::SIGTERM);
                        pk.worker.retire_stage = crate::worker::RetireStage::Term;
                        pk.worker.retire_since = now;
                    }
                }
                crate::worker::RetireStage::Term => {
                    if now - pk.worker.retire_since >= Self::RETIRE_GRACE_SECS {
                        eprintln!("Worker {} (key '{}'): SIGTERM grace elapsed, sending SIGKILL", pk.worker.id, pk.key);
                        pk.worker.process.kill(libc::SIGKILL);
                        if pk.worker.sandboxed { to_clean_sandbox.push(pk.worker.id); }
                        done.push(i);
                    }
                }
                crate::worker::RetireStage::Running => {
                    // Shouldn't happen — retire_worker always sets Soft — but
                    // treat as already-terminal to avoid a stuck entry.
                    done.push(i);
                }
            }
        }
        for &i in done.iter().rev() {
            self.pending_kills.remove(i);
        }
        for id in to_clean_sandbox {
            self.remove_sandbox_dir(id);
        }
    }

    // --- Client-requested interrupt (Ctrl-C mid-eval) ---

    /// Deliver SIGINT to the worker process serving `pid`. Julia's runtime
    /// turns this into an InterruptException on the running task; untargeted
    /// (the whole process gets it), but a worker normally serves one client
    /// at a time, so in practice it reaches the right one.
    /// If `pid` belongs to a live `--status=live` subscriber (not a real
    /// assigned client — see status.rs), tear its dashboard connection down
    /// and report handled so the caller skips normal client bookkeeping.
    fn drop_live_client(&mut self, pid: u32) -> bool {
        if let Some(flag) = self.live_status_clients.remove(&pid) {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn handle_interrupt(&mut self, pid: u32) {
        let Some(info) = self.active_clients.get(&pid) else { return };
        let w_id = info.worker_id;
        if let Some(w) = self.get_worker_mut(w_id) {
            w.process.kill(libc::SIGINT);
        }
    }

    // --- Graceful shutdown ---

    pub fn graceful_shutdown(&mut self) {
        for list in self.workers.values() {
            for w in list { w.soft_exit(); }
        }
        if let Some(r) = &self.reserve { r.soft_exit(); }
        std::thread::sleep(Duration::from_secs(1));
        self.signal_all_workers(libc::SIGTERM);
        std::thread::sleep(Duration::from_secs(1));
        self.signal_all_workers(libc::SIGKILL);
    }

    fn signal_all_workers(&mut self, sig: libc::c_int) {
        for list in self.workers.values_mut() {
            for w in list { w.process.kill(sig); }
        }
        if let Some(r) = &mut self.reserve { r.process.kill(sig); }
    }

    // --- serve_string: temporary stdio proxy for help/version ---

    fn serve_string(&mut self, client_fd: RawFd, content: &str) -> io::Result<()> {
        let (conn_fds, port_set_idx) = self.open_client_streams(client_fd)?;
        protocol::write_all_fd(conn_fds[1], content.as_bytes());
        protocol::write_all_fd(conn_fds[3], &[protocol::signals::EXIT, 0x01, 0x00]);
        for &fd in &conn_fds { unsafe { libc::close(fd); } }
        self.release_port_set_idx(port_set_idx);
        Ok(())
    }

    /// `--status[=live|json]`. `format=="json"` and non-TTY requests render
    /// once and close. `format=="live"` on a TTY holds the connection open
    /// and redraws periodically from a background thread (never from the
    /// main accept-loop thread — see status.rs) until the client disconnects
    /// or interrupts. Anything else (bare `--status` on a TTY) renders a
    /// single text frame, same as the non-live case, just human-formatted.
    fn serve_status(&mut self, client_fd: RawFd, format: &str, tty: bool, pid: u32) -> io::Result<()> {
        let live = format == "live" && tty;
        let json = format == "json";

        let (conn_fds, port_set_idx) = self.open_client_streams(client_fd)?;
        self.refresh_all_stats();
        let now = unix_time();
        let frame = if json {
            status::render_json(self, now)
        } else {
            status::render_text(self, now)
        };

        if !live {
            protocol::write_all_fd(conn_fds[1], frame.as_bytes());
            protocol::write_all_fd(conn_fds[3], &[protocol::signals::EXIT, 0x01, 0x00]);
            for &fd in &conn_fds { unsafe { libc::close(fd); } }
            self.release_port_set_idx(port_set_idx);
            return Ok(());
        }

        // Live: keep the connection open, hand it to a background thread.
        // Cook the terminal (not raw) so ^C/^D become real SIGINT/EOF at the
        // client and tear the session down through the normal exit-notify /
        // client_interrupt paths rather than needing bespoke keypress handling.
        protocol::write_all_fd(conn_fds[3], &[protocol::signals::RAW_MODE, 0x01, 0x00]);
        protocol::write_all_fd(conn_fds[1], status::HIDE_CURSOR);
        protocol::write_all_fd(conn_fds[1], frame.as_bytes());

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.live_status_clients.insert(pid, stop.clone());
        self.release_port_set_idx(port_set_idx);

        let Some(self_ref) = self.self_ref.clone() else {
            // No self-reference registered (shouldn't happen once main.rs
            // wires it up) — can't redraw, just leave the first frame up.
            unsafe { libc::close(conn_fds[1]); libc::close(conn_fds[3]); }
            return Ok(());
        };
        let stdout_fd = conn_fds[1];
        let signals_fd = conn_fds[3];
        let mut prev_lines = frame.lines().count();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(status::LIVE_HEARTBEAT_MS));
                if stop.load(std::sync::atomic::Ordering::SeqCst) { break; }
                let Some(conductor) = self_ref.upgrade() else { break };
                let frame = {
                    let Ok(mut c) = conductor.lock() else { break };
                    if !c.live_status_clients.contains_key(&pid) { break; }
                    c.refresh_all_stats();
                    let now = unix_time();
                    status::render_text(&mut c, now)
                };
                let redraw = status::redraw_sequence(prev_lines, &frame);
                prev_lines = frame.lines().count();
                if !write_all_checked(stdout_fd, &redraw) { break; }
            }
            protocol::write_all_fd(stdout_fd, status::SHOW_CURSOR);
            protocol::write_all_fd(signals_fd, &[protocol::signals::EXIT, 0x01, 0x00]);
            unsafe { libc::close(stdout_fd); libc::close(signals_fd); }
            if let Some(conductor) = self_ref.upgrade() {
                if let Ok(mut c) = conductor.lock() {
                    c.live_status_clients.remove(&pid);
                }
            }
        });

        Ok(())
    }

    /// Open a fresh stdin/stdout/stderr/signals socket quad for `client_fd`
    /// (sending it the paths/ports to connect to) and return the connected
    /// fds once the client has dialed back in, plus the allocated port-pool
    /// index (TCP transport only). Shared by serve_string (--help/--version)
    /// and serve_status (--status), which otherwise differ only in what they
    /// do with the resulting fds.
    fn open_client_streams(&mut self, client_fd: RawFd) -> io::Result<([i32; 4], Option<u16>)> {
        let rdir = self.config.runtime_dir.clone();
        let transport = self.config.transport;
        let bind = self.config.bind_address.clone();

        let port_set_idx = self.port_pool.as_mut().and_then(|p| p.allocate());
        let ports: Option<[u16; 4]> = port_set_idx.and_then(|idx| {
            self.port_pool.as_ref().map(|p| p.ports_for_index(idx))
        });

        let suffixes = ["stdin.sock", "stdout.sock", "stderr.sock", "signals.sock"];
        let mut paths = [String::new(), String::new(), String::new(), String::new()];
        let mut unix_listeners: Vec<(UnixListener, usize)> = Vec::new();
        let mut tcp_listeners: Vec<(TcpListener, usize)> = Vec::new();

        for (i, suffix) in suffixes.iter().enumerate() {
            if transport == TransportMode::Tcp {
                let port = ports.map_or(0, |p| p[i]);
                let addr = protocol::parse_host_port(&format!("{}:{}", bind, port))
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                let l = TcpListener::bind(addr)?;
                let actual = l.local_addr()?.port();
                paths[i] = format!("{}:{}", bind, actual);
                tcp_listeners.push((l, i));
            } else {
                let path = protocol::random_socket_path(&rdir, suffix);
                let l = UnixListener::bind(&path)?;
                paths[i] = path;
                unix_listeners.push((l, i));
            }
        }

        self.send_socket_paths_raw(client_fd, &paths);

        let mut conn_fds = [0i32; 4];
        if transport == TransportMode::Unix {
            for (l, i) in &unix_listeners {
                let (s, _) = l.accept()?;
                use std::os::unix::io::IntoRawFd;
                conn_fds[*i] = s.into_raw_fd();
                let _ = std::fs::remove_file(&paths[*i]);
            }
        } else {
            for (l, i) in &tcp_listeners {
                let (s, _) = l.accept()?;
                use std::os::unix::io::IntoRawFd;
                conn_fds[*i] = s.into_raw_fd();
            }
        }

        Ok((conn_fds, port_set_idx))
    }

    fn release_port_set_idx(&mut self, idx: Option<u16>) {
        if let Some(idx) = idx {
            if let Some(pool) = &mut self.port_pool { pool.release(idx); }
        }
    }

    fn send_socket_paths(&self, fd: RawFd, paths: &SocketPaths) {
        let all = [&paths.stdin, &paths.stdout, &paths.stderr, &paths.signals];
        let strs: [String; 4] = std::array::from_fn(|i| {
            let p = all[i];
            if self.config.transport == TransportMode::Tcp {
                p.rfind(':').map(|pos| p[pos..].to_string()).unwrap_or_else(|| p.clone())
            } else {
                p.clone()
            }
        });
        self.send_socket_paths_raw(fd, &strs);
    }

    fn send_socket_paths_raw(&self, fd: RawFd, paths: &[String; 4]) {
        let mut buf = Vec::with_capacity(1024);
        for path in paths {
            let b = path.as_bytes();
            buf.extend_from_slice(&(b.len() as u16).to_le_bytes());
            buf.extend_from_slice(b);
        }
        protocol::write_all_fd(fd, &buf);
    }
}

// --- Free helpers ---

/// Like protocol::write_all_fd, but reports failure (e.g. broken pipe once
/// the client's gone) instead of silently swallowing it — used by the live
/// status redraw loop to know when to stop.
fn write_all_checked(fd: RawFd, buf: &[u8]) -> bool {
    let mut written = 0;
    while written < buf.len() {
        let n = unsafe {
            libc::write(fd, buf[written..].as_ptr() as *const libc::c_void, buf.len() - written)
        };
        if n <= 0 { return false; }
        written += n as usize;
    }
    true
}

fn read_len_prefixed_str(fd: RawFd) -> io::Result<String> {
    let bytes = read_len_prefixed_bytes(fd)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_len_prefixed_bytes(fd: RawFd) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    protocol::read_exact_fd(fd, &mut len_buf)?;
    let len = u16::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    if len > 0 { protocol::read_exact_fd(fd, &mut buf)?; }
    Ok(buf)
}

fn read_full_env(fd: RawFd) -> io::Result<Vec<EnvVar>> {
    let mut count_buf = [0u8; 2];
    protocol::read_exact_fd(fd, &mut count_buf)?;
    let count = u16::from_le_bytes(count_buf) as usize;
    let mut env = Vec::with_capacity(count);
    for _ in 0..count {
        let key   = String::from_utf8_lossy(&read_len_prefixed_bytes(fd)?).into_owned();
        let value = String::from_utf8_lossy(&read_len_prefixed_bytes(fd)?).into_owned();
        env.push(EnvVar { key, value });
    }
    Ok(env)
}

fn make_worker_key(project: &str, channel: Option<&str>, tkey: u32) -> String {
    if let Some(ch) = channel { format!("{}\x00{}\x00{}", project, ch, tkey) }
    else { format!("{}\x00\x00{}", project, tkey) }
}

/// Resolve the effective --threads spec for a client request: the explicit
/// switch if given, else the client's own JULIA_NUM_THREADS env var, else
/// unset (worker gets Julia's own default).
fn resolve_threads(request: &ClientRequest) -> args::Threads {
    let spec = request.parsed.thread_switch();
    if spec != args::THREADS_NONE { return spec; }
    request.env.iter()
        .find(|e| e.key == "JULIA_NUM_THREADS")
        .map(|e| args::parse_threads(&e.value))
        .unwrap_or(args::THREADS_NONE)
}

fn trim_trailing_slashes(s: &str) -> &str {
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() { &s[..s.len().min(1)] } else { trimmed }
}

fn path_covered_by(path: &str, dirs: &[&str]) -> bool {
    let p = trim_trailing_slashes(path);
    for &raw_d in dirs {
        let d = trim_trailing_slashes(raw_d);
        if d == p { return true; }
        if p.len() > d.len() && p.starts_with(d) && p.as_bytes().get(d.len()) == Some(&b'/') {
            return true;
        }
    }
    false
}

fn build_sandbox_env(env: &[EnvVar]) -> Vec<EnvVar> {
    const IDENTITY_KEYS: &[&str] = &["HOME", "USER", "LOGNAME"];
    let mut result: Vec<EnvVar> = env.iter()
        .filter(|e| !IDENTITY_KEYS.contains(&e.key.as_str()))
        .cloned()
        .collect();
    for (k, v) in [("HOME", "/home/sandbox"), ("USER", "sandbox"), ("LOGNAME", "sandbox")] {
        result.push(EnvVar { key: k.to_string(), value: v.to_string() });
    }
    result
}
