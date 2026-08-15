use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::io::AsRawFd;
use std::time::Duration;
use std::process::{Child, Command, Stdio};

use crate::args::{Switch, Threads};
use crate::config::Config;
use crate::env_cache::EnvVar;
use crate::protocol::{self, worker as proto_worker};

const MAX_RECENT_PPIDS: usize = 32;

// --- Occupancy: EWMA of busy-fraction (attached-client time / wall time) ---
//
// Two independent instances per worker: `fast` (short half-life) drives
// pressure-eviction ranking and --status display; `slow` (long half-life)
// drives the idle-budget calculation. Both are frozen (not decayed further)
// once a worker goes idle — see Worker::idle_budget, which reads `slow` as of
// `last_active` rather than "now".
#[derive(Clone, Copy, Debug)]
pub struct Ewma {
    value: f64,
    last_t: i64,
    busy_since: Option<i64>,
}

impl Ewma {
    fn new(now: i64) -> Self {
        Ewma { value: 0.0, last_t: now, busy_since: None }
    }

    fn update_to(&mut self, now: i64, half_life: f64) {
        let dt = (now - self.last_t) as f64;
        if dt <= 0.0 { return; }
        let busy_frac = if self.busy_since.is_some() { 1.0 } else { 0.0 };
        let decay = if half_life > 0.0 { 2f64.powf(-dt / half_life) } else { 0.0 };
        self.value = self.value * decay + busy_frac * (1.0 - decay);
        self.last_t = now;
    }

    pub fn attach(&mut self, now: i64, half_life: f64) {
        self.update_to(now, half_life);
        self.busy_since = Some(now);
    }

    pub fn detach(&mut self, now: i64, half_life: f64) {
        self.update_to(now, half_life);
        self.busy_since = None;
    }

    /// Decayed value as of `now`.
    pub fn read(&mut self, now: i64, half_life: f64) -> f64 {
        self.update_to(now, half_life);
        self.value
    }

    /// Decayed value as of `now`, without mutating stored state. `now` is
    /// normally <= the last update time (e.g. reading a detached worker's
    /// occupancy as of when it went idle), in which case this is just the
    /// frozen value — occupancy intentionally isn't decayed further purely
    /// by the passage of idle time.
    pub fn peek(&self, now: i64, half_life: f64) -> f64 {
        let dt = (now - self.last_t) as f64;
        if dt <= 0.0 { return self.value; }
        let busy_frac = if self.busy_since.is_some() { 1.0 } else { 0.0 };
        let decay = if half_life > 0.0 { 2f64.powf(-dt / half_life) } else { 0.0 };
        self.value * decay + busy_frac * (1.0 - decay)
    }
}

// --- Crf: recency-frequency score + Jacobson/RFC6298 inter-summon interval estimator ---
//
// Tracked per worker *pool key* (not per worker instance — a key can outlive
// any individual worker as workers are recycled). `value` is an LRFU-style
// score that grows on each summon and decays between them; `srtt`/`rttvar`
// estimate the typical gap between summons (like TCP's RTO), giving an
// adaptive idle budget: keys summoned often/regularly get long budgets, cold
// keys decay quickly.
#[derive(Clone, Copy, Debug, Default)]
pub struct Crf {
    value: f64,
    srtt: f64,
    rttvar: f64,
    last_summon: Option<i64>,
    summons: u32,
}

impl Crf {
    pub fn bump(&mut self, now: i64, half_life: f64) {
        if let Some(last) = self.last_summon {
            let gap = (now - last) as f64;
            let decay = if half_life > 0.0 { 2f64.powf(-gap / half_life) } else { 0.0 };
            self.value = 1.0 + self.value * decay;
            if self.summons == 1 {
                self.srtt = gap;
                self.rttvar = gap / 2.0;
            } else if self.summons > 1 {
                let err = gap - self.srtt;
                self.srtt += err / 8.0;
                self.rttvar += (err.abs() - self.rttvar) / 4.0;
            }
        } else {
            self.value = 1.0;
        }
        self.last_summon = Some(now);
        self.summons += 1;
    }

    /// Decayed value as of `now`, without mutating stored state.
    pub fn read(&self, now: i64, half_life: f64) -> f64 {
        match self.last_summon {
            None => 0.0,
            Some(last) => {
                let gap = (now - last) as f64;
                let decay = if half_life > 0.0 { 2f64.powf(-gap / half_life) } else { 0.0 };
                self.value * decay
            }
        }
    }

    /// RFC6298-style RTO estimate of the typical inter-summon gap.
    pub fn interval_budget(&self) -> f64 {
        if self.summons < 2 { return 0.0; }
        self.srtt + 4.0 * self.rttvar
    }
}

// --- Process handle: either a Command-spawned child or a raw fork()ed PID ---

pub enum ProcessHandle {
    Spawned(Child),
    #[allow(dead_code)]
    Forked(libc::pid_t),
}

impl ProcessHandle {
    pub fn pid(&self) -> u32 {
        match self {
            ProcessHandle::Spawned(c) => c.id(),
            ProcessHandle::Forked(p) => *p as u32,
        }
    }

    pub fn kill(&mut self, sig: libc::c_int) {
        let pid = self.pid() as libc::pid_t;
        unsafe { libc::kill(pid, sig); }
    }

    pub fn try_wait(&mut self) -> Option<bool> {
        match self {
            ProcessHandle::Spawned(c) => c.try_wait().ok().map(|s| s.is_some()),
            ProcessHandle::Forked(p) => {
                let mut status = 0;
                let r = unsafe { libc::waitpid(*p, &mut status, libc::WNOHANG) };
                if r == *p { Some(true) }
                else if r == 0 { Some(false) }
                else { None }
            }
        }
    }
}

pub struct ClientInfo<'a> {
    pub tty: bool,
    pub force: bool,
    pub pid: u32,
    pub ppid: u32,
    pub cwd: &'a str,
    pub env: &'a [EnvVar],
    pub switches: &'a [Switch],
    pub program_file: Option<&'a str>,
    pub program_args: &'a [String],
    pub port_set: u16,
}

pub struct SocketPaths {
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
    pub signals: String,
}

pub struct Worker {
    pub id: u32,
    pub process: ProcessHandle,
    pub socket: UnixStream,
    pub project: Option<String>,
    pub julia_channel: Option<String>,
    pub threads: Threads,
    pub session_label: Option<String>,
    pub created_at: i64,
    pub last_active: i64,
    pub last_pinged: i64,
    pub ping_pending: bool,
    pub active_clients: u32,
    pub sandboxed: bool,
    pub interactive: bool,
    pub recent_ppids: [u32; MAX_RECENT_PPIDS],
    pub recent_ppids_next: usize,
    // --- pressure/status sampling (see refresh_stats) ---
    pub occ_fast: Ewma,
    pub occ_slow: Ewma,
    pub mem_bytes: u64,
    pub cpu_pct: f64,
    cpu_last_ticks: u64,
    cpu_last_sample_at: i64, // microseconds
    // --- staged retirement (soft_exit -> grace -> SIGTERM -> grace -> SIGKILL) ---
    pub retire_stage: RetireStage,
    pub retire_since: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RetireStage {
    Running,
    Soft,
    Term,
}

impl Worker {
    pub fn spawn(
        cfg: &Config,
        id: u32,
        runtime_dir: &str,
        julia_channel: Option<&str>,
        interactive: bool,
        threads: Threads,
    ) -> io::Result<Self> {
        Self::spawn_impl(cfg, id, runtime_dir, julia_channel, interactive, threads, false, None, &[], &[])
    }

    #[cfg(target_os = "linux")]
    pub fn spawn_sandboxed(
        cfg: &Config,
        id: u32,
        runtime_dir: &str,
        julia_channel: Option<&str>,
        threads: Threads,
        environ: &std::collections::HashMap<String, String>,
        extra_ro: &[String],
        extra_rw: &[String],
    ) -> io::Result<Self> {
        Self::spawn_impl(cfg, id, runtime_dir, julia_channel, false, threads, true,
            Some(environ), extra_ro, extra_rw)
    }

    fn spawn_impl(
        cfg: &Config,
        id: u32,
        runtime_dir: &str,
        julia_channel: Option<&str>,
        interactive: bool,
        threads: Threads,
        sandboxed: bool,
        _environ: Option<&std::collections::HashMap<String, String>>,
        _extra_ro: &[String],
        _extra_rw: &[String],
    ) -> io::Result<Self> {
        // Sandboxed workers use a per-worker subdir for isolation
        let effective_runtime_dir = if sandboxed {
            #[cfg(target_os = "linux")]
            {
                let subdir = format!("{}/sandbox-{}", runtime_dir, id);
                std::fs::create_dir_all(&subdir)?;
                subdir
            }
            #[cfg(not(target_os = "linux"))]
            runtime_dir.to_string()
        } else {
            runtime_dir.to_string()
        };

        let setup_path = protocol::random_socket_path(&effective_runtime_dir, "wsetup.sock");
        let setup_listener = UnixListener::bind(&setup_path)?;
        // Worker connects back within a reasonable startup time
        setup_listener.set_nonblocking(false)?;

        let eval_expr = format!(
            "using DaemonWorker; DaemonWorker.runworker(\"{}\", {}, \"{}\")",
            setup_path, id, cfg.socket_path
        );

        let process = if sandboxed {
            #[cfg(target_os = "linux")]
            {
                // Resolve executable path for sandbox execve
                let exe_path = resolve_in_path(&cfg.worker_executable)
                    .unwrap_or_else(|| cfg.worker_executable.clone());

                let mut ro_binds: Vec<String> = vec![cfg.worker_project.clone()];
                ro_binds.extend_from_slice(_extra_ro);

                let sandbox_cfg = crate::sandbox::SandboxConfig {
                    julia_executable: exe_path,
                    julia_channel: julia_channel.map(|s| s.to_string()),
                    worker_project: cfg.worker_project.clone(),
                    worker_args: cfg.worker_args.clone(),
                    threads_arg: crate::args::render_threads(threads),
                    eval_expr: eval_expr.clone(),
                    host_environ: _environ.cloned().unwrap_or_default(),
                    setup_socket_path: setup_path.clone(),
                    worker_id: id,
                    host_home: cfg.host_home.clone(),
                    extra_ro_binds: ro_binds,
                    extra_rw_binds: _extra_rw.to_vec(),
                    empty_environment: cfg.sandbox_empty_environment,
                    max_memory: cfg.sandbox_max_memory.clone(),
                    max_cpu: cfg.sandbox_max_cpu,
                };
                let pid = crate::sandbox::spawn_sandboxed(&sandbox_cfg)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("sandbox: {:?}", e)))?;
                ProcessHandle::Forked(pid)
            }
            #[cfg(not(target_os = "linux"))]
            return Err(io::Error::new(io::ErrorKind::Unsupported, "sandbox requires Linux"))
        } else {
            let mut argv: Vec<String> = vec![cfg.worker_executable.clone()];
            if let Some(ch) = julia_channel {
                argv.push(ch.to_string());
            }
            if !cfg.worker_project.is_empty() {
                argv.push(format!("--project={}", cfg.worker_project));
            }
            for arg in cfg.worker_args.split_whitespace() {
                argv.push(arg.to_string());
            }
            if let Some(t) = crate::args::render_threads(threads) {
                argv.push(format!("--threads={}", t));
            }
            if interactive { argv.push("-i".to_string()); }
            argv.push("--eval".to_string());
            argv.push(eval_expr);

            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            // Separate process group so terminal SIGINT goes only to conductor
            #[cfg(unix)]
            unsafe {
                use std::os::unix::process::CommandExt;
                cmd.pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
            ProcessHandle::Spawned(cmd.spawn()?)
        };

        // Accept worker's connection (Julia connects back on startup)
        let (worker_stream, _) = setup_listener.accept()?;
        let _ = std::fs::remove_file(&setup_path);

        // Set recv timeout so the conductor doesn't block forever if worker hangs
        worker_stream.set_read_timeout(Some(Duration::from_secs(cfg.ping_timeout)))?;

        // Send magic to establish protocol
        let magic = proto_worker::MAGIC.to_le_bytes();
        write_all(&worker_stream, &magic)?;

        let now = unix_time();
        Ok(Worker {
            id,
            process,
            socket: worker_stream,
            project: None,
            julia_channel: julia_channel.map(|s| s.to_string()),
            threads,
            session_label: None,
            created_at: now,
            last_active: now,
            last_pinged: now,
            ping_pending: false,
            active_clients: 0,
            sandboxed,
            interactive,
            recent_ppids: [0; MAX_RECENT_PPIDS],
            recent_ppids_next: 0,
            occ_fast: Ewma::new(now),
            occ_slow: Ewma::new(now),
            mem_bytes: 0,
            cpu_pct: 0.0,
            cpu_last_ticks: 0,
            cpu_last_sample_at: 0,
            retire_stage: RetireStage::Running,
            retire_since: 0,
        })
    }

    pub fn record_ppid(&mut self, ppid: u32, max_history: u32) {
        let cap = if max_history == 0 { MAX_RECENT_PPIDS } else { (max_history as usize).min(MAX_RECENT_PPIDS) };
        self.recent_ppids[self.recent_ppids_next] = ppid;
        self.recent_ppids_next = (self.recent_ppids_next + 1) % cap;
    }

    pub fn should_ping(&self, now: i64, interval: u64) -> bool {
        !self.ping_pending
            && self.active_clients == 0
            && (now - self.last_pinged) >= interval as i64
    }

    fn write_header(&self, msg_type: proto_worker::MessageType, payload_len: u16) -> io::Result<()> {
        let buf = [msg_type as u8, payload_len as u8, (payload_len >> 8) as u8];
        write_all(&self.socket, &buf)
    }

    fn read_header(&self) -> io::Result<(proto_worker::MessageType, u16)> {
        let mut buf = [0u8; 3];
        read_exact(&self.socket, &mut buf)?;
        let msg = proto_worker::MessageType::from_u8(buf[0]);
        let len = u16::from_le_bytes([buf[1], buf[2]]);
        Ok((msg, len))
    }

    pub fn ping(&self) -> io::Result<[u8; 5]> {
        // Temporarily raise timeout to ping_timeout for the response
        self.write_header(proto_worker::MessageType::Ping, 0)?;
        let (msg, _) = self.read_header()?;
        if msg != proto_worker::MessageType::Pong {
            return Err(io::Error::new(io::ErrorKind::Other, "expected pong"));
        }
        let mut pong_buf = [0u8; 5];
        pong_buf[0] = msg as u8;
        read_exact(&self.socket, &mut pong_buf[3..])?;
        Ok(pong_buf)
    }

    pub fn set_project(&mut self, project: String) -> io::Result<()> {
        let payload_len = 2 + project.len() as u16;
        self.write_header(proto_worker::MessageType::SetProject, payload_len)?;
        let len_bytes = (project.len() as u16).to_le_bytes();
        write_all(&self.socket, &len_bytes)?;
        write_all(&self.socket, project.as_bytes())?;
        let (msg, _) = self.read_header()?;
        if msg == proto_worker::MessageType::Err {
            return Err(io::Error::new(io::ErrorKind::Other, "worker returned error for set_project"));
        }
        if msg != proto_worker::MessageType::ProjectOk {
            return Err(io::Error::new(io::ErrorKind::Other, "unexpected response to set_project"));
        }
        self.project = Some(project);
        Ok(())
    }

    /// Tell the worker to tear down a named session's REPL/Main-module state
    /// (its label's `--session` scope), typically right before the label is
    /// reassigned to a different client. Fire-and-forget: no response is read.
    pub fn drop_session(&self, label: &str) -> io::Result<()> {
        let payload_len = 2 + label.len() as u16;
        self.write_header(proto_worker::MessageType::DropSession, payload_len)?;
        write_all(&self.socket, &(label.len() as u16).to_le_bytes())?;
        write_all(&self.socket, label.as_bytes())
    }

    /// Ask the worker for its live client PID list (used to reconcile
    /// `active_clients` after a lost `client_done` notification — a stronger
    /// repair than the count-only `sync_clients` push).
    pub fn query_clients(&self) -> io::Result<Vec<u32>> {
        self.write_header(proto_worker::MessageType::QueryClients, 0)?;
        let (msg, _) = self.read_header()?;
        if msg != proto_worker::MessageType::Clients {
            return Err(io::Error::new(io::ErrorKind::Other, "expected clients response"));
        }
        let mut count_buf = [0u8; 2];
        read_exact(&self.socket, &mut count_buf)?;
        let count = u16::from_le_bytes(count_buf) as usize;
        let mut pids = Vec::with_capacity(count);
        for _ in 0..count {
            let mut buf = [0u8; 4];
            read_exact(&self.socket, &mut buf)?;
            pids.push(u32::from_le_bytes(buf));
        }
        Ok(pids)
    }

    /// Sample RSS (from /proc/[pid]/statm) and CPU% (from /proc/[pid]/stat,
    /// as a delta over the last sample) for the worker process. Best-effort:
    /// leaves prior values in place if /proc is unreadable (e.g. process
    /// just exited, or a sandboxed worker whose PID lives in another
    /// namespace and isn't visible under our /proc).
    pub fn refresh_stats(&mut self, now_us: i64) {
        let pid = self.process.pid();
        if let Some(rss) = read_rss_bytes(pid) {
            self.mem_bytes = rss;
        }
        if let Some(ticks) = read_cpu_ticks(pid) {
            if self.cpu_last_sample_at > 0 {
                let dt_us = (now_us - self.cpu_last_sample_at).max(1) as f64;
                let dt_ticks = ticks.saturating_sub(self.cpu_last_ticks) as f64;
                let hz = clock_ticks_per_sec() as f64;
                self.cpu_pct = (dt_ticks / hz) / (dt_us / 1_000_000.0) * 100.0;
            }
            self.cpu_last_ticks = ticks;
            self.cpu_last_sample_at = now_us;
        }
    }

    pub fn soft_exit(&self) {
        let _ = self.write_header(proto_worker::MessageType::SoftExit, 0);
    }

    pub fn sync_clients(&self, pids: &[u32]) -> io::Result<u16> {
        let payload_len = 2 + (pids.len() as u16) * 4;
        self.write_header(proto_worker::MessageType::SyncClients, payload_len)?;
        let count = (pids.len() as u16).to_le_bytes();
        write_all(&self.socket, &count)?;
        for &pid in pids {
            write_all(&self.socket, &pid.to_le_bytes())?;
        }
        let (msg, _) = self.read_header()?;
        if msg != proto_worker::MessageType::Ack {
            return Err(io::Error::new(io::ErrorKind::Other, "expected ack from sync_clients"));
        }
        let mut count_buf = [0u8; 2];
        read_exact(&self.socket, &mut count_buf)?;
        Ok(u16::from_le_bytes(count_buf))
    }

    pub fn run_client(&mut self, info: &ClientInfo) -> io::Result<SocketPaths> {
        // Build payload
        let flags = proto_worker::Flags::new(info.tty, info.force);
        let pf_len = info.program_file.map(|p| 2 + p.len()).unwrap_or(0);
        let mut payload_size = 1 + 4 + 2 + info.cwd.len() + 2 + 2 + 1 + pf_len + 2 + 2;
        for e in info.env { payload_size += 4 + e.key.len() + e.value.len(); }
        for sw in info.switches { payload_size += 4 + sw.name.len() + sw.value.len(); }
        for arg in info.program_args { payload_size += 2 + arg.len(); }

        let mut buf = vec![0u8; payload_size];
        let mut pos = 0;

        put_u8(&mut buf, &mut pos, flags);
        put_u32(&mut buf, &mut pos, info.pid);
        put_len_prefixed(&mut buf, &mut pos, info.cwd.as_bytes());
        put_u16(&mut buf, &mut pos, info.env.len() as u16);
        for e in info.env {
            put_len_prefixed(&mut buf, &mut pos, e.key.as_bytes());
            put_len_prefixed(&mut buf, &mut pos, e.value.as_bytes());
        }
        put_u16(&mut buf, &mut pos, info.switches.len() as u16);
        for sw in info.switches {
            put_len_prefixed(&mut buf, &mut pos, sw.name.as_bytes());
            put_len_prefixed(&mut buf, &mut pos, sw.value.as_bytes());
        }
        if let Some(pf) = info.program_file {
            put_u8(&mut buf, &mut pos, 1);
            put_len_prefixed(&mut buf, &mut pos, pf.as_bytes());
        } else {
            put_u8(&mut buf, &mut pos, 0);
        }
        put_u16(&mut buf, &mut pos, info.program_args.len() as u16);
        for arg in info.program_args {
            put_len_prefixed(&mut buf, &mut pos, arg.as_bytes());
        }
        put_u16(&mut buf, &mut pos, info.port_set);

        eprintln!("Worker {}: sending client_run ({} bytes)", self.id, payload_size);
        self.write_header(proto_worker::MessageType::ClientRun, payload_size as u16)?;
        write_all(&self.socket, &buf[..pos])?;

        eprintln!("Worker {}: waiting for response...", self.id);
        let (msg, payload_len) = self.read_header()?;
        eprintln!("Worker {}: got response {:?} ({} bytes)", self.id, msg, payload_len);

        if msg == proto_worker::MessageType::Err {
            // Try to read error details
            if payload_len > 0 && payload_len < 4096 {
                let mut err_buf = vec![0u8; payload_len as usize];
                let _ = read_exact(&self.socket, &mut err_buf);
                if err_buf.len() >= 4 {
                    let code = u16::from_le_bytes([err_buf[0], err_buf[1]]);
                    let msg_len = u16::from_le_bytes([err_buf[2], err_buf[3]]) as usize;
                    if 4 + msg_len <= err_buf.len() {
                        let msg_str = std::str::from_utf8(&err_buf[4..4+msg_len]).unwrap_or("?");
                        eprintln!("Worker {}: error (code {}): {}", self.id, code, msg_str);
                    }
                }
            }
            return Err(io::Error::new(io::ErrorKind::Other, "worker returned error"));
        }

        if msg != proto_worker::MessageType::Sockets {
            return Err(io::Error::new(io::ErrorKind::Other, "expected sockets response"));
        }

        let mut payload = vec![0u8; payload_len as usize];
        read_exact(&self.socket, &mut payload)?;

        let mut rpos = 0;
        self.active_clients = u32::from_le_bytes(payload[rpos..rpos+4].try_into().unwrap());
        rpos += 4;

        let stdin_len = u16::from_le_bytes(payload[rpos..rpos+2].try_into().unwrap()) as usize;
        rpos += 2;
        if stdin_len == 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "WorkerBusy"));
        }
        let stdin = std::str::from_utf8(&payload[rpos..rpos+stdin_len]).unwrap_or("").to_string();
        rpos += stdin_len;

        let stdout_len = u16::from_le_bytes(payload[rpos..rpos+2].try_into().unwrap()) as usize;
        rpos += 2;
        let stdout = std::str::from_utf8(&payload[rpos..rpos+stdout_len]).unwrap_or("").to_string();
        rpos += stdout_len;

        let stderr_len = u16::from_le_bytes(payload[rpos..rpos+2].try_into().unwrap()) as usize;
        rpos += 2;
        let stderr = std::str::from_utf8(&payload[rpos..rpos+stderr_len]).unwrap_or("").to_string();
        rpos += stderr_len;

        let signals_len = u16::from_le_bytes(payload[rpos..rpos+2].try_into().unwrap()) as usize;
        rpos += 2;
        let signals = std::str::from_utf8(&payload[rpos..rpos+signals_len]).unwrap_or("").to_string();

        eprintln!("Worker {}: sockets: in={} out={} err={} sig={}", self.id, stdin, stdout, stderr, signals);
        Ok(SocketPaths { stdin, stdout, stderr, signals })
    }
}

// --- Helpers ---

fn write_all(stream: &UnixStream, buf: &[u8]) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let mut written = 0;
    while written < buf.len() {
        let n = unsafe {
            libc::write(fd, buf[written..].as_ptr() as *const _, buf.len() - written)
        };
        if n <= 0 {
            return Err(io::Error::last_os_error());
        }
        written += n as usize;
    }
    Ok(())
}

fn read_exact(stream: &UnixStream, buf: &mut [u8]) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let mut read = 0;
    while read < buf.len() {
        let n = unsafe {
            libc::read(fd, buf[read..].as_mut_ptr() as *mut _, buf.len() - read)
        };
        if n <= 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "end of stream"));
        }
        read += n as usize;
    }
    Ok(())
}

fn put_u8(buf: &mut [u8], pos: &mut usize, v: u8) {
    buf[*pos] = v;
    *pos += 1;
}

fn put_u16(buf: &mut [u8], pos: &mut usize, v: u16) {
    buf[*pos..*pos+2].copy_from_slice(&v.to_le_bytes());
    *pos += 2;
}

fn put_u32(buf: &mut [u8], pos: &mut usize, v: u32) {
    buf[*pos..*pos+4].copy_from_slice(&v.to_le_bytes());
    *pos += 4;
}

fn put_len_prefixed(buf: &mut [u8], pos: &mut usize, data: &[u8]) {
    put_u16(buf, pos, data.len() as u16);
    buf[*pos..*pos+data.len()].copy_from_slice(data);
    *pos += data.len();
}

pub fn unix_time() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn unix_time_us() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn clock_ticks_per_sec() -> i64 {
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) }
}

/// RSS in bytes from /proc/[pid]/statm (field 2, in pages).
fn read_rss_bytes(pid: u32) -> Option<u64> {
    let content = std::fs::read_to_string(format!("/proc/{}/statm", pid)).ok()?;
    let rss_pages: u64 = content.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Some(rss_pages * page_size)
}

/// Total CPU ticks (utime+stime, fields 14+15) from /proc/[pid]/stat.
/// The comm field can contain spaces/parens, so we split on the last ')'.
fn read_cpu_ticks(pid: u32) -> Option<u64> {
    let content = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let after_comm = content.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // fields[0] is state (field 3); utime=field 14, stime=field 15 => indices 11, 12 here.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

fn resolve_in_path(name: &str) -> Option<String> {
    if name.contains('/') { return Some(name.to_string()); }
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() { continue; }
        let candidate = format!("{}/{}", dir, name);
        if std::fs::metadata(&candidate).is_ok() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Ewma ---

    #[test]
    fn ewma_starts_at_zero() {
        let mut e = Ewma::new(0);
        assert_eq!(e.read(0, 60.0), 0.0);
    }

    #[test]
    fn ewma_rises_toward_one_while_attached() {
        let mut e = Ewma::new(0);
        e.attach(0, 60.0);
        // At exactly one half-life of continuous busy time, the decayed
        // value should have closed half the gap to the busy asymptote (1.0).
        let v = e.read(60, 60.0);
        assert!((v - 0.5).abs() < 1e-9, "expected ~0.5, got {v}");
    }

    #[test]
    fn ewma_decays_toward_zero_after_detach() {
        let mut e = Ewma::new(0);
        e.attach(0, 60.0);
        e.detach(60, 60.0); // value is ~0.5 at detach
        let v = e.read(120, 60.0); // another half-life idle
        assert!(v < 0.5, "expected further decay after detach, got {v}");
        assert!(v > 0.0);
    }

    #[test]
    fn ewma_peek_does_not_mutate_state() {
        let mut e = Ewma::new(0);
        e.attach(0, 60.0);
        let peeked = e.peek(60, 60.0);
        // read() at the same `now` must match peek()'s result exactly, i.e.
        // peek() didn't advance last_t/consume the pending decay.
        let read = e.read(60, 60.0);
        assert!((peeked - read).abs() < 1e-9);
    }

    #[test]
    fn ewma_zero_or_negative_dt_is_a_no_op() {
        let mut e = Ewma::new(100);
        e.attach(100, 60.0);
        let before = e.read(100, 60.0);
        // update_to() bails out early when dt <= 0, so going "backwards" or
        // staying put must not change the stored value.
        let after = e.read(100, 60.0);
        assert_eq!(before, after);
    }

    // --- Crf ---

    #[test]
    fn crf_first_bump_sets_value_to_one_no_interval_budget_yet() {
        let mut c = Crf::default();
        c.bump(0, 60.0);
        assert_eq!(c.read(0, 60.0), 1.0);
        assert_eq!(c.interval_budget(), 0.0); // needs >= 2 summons
    }

    #[test]
    fn crf_read_decays_between_summons() {
        let mut c = Crf::default();
        c.bump(0, 60.0);
        let v = c.read(60, 60.0); // one half-life later
        assert!((v - 0.5).abs() < 1e-9, "expected ~0.5, got {v}");
    }

    #[test]
    fn crf_srtt_rttvar_after_second_bump() {
        let mut c = Crf::default();
        c.bump(0, 60.0);
        c.bump(100, 60.0); // gap = 100
        // After exactly two summons, srtt/rttvar are seeded directly from
        // the observed gap (srtt = gap, rttvar = gap / 2), per the
        // Jacobson/RFC6298-style seeding in Crf::bump.
        assert_eq!(c.interval_budget(), 100.0 + 4.0 * 50.0);
    }

    #[test]
    fn crf_srtt_rttvar_smooth_after_third_bump() {
        let mut c = Crf::default();
        c.bump(0, 60.0);
        c.bump(100, 60.0);  // gap = 100 -> srtt=100, rttvar=50
        c.bump(250, 60.0);  // gap = 150 -> err=50, srtt=106.25, rttvar=50.0
        let budget = c.interval_budget();
        assert!((budget - (106.25 + 4.0 * 50.0)).abs() < 1e-9, "got {budget}");
    }

    #[test]
    fn crf_regular_cadence_yields_larger_budget_than_erratic_cadence() {
        // A key summoned at a steady cadence should end up with a tighter
        // (smaller) rttvar, and thus a smaller interval_budget, than one
        // summoned at the same average rate but erratically.
        let mut regular = Crf::default();
        for t in [0, 100, 200, 300, 400] {
            regular.bump(t, 60.0);
        }
        let mut erratic = Crf::default();
        for t in [0, 20, 380, 400, 780] {
            erratic.bump(t, 60.0);
        }
        assert!(regular.interval_budget() < erratic.interval_budget());
    }
}
