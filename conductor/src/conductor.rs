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
use crate::worker::{ClientInfo, Worker, SocketPaths, unix_time};

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
         --sandbox                  Run in an isolated sandbox (Linux only)\n\n\
        {}\n",
        daemon_mgmt
    )
}

// --- Active client tracking ---

struct ActiveClientInfo {
    worker_id: u32,
    client_num: u32,
    start_time_us: i64,
    port_set: u16,
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
    workers: std::collections::HashMap<String, Vec<Worker>>,
    active_clients: std::collections::HashMap<u32, ActiveClientInfo>,
    port_pool: Option<PortPool>,
    reserve: Option<Worker>,
    next_worker_id: u32,
    client_counter: u32,
    env_cache: EnvCache,
    pub socket_path: String,
}

impl Conductor {
    pub fn new(config: Config) -> Self {
        let port_pool = config.port_range.map(|(base, count)| PortPool::new(base, count));
        let socket_path = config.socket_path.clone();
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
        }
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
        let mut w = Worker::spawn(&self.config, id, &self.config.runtime_dir, julia_channel, false)?;
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
                if let Some(w_id) = self.client_done(pid) {
                    self.maybe_health_check_worker(w_id);
                }
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

        // Worker key
        let project_path = request.project.as_deref().unwrap_or("").to_string();
        let julia_channel = request.parsed.julia_channel.clone();
        let worker_key = match sandbox {
            SandboxMode::None => make_worker_key(&project_path, julia_channel.as_deref()),
            SandboxMode::Remote => julia_channel.as_ref()
                .map(|ch| format!("__sandbox__\x00{}", ch))
                .unwrap_or_else(|| "__sandbox__".to_string()),
            SandboxMode::Local => {
                let cwd = trim_trailing_slashes(&request.cwd).to_string();
                let proj = trim_trailing_slashes(&project_path).to_string();
                let is_named = !proj.is_empty() && proj.starts_with('@');
                let has_local = !proj.is_empty() && !is_named;
                let rw_mount = if has_local && path_covered_by(&cwd, &[&proj]) { proj.clone() } else { cwd.clone() };
                if let Some(ch) = &julia_channel {
                    format!("__lsandbox__\x00{}\x00{}\x00{}", rw_mount, proj, ch)
                } else {
                    format!("__lsandbox__\x00{}\x00{}", rw_mount, proj)
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

        self.assign_client_to_worker(fd, &request, &worker_key, sandbox, client_num)
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
        worker_key: &str, sandbox: SandboxMode, client_num: u32,
    ) -> io::Result<()> {
        let session_label = request.parsed.get_switch("--session").map(|s| s.to_string());
        let is_labeled = session_label.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        let want_interactive = request.parsed.has_switch("-i");
        let now = unix_time();

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
            id
        } else {
            // Spawn new worker
            let project_path = request.project.as_deref().unwrap_or("").to_string();
            let julia_channel = request.parsed.julia_channel.as_deref().map(|s| s.to_string());
            let new_id = self.spawn_worker_for_key(worker_key, &project_path, julia_channel.as_deref(), want_interactive, sandbox)?;
            if is_labeled {
                if let Some(label) = &session_label {
                    if let Some(w) = self.get_worker_mut(new_id) {
                        if w.session_label.is_none() {
                            eprintln!("Worker {}: assigning label '{}'", new_id, label);
                            w.session_label = Some(label.clone());
                        }
                    }
                }
            }
            new_id
        };

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

        self.register_client(request.pid, client_num, w_id, port_set);
        eprintln!("Client {}: done (assigned to worker {})", client_num, w_id);
        self.send_socket_paths(fd, &paths);
        Ok(())
    }

    fn assign_client_by_worker_id(&mut self, fd: RawFd, request: &ClientRequest, w_id: u32) -> io::Result<()> {
        let port_set = self.allocate_port_set()?;
        let is_labeled = request.parsed.get_switch("--session").map(|s| !s.is_empty()).unwrap_or(false);
        let cwd = if !self.config.host_home.is_empty() { self.config.host_home.clone() } else { "/".to_string() };

        let maxclients = self.config.worker_maxclients;
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

        self.register_client(request.pid, self.client_counter, w_id, port_set);
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
        interactive: bool, sandbox: SandboxMode,
    ) -> io::Result<u32> {
        match sandbox {
            SandboxMode::None => self.add_worker_to_pool(key, project, julia_channel, interactive),
            #[cfg(target_os = "linux")]
            SandboxMode::Remote => self.add_sandboxed_worker(key, project, julia_channel, &[], &[]),
            #[cfg(target_os = "linux")]
            SandboxMode::Local => {
                let rw = vec![project.to_string()];
                self.add_sandboxed_worker(key, project, julia_channel, &[], &rw)
            }
            _ => Err(io::Error::new(io::ErrorKind::Unsupported, "sandbox requires Linux")),
        }
    }

    fn add_worker_to_pool(
        &mut self, key: &str, project: &str, julia_channel: Option<&str>, interactive: bool,
    ) -> io::Result<u32> {
        let can_use_reserve = !interactive && self.reserve.as_ref().map_or(false, |r| {
            r.julia_channel.as_deref() == julia_channel
        });

        let id = if can_use_reserve {
            let mut reserve = self.reserve.take().unwrap();
            let id = reserve.id;
            eprintln!("Assigning reserve worker {} to project {}", id, project);
            let list = self.workers.entry(key.to_string()).or_default();
            list.push(reserve);
            id
        } else {
            let id = self.next_worker_id;
            self.next_worker_id += 1;
            let w = Worker::spawn(&self.config, id, &self.config.runtime_dir, julia_channel, interactive)?;
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
        &mut self, key: &str, project: &str, julia_channel: Option<&str>,
        extra_ro: &[String], extra_rw: &[String],
    ) -> io::Result<u32> {
        let id = self.next_worker_id;
        self.next_worker_id += 1;
        let environ: std::collections::HashMap<String, String> = std::env::vars().collect();
        let mut ro = vec![self.config.worker_project.clone()];
        ro.extend_from_slice(extra_ro);
        let w = crate::worker::Worker::spawn_sandboxed(
            &self.config, id, &self.config.runtime_dir,
            julia_channel, &environ, &ro, extra_rw,
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
        if let Some(list) = self.workers.remove(key) {
            let count = list.len();
            for mut w in list {
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

    fn register_client(&mut self, pid: u32, client_num: u32, worker_id: u32, port_set: u16) {
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        self.active_clients.insert(pid, ActiveClientInfo { worker_id, client_num, start_time_us: now_us, port_set });
        if let Some(w) = self.get_worker_mut(worker_id) {
            w.active_clients += 1;
        }
    }

    fn client_done(&mut self, pid: u32) -> Option<u32> {
        let info = self.active_clients.remove(&pid)?;
        self.release_port_set(info.port_set);
        let w_id = info.worker_id;
        if let Some(w) = self.get_worker_mut(w_id) {
            if w.active_clients > 0 { w.active_clients -= 1; }
            w.last_active = unix_time();
        }
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let dur_us = now_us - info.start_time_us;
        eprintln!("Client {} disconnected; worker: {}, duration: {}.{:03}s",
            info.client_num, w_id, dur_us / 1_000_000, (dur_us % 1_000_000) / 1_000);
        let active = self.get_worker_mut(w_id).map(|w| w.active_clients).unwrap_or(1);
        if active == 0 { Some(w_id) } else { None }
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
            }
            Err(_) => self.kill_unresponsive_worker(w_id),
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

        protocol::write_all_fd(conn_fds[1], content.as_bytes());
        protocol::write_all_fd(conn_fds[3], &[protocol::signals::EXIT, 0x01, 0x00]);
        for &fd in &conn_fds { unsafe { libc::close(fd); } }

        if let Some(idx) = port_set_idx {
            if let Some(pool) = &mut self.port_pool { pool.release(idx); }
        }
        Ok(())
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

fn make_worker_key(project: &str, channel: Option<&str>) -> String {
    if let Some(ch) = channel { format!("{}\x00{}", project, ch) }
    else { project.to_string() }
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
