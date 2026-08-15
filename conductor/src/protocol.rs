// Wire protocol definitions for client ↔ conductor ↔ worker communication.
use std::io::{self};

// --- Magic numbers ---

pub mod client {
    pub const MAGIC: u32 = 0x4A444301; // "JDC\x01"
    pub const ENV_REQUEST: u8 = 0x3F;  // '?' — conductor requests full env

    pub struct Flags(pub u8);
    impl Flags {
        pub fn is_tty(&self) -> bool { self.0 & 1 != 0 }
        pub fn new(tty: bool) -> u8 { if tty { 1 } else { 0 } }
    }
}

pub mod worker {
    pub const MAGIC: u32 = 0x4A445701; // "JDW\x01"

    #[repr(u8)]
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum MessageType {
        Ping        = 0x01,
        Pong        = 0x02,
        SetProject  = 0x10,
        ProjectOk   = 0x11,
        ClientRun   = 0x20,
        Sockets     = 0x21,
        QueryState  = 0x30,
        State       = 0x31,
        QueryClients = 0x32,
        Clients     = 0x33,
        SoftExit    = 0x40,
        Ack         = 0x41,
        SyncClients = 0x50,
        DropSession = 0x51,
        Err         = 0xFF,
    }

    impl MessageType {
        pub fn from_u8(b: u8) -> Self {
            match b {
                0x01 => Self::Ping,
                0x02 => Self::Pong,
                0x10 => Self::SetProject,
                0x11 => Self::ProjectOk,
                0x20 => Self::ClientRun,
                0x21 => Self::Sockets,
                0x30 => Self::QueryState,
                0x31 => Self::State,
                0x32 => Self::QueryClients,
                0x33 => Self::Clients,
                0x40 => Self::SoftExit,
                0x41 => Self::Ack,
                0x50 => Self::SyncClients,
                0x51 => Self::DropSession,
                _    => Self::Err,
            }
        }
    }

    pub struct Flags(pub u8);
    impl Flags {
        pub fn new(tty: bool, force: bool) -> u8 {
            (if tty { 1 } else { 0 }) | (if force { 2 } else { 0 })
        }
    }
}

pub mod notification {
    pub const MAGIC: u32 = 0x4A444E01; // "JDN\x01"

    #[repr(u8)]
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Type {
        ClientDone         = 0x01,
        WorkerUnresponsive = 0x02,
        WorkerExit         = 0x03,
        ClientExit         = 0x04,
        ClientInterrupt    = 0x05,
    }

    impl Type {
        pub fn from_u8(b: u8) -> Option<Self> {
            match b {
                0x01 => Some(Self::ClientDone),
                0x02 => Some(Self::WorkerUnresponsive),
                0x03 => Some(Self::WorkerExit),
                0x04 => Some(Self::ClientExit),
                0x05 => Some(Self::ClientInterrupt),
                _    => None,
            }
        }
    }
}

pub mod signals {
    pub const EXIT:       u8 = 0x01;
    pub const RAW_MODE:   u8 = 0x02;
    pub const QUERY_SIZE: u8 = 0x03;
    pub const NODELAY:    u8 = 0x04;
    pub const EXECUTING:  u8 = 0x05;
}

// --- Transport mode ---

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TransportMode { Unix, Tcp }

#[derive(Clone, Debug)]
pub struct ParsedAddress {
    pub mode: TransportMode,
    pub addr: String,
}

pub fn parse_address(raw: &str) -> Result<ParsedAddress, String> {
    if let Some(rest) = raw.strip_prefix("tcp://") {
        return Ok(ParsedAddress { mode: TransportMode::Tcp, addr: rest.to_string() });
    }
    if raw.contains("://") {
        return Err(format!("Unsupported scheme in '{}'", raw));
    }
    // Bare host:port (no slash) → TCP
    if !raw.starts_with('/') && !raw.starts_with('.') && !raw.contains('/') {
        return Ok(ParsedAddress { mode: TransportMode::Tcp, addr: raw.to_string() });
    }
    Ok(ParsedAddress { mode: TransportMode::Unix, addr: raw.to_string() })
}

pub const DEFAULT_TCP_PORT: u16 = 9345;

pub fn parse_host_port(addr: &str) -> Result<std::net::SocketAddr, String> {
    let (host, port) = if let Some(pos) = addr.rfind(':') {
        let port: u16 = addr[pos+1..].parse().map_err(|_| format!("Invalid port in '{}'", addr))?;
        (&addr[..pos], port)
    } else {
        (addr, DEFAULT_TCP_PORT)
    };
    let addr_str = format!("{}:{}", host, port);
    addr_str.parse().map_err(|_| format!("Invalid address '{}'", addr_str))
}

// --- Port pool for TCP mode ---

pub struct PortPool {
    base: u16,
    count: u16,
    free: Vec<bool>,
}

pub const PORT_POOL_NONE: u16 = 0xFFFF;

impl PortPool {
    pub fn new(base: u16, count: u16) -> Self {
        Self { base, count, free: vec![true; count as usize] }
    }

    pub fn allocate(&mut self) -> Option<u16> {
        let idx = self.free.iter().position(|&f| f)?;
        self.free[idx] = false;
        Some(idx as u16)
    }

    pub fn release(&mut self, idx: u16) {
        if (idx as usize) < self.free.len() {
            self.free[idx as usize] = true;
        }
    }

    pub fn ports_for_index(&self, idx: u16) -> [u16; 4] {
        let start = self.base + idx * 4;
        [start, start + 1, start + 2, start + 3]
    }
}

pub fn parse_port_range(s: &str) -> Option<(u16, u16)> {
    let dash = s.find('-')?;
    let low: u16 = s[..dash].parse().ok()?;
    let high: u16 = s[dash+1..].parse().ok()?;
    if high <= low { return None; }
    let count = (high - low + 1) / 4;
    if count == 0 { return None; }
    Some((low, count))
}

// --- Blocking I/O helpers (used for client socket and conductor-side reads) ---

pub fn read_exact_fd(fd: std::os::unix::io::RawFd, buf: &mut [u8]) -> io::Result<()> {
    let mut total = 0;
    while total < buf.len() {
        let n = unsafe {
            libc::read(fd, buf[total..].as_mut_ptr() as *mut libc::c_void, buf.len() - total)
        };
        if n <= 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "end of stream"));
        }
        total += n as usize;
    }
    Ok(())
}

pub fn write_all_fd(fd: std::os::unix::io::RawFd, buf: &[u8]) {
    let mut total = 0;
    while total < buf.len() {
        let n = unsafe {
            libc::write(fd, buf[total..].as_ptr() as *const libc::c_void, buf.len() - total)
        };
        if n <= 0 { break; }
        total += n as usize;
    }
}

// --- Random socket path generation ---

pub fn random_socket_path(runtime_dir: &str, suffix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos();
    let pid = std::process::id();
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}/{:08x}{:08x}{:04x}-{}", runtime_dir, t, pid, c, suffix)
}

// --- Platform default runtime dir ---

pub fn default_runtime_dir(xdg_runtime_dir: Option<&str>, home: Option<&str>) -> String {
    #[cfg(target_os = "linux")]
    {
        if let Some(xdg) = xdg_runtime_dir {
            return format!("{}/julia-daemon", xdg);
        }
        let uid = unsafe { libc::getuid() };
        return format!("/run/user/{}/julia-daemon", uid);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = home {
            return format!("{}/Library/Application Support/julia-daemon", h);
        }
        return "/tmp/julia-daemon".to_string();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let uid = unsafe { libc::getuid() };
        return format!("/tmp/julia-daemon-{}", uid);
    }
}

// --- Loopback detection for TCP connections ---

pub fn is_loopback(addr: &std::net::SocketAddr) -> bool {
    match addr {
        std::net::SocketAddr::V4(a) => a.ip().is_loopback(),
        std::net::SocketAddr::V6(a) => {
            let ip = a.ip();
            if ip.is_loopback() { return true; }
            // Check IPv4-mapped ::ffff:127.x.x.x
            if let Some(v4) = ip.to_ipv4_mapped() {
                return v4.is_loopback();
            }
            false
        }
    }
}

// --- TCP nodelay ---

pub fn set_tcp_nodelay_raw(fd: std::os::unix::io::RawFd) {
    let val: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_message_type_round_trips_all_variants() {
        use worker::MessageType::*;
        for (byte, variant) in [
            (0x01u8, Ping), (0x02, Pong),
            (0x10, SetProject), (0x11, ProjectOk),
            (0x20, ClientRun), (0x21, Sockets),
            (0x30, QueryState), (0x31, State),
            (0x32, QueryClients), (0x33, Clients),
            (0x40, SoftExit), (0x41, Ack),
            (0x50, SyncClients), (0x51, DropSession),
        ] {
            assert_eq!(worker::MessageType::from_u8(byte), variant);
        }
    }

    #[test]
    fn worker_message_type_unknown_byte_is_err() {
        assert_eq!(worker::MessageType::from_u8(0x99), worker::MessageType::Err);
    }

    #[test]
    fn notification_type_round_trips_all_variants() {
        use notification::Type::*;
        for (byte, variant) in [
            (0x01u8, ClientDone),
            (0x02, WorkerUnresponsive),
            (0x03, WorkerExit),
            (0x04, ClientExit),
            (0x05, ClientInterrupt),
        ] {
            assert_eq!(notification::Type::from_u8(byte), Some(variant));
        }
    }

    #[test]
    fn notification_type_unknown_byte_is_none() {
        assert_eq!(notification::Type::from_u8(0x99), None);
    }

    #[test]
    fn client_flags_tty_bit() {
        assert!(client::Flags(1).is_tty());
        assert!(!client::Flags(0).is_tty());
        assert_eq!(client::Flags::new(true), 1);
        assert_eq!(client::Flags::new(false), 0);
    }

    #[test]
    fn worker_flags_pack_tty_and_force_independently() {
        assert_eq!(worker::Flags::new(false, false), 0);
        assert_eq!(worker::Flags::new(true, false), 1);
        assert_eq!(worker::Flags::new(false, true), 2);
        assert_eq!(worker::Flags::new(true, true), 3);
    }

    #[test]
    fn parse_address_tcp_scheme() {
        let a = parse_address("tcp://127.0.0.1:9345").unwrap();
        assert_eq!(a.mode, TransportMode::Tcp);
        assert_eq!(a.addr, "127.0.0.1:9345");
    }

    #[test]
    fn parse_address_bare_host_port_is_tcp() {
        let a = parse_address("localhost:9345").unwrap();
        assert_eq!(a.mode, TransportMode::Tcp);
    }

    #[test]
    fn parse_address_path_is_unix() {
        let a = parse_address("/run/user/1000/julia-daemon/conductor.sock").unwrap();
        assert_eq!(a.mode, TransportMode::Unix);
    }

    #[test]
    fn parse_address_relative_path_is_unix() {
        let a = parse_address("./conductor.sock").unwrap();
        assert_eq!(a.mode, TransportMode::Unix);
    }

    #[test]
    fn parse_address_rejects_unknown_scheme() {
        assert!(parse_address("http://example.com").is_err());
    }

    #[test]
    fn parse_host_port_explicit_port() {
        let addr = parse_host_port("127.0.0.1:1234").unwrap();
        assert_eq!(addr.port(), 1234);
    }

    #[test]
    fn parse_host_port_defaults_when_missing() {
        let addr = parse_host_port("127.0.0.1").unwrap();
        assert_eq!(addr.port(), DEFAULT_TCP_PORT);
    }

    #[test]
    fn parse_host_port_rejects_bad_port() {
        assert!(parse_host_port("127.0.0.1:not-a-port").is_err());
    }

    #[test]
    fn parse_host_port_rejects_unresolvable_host() {
        // parse_host_port only accepts numeric IP literals — DNS names fail
        // at the final SocketAddr parse, not the port-splitting step.
        assert!(parse_host_port("localhost:1234").is_err());
    }

    #[test]
    fn port_pool_allocate_and_release() {
        let mut pool = PortPool::new(10000, 2);
        let a = pool.allocate().unwrap();
        let b = pool.allocate().unwrap();
        assert_ne!(a, b);
        assert!(pool.allocate().is_none()); // exhausted
        pool.release(a);
        assert_eq!(pool.allocate(), Some(a)); // reused after release
    }

    #[test]
    fn port_pool_ports_for_index_are_four_apart() {
        let pool = PortPool::new(10000, 4);
        assert_eq!(pool.ports_for_index(0), [10000, 10001, 10002, 10003]);
        assert_eq!(pool.ports_for_index(1), [10004, 10005, 10006, 10007]);
    }

    #[test]
    fn parse_port_range_valid() {
        assert_eq!(parse_port_range("10000-10015"), Some((10000, 4)));
    }

    #[test]
    fn parse_port_range_invalid_or_too_narrow() {
        assert_eq!(parse_port_range("10000-10001"), None); // < 4 ports
        assert_eq!(parse_port_range("10015-10000"), None); // high <= low
        assert_eq!(parse_port_range("not-a-range"), None);
    }

    #[test]
    fn is_loopback_v4() {
        let addr: std::net::SocketAddr = "127.0.0.1:9345".parse().unwrap();
        assert!(is_loopback(&addr));
        let addr: std::net::SocketAddr = "10.0.0.1:9345".parse().unwrap();
        assert!(!is_loopback(&addr));
    }

    #[test]
    fn is_loopback_v6_mapped_v4() {
        let addr: std::net::SocketAddr = "[::ffff:127.0.0.1]:9345".parse().unwrap();
        assert!(is_loopback(&addr));
    }

    #[test]
    fn random_socket_path_is_unique_and_uses_suffix() {
        let a = random_socket_path("/run/user/1000/julia-daemon", "client");
        let b = random_socket_path("/run/user/1000/julia-daemon", "client");
        assert_ne!(a, b);
        assert!(a.starts_with("/run/user/1000/julia-daemon/"));
        assert!(a.ends_with("-client"));
    }
}
