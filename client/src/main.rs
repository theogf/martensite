// juliaclient — connects to julia-conductor and proxies stdio to a Julia worker.

mod cooked;

use std::os::unix::io::RawFd;
use std::time::Duration;

// --- Protocol constants (duplicated from conductor to avoid workspace coupling) ---
mod protocol {
    pub const CLIENT_MAGIC: u32 = 0x4A444301;
    pub const ENV_REQUEST: u8  = 0x3F;
    pub const NOTIFICATION_MAGIC: u32 = 0x4A444E01;
    pub const NOTIFICATION_CLIENT_EXIT: u8 = 0x04;
    pub const DEFAULT_TCP_PORT: u16 = 9345;
    pub const SIG_EXIT: u8     = 0x01;
    pub const SIG_RAW_MODE: u8 = 0x02;
    pub const SIG_QUERY_SIZE: u8 = 0x03;
    pub const SIG_NODELAY: u8  = 0x04;
}

const MAX_SOCKET_PATH: usize = 256;

// --- Env scanning ---

struct EnvInfo {
    fingerprint: u64,
    count: u16,
    server_path: Option<String>,
    runtime_dir: Option<String>,
    xdg_runtime_dir: Option<String>,
    home: Option<String>,
}

fn scan_env() -> EnvInfo {
    let mut info = EnvInfo {
        fingerprint: 0,
        count: 0,
        server_path: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
        home: None,
    };

    for (key, value) in std::env::vars() {
        let kv = format!("{}={}", key, value);
        // Skip benchmarking noise
        if key.starts_with("HYPERFINE_") { continue; }
        info.count += 1;

        // XOR hash for order-independent fingerprint
        let h = wyhash(&kv);
        info.fingerprint ^= h;

        match key.as_str() {
            "JULIA_DAEMON_SERVER"  => info.server_path = Some(value),
            "JULIA_DAEMON_RUNTIME" => info.runtime_dir = Some(value),
            "XDG_RUNTIME_DIR"      => info.xdg_runtime_dir = Some(value),
            "HOME"                 => info.home = Some(value),
            _ => {}
        }
    }
    info
}

fn wyhash(s: &str) -> u64 {
    // Simple xor hash matching the Zig wyhash behavior (length-seeded)
    let mut h = s.len() as u64;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        h ^= (b as u64).wrapping_mul(0x9e3779b97f4a7c15_u64.wrapping_add(i as u64));
        h = h.rotate_left(31);
    }
    h
}

// --- Terminal helpers ---

static mut SAVED_TERMIOS: Option<libc::termios> = None;

fn set_raw_mode(raw: bool) {
    unsafe {
        if raw {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
                if SAVED_TERMIOS.is_none() {
                    SAVED_TERMIOS = Some(t);
                }
                t.c_lflag &= !(libc::ECHO | libc::ICANON);
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &t);
            }
        } else if let Some(t) = SAVED_TERMIOS {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &t);
            SAVED_TERMIOS = None;
        }
    }
}

fn is_tty(fd: i32) -> bool {
    terminal_size(fd).is_some()
}

fn terminal_size(fd: i32) -> Option<(u16, u16)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 {
            Some((ws.ws_row, ws.ws_col))
        } else {
            None
        }
    }
}

// --- Socket path / address utilities ---

fn parse_address(raw: &str) -> (bool, String) {
    // Returns (is_tcp, addr)
    if let Some(rest) = raw.strip_prefix("tcp://") {
        return (true, rest.to_string());
    }
    if raw.contains("://") {
        eprintln!("Unsupported address scheme: {}", raw);
        eprintln!("Only tcp:// and unix paths are supported.");
        std::process::exit(1);
    }
    if !raw.starts_with('/') && !raw.starts_with('.') && !raw.contains('/') {
        return (true, raw.to_string());
    }
    (false, raw.to_string())
}

fn default_runtime_dir(xdg: Option<&str>, home: Option<&str>) -> String {
    #[cfg(target_os = "linux")]
    {
        if let Some(x) = xdg { return format!("{}/julia-daemon", x); }
        let uid = unsafe { libc::getuid() };
        return format!("/run/user/{}/julia-daemon", uid);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = home { return format!("{}/Library/Application Support/julia-daemon", h); }
        return "/tmp/julia-daemon".to_string();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let uid = unsafe { libc::getuid() };
        return format!("/tmp/julia-daemon-{}", uid);
    }
}

// --- Low-level I/O ---

fn write_fd(fd: i32, data: &[u8]) {
    let mut written = 0;
    while written < data.len() {
        let n = unsafe {
            libc::write(fd, data[written..].as_ptr() as *const _, data.len() - written)
        };
        if n <= 0 { break; }
        written += n as usize;
    }
}

fn read_exact_fd(fd: i32, buf: &mut [u8]) -> bool {
    let mut read = 0;
    while read < buf.len() {
        let n = unsafe { libc::read(fd, buf[read..].as_mut_ptr() as *mut _, buf.len() - read) };
        if n <= 0 { return false; }
        read += n as usize;
    }
    true
}

// --- Connect to conductor ---

struct ConductorConn {
    fd: RawFd,
    is_tcp: bool,
    addr: String,
}

impl Drop for ConductorConn {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

fn connect_unix(path: &str) -> Option<RawFd> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 { return None; }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.as_bytes();
        if bytes.len() >= addr.sun_path.len() { libc::close(fd); return None; }
        for (i, &b) in bytes.iter().enumerate() {
            addr.sun_path[i] = b as libc::c_char;
        }
        let addr_len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
        let rc = libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr_len as libc::socklen_t);
        if rc == 0 { Some(fd) } else { libc::close(fd); None }
    }
}

fn connect_tcp(addr: &str) -> Option<RawFd> {
    let (host, port) = if let Some(pos) = addr.rfind(':') {
        let port: u16 = addr[pos+1..].parse().unwrap_or(protocol::DEFAULT_TCP_PORT);
        (&addr[..pos], port)
    } else {
        (addr, protocol::DEFAULT_TCP_PORT)
    };
    let addr_str = format!("{}:{}", host, port);
    let sock_addr: std::net::SocketAddr = addr_str.parse().ok()?;
    unsafe {
        let (af, sa_len) = match &sock_addr {
            std::net::SocketAddr::V4(_) => (libc::AF_INET, std::mem::size_of::<libc::sockaddr_in>()),
            std::net::SocketAddr::V6(_) => (libc::AF_INET6, std::mem::size_of::<libc::sockaddr_in6>()),
        };
        let fd = libc::socket(af, libc::SOCK_STREAM, 0);
        if fd < 0 { return None; }
        let rc = match &sock_addr {
            std::net::SocketAddr::V4(a) => {
                let mut sa: libc::sockaddr_in = std::mem::zeroed();
                sa.sin_family = libc::AF_INET as libc::sa_family_t;
                sa.sin_port = a.port().to_be();
                sa.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
                libc::connect(fd, &sa as *const _ as *const _, sa_len as libc::socklen_t)
            }
            std::net::SocketAddr::V6(a) => {
                let mut sa: libc::sockaddr_in6 = std::mem::zeroed();
                sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sa.sin6_port = a.port().to_be();
                sa.sin6_addr.s6_addr = a.ip().octets();
                libc::connect(fd, &sa as *const _ as *const _, sa_len as libc::socklen_t)
            }
        };
        if rc == 0 { Some(fd) } else { libc::close(fd); None }
    }
}

fn set_tcp_nodelay(fd: i32) {
    let val: libc::c_int = 1;
    unsafe {
        libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY,
            &val as *const _ as *const _, std::mem::size_of::<libc::c_int>() as libc::socklen_t);
    }
}

fn read_pid_and_signal(pid_path: &str) -> bool {
    let content = match std::fs::read_to_string(pid_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let pid: libc::pid_t = match content.trim().parse() {
        Ok(p) => p,
        Err(_) => return false,
    };
    unsafe { libc::kill(pid, libc::SIGUSR1) };
    true
}

fn connect_to_conductor(env: &EnvInfo, addr_override: Option<&str>) -> ConductorConn {
    let runtime_dir = env.runtime_dir.clone().unwrap_or_else(|| {
        default_runtime_dir(env.xdg_runtime_dir.as_deref(), env.home.as_deref())
    });

    let raw_path = addr_override
        .map(|s| s.to_string())
        .or_else(|| env.server_path.clone())
        .unwrap_or_else(|| format!("{}/conductor.sock", runtime_dir));

    let (is_tcp, addr) = parse_address(&raw_path);

    // First attempt
    let try_connect = |is_tcp: bool, addr: &str| -> Option<RawFd> {
        if is_tcp { connect_tcp(addr) } else { connect_unix(addr) }
    };

    if let Some(fd) = try_connect(is_tcp, &addr) {
        return ConductorConn { fd, is_tcp, addr };
    }

    if is_tcp {
        // Retry briefly for TCP
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(fd) = connect_tcp(&addr) {
                return ConductorConn { fd, is_tcp, addr };
            }
        }
    } else {
        // Unix: try to signal conductor to recreate socket
        let pid_path = format!("{}/conductor.pid", runtime_dir);
        if read_pid_and_signal(&pid_path) {
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(100));
                if let Some(fd) = connect_unix(&addr) {
                    return ConductorConn { fd, is_tcp, addr };
                }
            }
            // Try default TCP port
            let tcp_addr = format!("localhost:{}", protocol::DEFAULT_TCP_PORT);
            if let Some(fd) = connect_tcp(&tcp_addr) {
                return ConductorConn { fd, is_tcp: true, addr: tcp_addr };
            }
        }
    }

    eprintln!("Failed to connect to {}", addr);
    eprintln!();
    eprintln!("Try restarting the daemon:");
    eprintln!();
    #[cfg(target_os = "linux")]
    eprintln!("  systemctl --user restart julia-daemon");
    #[cfg(target_os = "macos")]
    eprintln!("  launchctl kickstart -k gui/$(id -u)/net.julialang.julia-daemon");
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    eprintln!("  pkill -f julia-conductor && julia-conductor &");
    eprintln!();
    eprintln!("Or specify a different address with -a <addr>");
    std::process::exit(127);
}

// --- Send client info ---

struct BufWriter {
    buf: Vec<u8>,
}

impl BufWriter {
    fn new() -> Self { Self { buf: Vec::with_capacity(4096) } }
    fn write_u8(&mut self, v: u8) { self.buf.push(v); }
    fn write_u16(&mut self, v: u16) { self.buf.extend_from_slice(&v.to_le_bytes()); }
    fn write_u32(&mut self, v: u32) { self.buf.extend_from_slice(&v.to_le_bytes()); }
    fn write_u64(&mut self, v: u64) { self.buf.extend_from_slice(&v.to_le_bytes()); }
    fn write_bytes(&mut self, b: &[u8]) { self.buf.extend_from_slice(b); }
    fn write_len_prefixed(&mut self, data: &[u8]) {
        self.write_u16(data.len() as u16);
        self.write_bytes(data);
    }
    fn flush_to(&self, fd: i32) {
        write_fd(fd, &self.buf);
    }
}

fn send_client_info(
    fd: i32,
    env: &EnvInfo,
    is_tty: bool,
    argv: &[String],
    skip_indices: &[usize],
) {
    let mut w = BufWriter::new();
    w.write_u32(protocol::CLIENT_MAGIC);
    w.write_u8(if is_tty { 1 } else { 0 }); // flags
    w.write_bytes(&[0, 0, 0]);               // reserved
    w.write_u32(unsafe { libc::getpid() as u32 });
    w.write_u32(unsafe { libc::getppid() as u32 });

    // CWD
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    w.write_len_prefixed(cwd.as_bytes());

    w.write_u64(env.fingerprint);

    // Args (skip address flag indices)
    let skip_count = skip_indices.len() as u16;
    w.write_u16(argv.len() as u16 - skip_count);
    for (i, arg) in argv.iter().enumerate() {
        if skip_indices.contains(&i) { continue; }
        w.write_len_prefixed(arg.as_bytes());
    }

    w.flush_to(fd);
}

fn send_full_env(fd: i32, env: &EnvInfo) {
    let mut w = BufWriter::new();
    w.write_u16(env.count);
    for (key, value) in std::env::vars() {
        if key.starts_with("HYPERFINE_") { continue; }
        w.write_len_prefixed(key.as_bytes());
        w.write_len_prefixed(value.as_bytes());
    }
    w.flush_to(fd);
}

// --- Receive worker socket paths ---

struct WorkerSockets {
    stdin_fd: RawFd,
    stdout_fd: RawFd,
    stderr_fd: RawFd,
    signals_fd: RawFd,
    is_tcp: bool,
    conductor_host: String,
}

fn read_u16(fd: i32) -> Option<u16> {
    let mut buf = [0u8; 2];
    if !read_exact_fd(fd, &mut buf) { None } else { Some(u16::from_le_bytes(buf)) }
}

fn connect_to_worker_socket(raw: &str, is_tcp: bool, conductor_host: &str) -> Option<RawFd> {
    let addr = if raw.starts_with(':') && is_tcp {
        format!("{}{}", conductor_host, raw)
    } else {
        raw.to_string()
    };

    let (socket_is_tcp, socket_addr) = parse_address(&addr);
    let fd = if socket_is_tcp {
        connect_tcp(&socket_addr)
    } else {
        let f = connect_unix(&socket_addr);
        if f.is_some() && !socket_is_tcp {
            // Clean up unix socket file after connecting (worker creates it)
            let _ = std::fs::remove_file(&socket_addr);
        }
        f
    }?;

    Some(fd)
}

fn receive_worker_sockets(
    conductor_fd: i32, env: &EnvInfo, is_tcp: bool, conductor_addr: &str,
) -> WorkerSockets {
    // First byte: either ENV_REQUEST or low byte of stdin path length
    let mut first_byte = [0u8; 1];
    if !read_exact_fd(conductor_fd, &mut first_byte) {
        eprintln!("Failed to read from conductor");
        std::process::exit(127);
    }

    if first_byte[0] == protocol::ENV_REQUEST {
        send_full_env(conductor_fd, env);
    }

    // Read 4 length-prefixed paths
    let read_path = |fd: i32, first: Option<u8>| -> String {
        let len = if let Some(b) = first {
            let mut second = [0u8; 1];
            if !read_exact_fd(fd, &mut second) { return String::new(); }
            u16::from_le_bytes([b, second[0]]) as usize
        } else {
            let mut lbuf = [0u8; 2];
            if !read_exact_fd(fd, &mut lbuf) { return String::new(); }
            u16::from_le_bytes(lbuf) as usize
        };
        let mut path = vec![0u8; len];
        if len > 0 && !read_exact_fd(fd, &mut path) { return String::new(); }
        String::from_utf8_lossy(&path).into_owned()
    };

    let first_for_path = if first_byte[0] == protocol::ENV_REQUEST { None } else { Some(first_byte[0]) };
    let stdin_path  = read_path(conductor_fd, first_for_path);
    let stdout_path = read_path(conductor_fd, None);
    let stderr_path = read_path(conductor_fd, None);
    let signals_path = read_path(conductor_fd, None);

    // Close conductor connection
    unsafe { libc::close(conductor_fd); }

    let conductor_host = if let Some(pos) = conductor_addr.rfind(':') {
        conductor_addr[..pos].to_string()
    } else {
        conductor_addr.to_string()
    };

    let connect = |path: &str, label: &str| -> RawFd {
        connect_to_worker_socket(path, is_tcp, &conductor_host).unwrap_or_else(|| {
            eprintln!("Failed to connect to {} socket at '{}'", label, path);
            std::process::exit(127);
        })
    };

    let stdin_fd  = connect(&stdin_path, "stdin");
    let stdout_fd = connect(&stdout_path, "stdout");
    let stderr_fd = connect(&stderr_path, "stderr");
    let signals_fd = connect(&signals_path, "signals");

    if is_tcp { set_tcp_nodelay(signals_fd); }

    WorkerSockets { stdin_fd, stdout_fd, stderr_fd, signals_fd, is_tcp, conductor_host }
}

// --- Signal parser ---

struct SignalParser {
    buf: Vec<u8>,
    sync_mode: bool,
    pub worker_wants_raw: bool,
}

impl SignalParser {
    fn new(sync_mode: bool) -> Self {
        Self { buf: Vec::with_capacity(256), sync_mode, worker_wants_raw: false }
    }

    fn feed(&mut self, data: &[u8], signals_fd: RawFd) -> Option<u8> {
        self.buf.extend_from_slice(data);
        self.process(signals_fd)
    }

    fn process(&mut self, signals_fd: RawFd) -> Option<u8> {
        let mut result = None;
        let mut pos = 0;
        while pos + 2 <= self.buf.len() {
            let id = self.buf[pos];
            let data_len = self.buf[pos + 1] as usize;
            if pos + 2 + data_len > self.buf.len() { break; }
            let data = self.buf[pos + 2..pos + 2 + data_len].to_vec();
            result = self.dispatch(id, &data, signals_fd);
            pos += 2 + data_len;
        }
        self.buf.drain(..pos);
        result
    }

    fn dispatch(&mut self, id: u8, data: &[u8], signals_fd: RawFd) -> Option<u8> {
        match id {
            protocol::SIG_EXIT => Some(if data.len() >= 1 { data[0] } else { 1 }),
            protocol::SIG_RAW_MODE => {
                if data.len() >= 1 {
                    let want_raw = data[0] != 0;
                    if self.sync_mode {
                        self.worker_wants_raw = want_raw;
                    } else {
                        set_raw_mode(want_raw);
                    }
                }
                // Ack: id + len=0
                write_fd(signals_fd, &[id, 0]);
                None
            }
            protocol::SIG_QUERY_SIZE => {
                let (rows, cols) = terminal_size(libc::STDIN_FILENO).unwrap_or((24, 80));
                let mut resp = [0u8; 6];
                resp[0] = id;
                resp[1] = 4;
                resp[2..4].copy_from_slice(&rows.to_le_bytes());
                resp[4..6].copy_from_slice(&cols.to_le_bytes());
                write_fd(signals_fd, &resp);
                None
            }
            protocol::SIG_NODELAY => {
                // Signal socket already has nodelay set; apply to stdin socket too
                // (we'd need the stdin fd here — skip for now)
                None
            }
            _ => None,
        }
    }
}

// --- Exit notification ---

fn notify_exit(is_tcp: bool, conductor_addr: &str) {
    let fd = if is_tcp {
        connect_tcp(conductor_addr)
    } else {
        connect_unix(conductor_addr)
    };
    if let Some(fd) = fd {
        let pid = unsafe { libc::getpid() as u32 };
        let mut buf = [0u8; 9];
        buf[..4].copy_from_slice(&protocol::NOTIFICATION_MAGIC.to_le_bytes());
        buf[4] = protocol::NOTIFICATION_CLIENT_EXIT;
        buf[5..9].copy_from_slice(&pid.to_le_bytes());
        write_fd(fd, &buf);
        unsafe { libc::close(fd); }
    }
}

// --- Signal handling globals ---

static CONDUCTOR_IS_TCP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CONDUCTOR_ADDR_BUF: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

// Worker stdin fd for forwarding SIGINT as Ctrl-C
static WORKER_STDIN_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn install_client_signal_handlers() {
    extern "C" fn handle_sigint(_: libc::c_int) {
        let fd = WORKER_STDIN_FD.load(std::sync::atomic::Ordering::SeqCst);
        if fd >= 0 {
            write_fd(fd, b"\x03");
        }
    }

    extern "C" fn handle_sigterm(_: libc::c_int) {
        let is_tcp = CONDUCTOR_IS_TCP.load(std::sync::atomic::Ordering::SeqCst);
        let addr = CONDUCTOR_ADDR_BUF.lock().unwrap().clone();
        notify_exit(is_tcp, &addr);
        set_raw_mode(false);
        unsafe { libc::exit(128 + libc::SIGTERM); }
    }

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_sigint as libc::sighandler_t;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());

        sa.sa_sigaction = handle_sigterm as libc::sighandler_t;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());

        // Ignore SIGPIPE
        sa.sa_sigaction = libc::SIG_IGN;
        libc::sigaction(libc::SIGPIPE, &sa, std::ptr::null_mut());
    }
}

// --- I/O event loop ---

fn run_event_loop(sockets: &WorkerSockets, sync_mode: bool) -> u8 {
    let mut signal_parser = SignalParser::new(sync_mode);
    let mut cooked_state = cooked::CookedState::new();

    let mut exit_code: Option<u8> = None;
    let mut stdout_eof = false;
    let mut stderr_eof = false;

    const BUF_SIZE: usize = 4096;
    let mut stdin_buf  = [0u8; BUF_SIZE];
    let mut stdout_buf = [0u8; BUF_SIZE];
    let mut stderr_buf = [0u8; BUF_SIZE];
    let mut sig_buf    = [0u8; BUF_SIZE];

    // Use epoll for efficient multiplexing on Linux
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::RawFd;

        let epoll_fd = unsafe { libc::epoll_create1(0) };
        if epoll_fd < 0 {
            eprintln!("epoll_create1 failed");
            std::process::exit(1);
        }

        let add_fd = |epoll: i32, fd: RawFd, id: u64| {
            let mut ev = libc::epoll_event { events: libc::EPOLLIN as u32, u64: id };
            unsafe { libc::epoll_ctl(epoll, libc::EPOLL_CTL_ADD, fd, &mut ev); }
        };

        const ID_STDIN:   u64 = 0;
        const ID_STDOUT:  u64 = 1;
        const ID_STDERR:  u64 = 2;
        const ID_SIGNALS: u64 = 3;

        add_fd(epoll_fd, libc::STDIN_FILENO, ID_STDIN);
        add_fd(epoll_fd, sockets.stdout_fd, ID_STDOUT);
        add_fd(epoll_fd, sockets.stderr_fd, ID_STDERR);
        add_fd(epoll_fd, sockets.signals_fd, ID_SIGNALS);

        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];

        'outer: loop {
            let n = unsafe {
                libc::epoll_wait(epoll_fd, events.as_mut_ptr(), events.len() as i32, -1)
            };
            if n < 0 {
                let e = unsafe { *libc::__errno_location() };
                if e == libc::EINTR { continue; }
                break;
            }
            for event in &events[..n as usize] {
                match event.u64 {
                    ID_STDIN => {
                        let n = unsafe { libc::read(libc::STDIN_FILENO, stdin_buf.as_mut_ptr() as *mut _, BUF_SIZE) };
                        if n <= 0 {
                            // stdin closed: close worker stdin
                            unsafe { libc::shutdown(sockets.stdin_fd, libc::SHUT_WR); }
                            unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_DEL, libc::STDIN_FILENO, std::ptr::null_mut()); }
                            continue;
                        }
                        if exit_code.is_some() { continue; }
                        let data = &stdin_buf[..n as usize];
                        if sync_mode && !signal_parser.worker_wants_raw {
                            // Cooked mode emulation
                            for &byte in data {
                                if let Some(to_send) = cooked_state.process(byte, libc::STDOUT_FILENO) {
                                    if to_send.is_empty() {
                                        unsafe { libc::shutdown(sockets.stdin_fd, libc::SHUT_WR); }
                                    } else {
                                        write_fd(sockets.stdin_fd, &to_send);
                                    }
                                }
                            }
                        } else {
                            write_fd(sockets.stdin_fd, data);
                        }
                    }
                    ID_STDOUT => {
                        let n = unsafe { libc::read(sockets.stdout_fd, stdout_buf.as_mut_ptr() as *mut _, BUF_SIZE) };
                        if n <= 0 { stdout_eof = true; continue; }
                        write_fd(libc::STDOUT_FILENO, &stdout_buf[..n as usize]);
                    }
                    ID_STDERR => {
                        let n = unsafe { libc::read(sockets.stderr_fd, stderr_buf.as_mut_ptr() as *mut _, BUF_SIZE) };
                        if n <= 0 { stderr_eof = true; continue; }
                        write_fd(libc::STDERR_FILENO, &stderr_buf[..n as usize]);
                    }
                    ID_SIGNALS => {
                        let n = unsafe { libc::read(sockets.signals_fd, sig_buf.as_mut_ptr() as *mut _, BUF_SIZE) };
                        if n <= 0 {
                            if exit_code.is_none() { exit_code = Some(1); }
                            // Remove from epoll so we don't busy-loop
                            unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_DEL, sockets.signals_fd, std::ptr::null_mut()); }
                            continue;
                        }
                        if let Some(code) = signal_parser.feed(&sig_buf[..n as usize], sockets.signals_fd) {
                            exit_code = Some(code);
                            // Remove signals from epoll
                            unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_DEL, sockets.signals_fd, std::ptr::null_mut()); }
                        }
                    }
                    _ => {}
                }
                if exit_code.is_some() && stdout_eof && stderr_eof {
                    break 'outer;
                }
            }
            if exit_code.is_some() && stdout_eof && stderr_eof {
                break;
            }
        }
        unsafe { libc::close(epoll_fd); }
    }

    // Fallback: poll-based for non-Linux
    #[cfg(not(target_os = "linux"))]
    {
        loop {
            let mut fds = [
                libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: sockets.stdout_fd,   events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: sockets.stderr_fd,   events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: sockets.signals_fd,  events: libc::POLLIN, revents: 0 },
            ];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
            if n < 0 { break; }

            if fds[0].revents & libc::POLLIN != 0 {
                let n = unsafe { libc::read(libc::STDIN_FILENO, stdin_buf.as_mut_ptr() as *mut _, BUF_SIZE) };
                if n > 0 && exit_code.is_none() {
                    write_fd(sockets.stdin_fd, &stdin_buf[..n as usize]);
                }
            }
            if fds[1].revents & libc::POLLIN != 0 {
                let n = unsafe { libc::read(sockets.stdout_fd, stdout_buf.as_mut_ptr() as *mut _, BUF_SIZE) };
                if n <= 0 { stdout_eof = true; }
                else { write_fd(libc::STDOUT_FILENO, &stdout_buf[..n as usize]); }
            }
            if fds[2].revents & libc::POLLIN != 0 {
                let n = unsafe { libc::read(sockets.stderr_fd, stderr_buf.as_mut_ptr() as *mut _, BUF_SIZE) };
                if n <= 0 { stderr_eof = true; }
                else { write_fd(libc::STDERR_FILENO, &stderr_buf[..n as usize]); }
            }
            if fds[3].revents & libc::POLLIN != 0 {
                let n = unsafe { libc::read(sockets.signals_fd, sig_buf.as_mut_ptr() as *mut _, BUF_SIZE) };
                if n <= 0 {
                    if exit_code.is_none() { exit_code = Some(1); }
                } else if let Some(code) = signal_parser.feed(&sig_buf[..n as usize], sockets.signals_fd) {
                    exit_code = Some(code);
                }
            }
            if exit_code.is_some() && stdout_eof && stderr_eof { break; }
        }
    }

    exit_code.unwrap_or(1)
}

// --- Entry point ---

fn extract_address_arg(argv: &[String]) -> (Option<String>, Vec<usize>) {
    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--" { break; }
        if arg.starts_with("--address=") {
            return (Some(arg["--address=".len()..].to_string()), vec![i]);
        }
        if arg.len() > 2 && arg.starts_with("-a") && &arg[..2] == "-a" {
            return (Some(arg[2..].to_string()), vec![i]);
        }
        if arg == "--address" || arg == "-a" {
            if let Some(next) = argv.get(i + 1) {
                return (Some(next.clone()), vec![i, i + 1]);
            }
        }
        i += 1;
    }
    (None, vec![])
}

fn extract_sync_arg(argv: &[String]) -> bool {
    argv.iter().skip(1).any(|a| a == "--sync")
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let env = scan_env();

    let (addr_override, skip_indices) = extract_address_arg(&argv);
    let sync_mode = extract_sync_arg(&argv);

    let is_tty = is_tty(libc::STDIN_FILENO);
    if is_tty { set_raw_mode(true); }

    // Connect to conductor
    let conductor = connect_to_conductor(&env, addr_override.as_deref());
    let conductor_fd = conductor.fd;
    let is_tcp = conductor.is_tcp;
    let conductor_addr = conductor.addr.clone();
    std::mem::forget(conductor); // Don't close fd in Drop

    if is_tcp { set_tcp_nodelay(conductor_fd); }

    // Store for signal handlers
    CONDUCTOR_IS_TCP.store(is_tcp, std::sync::atomic::Ordering::SeqCst);
    *CONDUCTOR_ADDR_BUF.lock().unwrap() = conductor_addr.clone();

    // Send client info
    send_client_info(conductor_fd, &env, is_tty, &argv, &skip_indices);

    // Receive worker socket paths (conductor may request env)
    let sockets = receive_worker_sockets(conductor_fd, &env, is_tcp, &conductor_addr);

    // Store worker stdin fd for SIGINT forwarding
    WORKER_STDIN_FD.store(sockets.stdin_fd, std::sync::atomic::Ordering::SeqCst);

    // Install signal handlers after connecting
    install_client_signal_handlers();

    // Run I/O event loop
    let exit_code = run_event_loop(&sockets, sync_mode);

    // Notify conductor we're done
    notify_exit(is_tcp, &conductor_addr);

    set_raw_mode(false);
    std::process::exit(exit_code as i32);
}
