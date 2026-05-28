use std::sync::{Arc, Mutex};
use std::time::Duration;

mod args;
mod config;
mod conductor;
mod env_cache;
mod project;
mod protocol;
mod worker;
#[cfg(target_os = "linux")]
mod sandbox;

use conductor::{Conductor, Server};
use config::Config;

fn main() {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    eprintln!("Starting Julia Daemon Conductor. Configuration:");
    eprintln!(" - Worker executable: {}", config.worker_executable);
    eprintln!(" - Worker args: {}", config.worker_args);
    eprintln!(" - Max clients per worker: {}", config.worker_maxclients);
    eprintln!(" - Worker TTL: {} seconds", config.worker_ttl);
    eprintln!(" - Transport: {}", if config.transport == protocol::TransportMode::Unix { "unix" } else { "tcp" });
    eprintln!(" - Address: {}", config.socket_path);
    if let Some((base, count)) = config.port_range {
        eprintln!(" - Port range: {}-{} ({} port sets)", base, base + count * 4 - 1, count);
    }
    if let Some(m) = &config.sandbox_max_memory { eprintln!(" - Sandbox memory limit: {}", m); }
    if let Some(c) = config.sandbox_max_cpu { eprintln!(" - Sandbox CPU limit: {}%", c); }
    if !config.sandbox_remote_clients { eprintln!(" - Sandbox remote clients: disabled"); }
    if config.sandbox_session_bypass { eprintln!(" - Sandbox session bypass: enabled"); }

    // Create runtime dir for unix transport
    if config.transport == protocol::TransportMode::Unix {
        if let Err(e) = std::fs::create_dir_all(&config.runtime_dir) {
            eprintln!("Failed to create runtime dir '{}': {}", config.runtime_dir, e);
            std::process::exit(1);
        }
    }

    let mut conductor = Conductor::new(config);

    // Create server
    let server = match conductor.create_server() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to create server: {}", e);
            std::process::exit(1);
        }
    };

    conductor.write_pid_file();
    eprintln!("Conductor listening on {}", conductor.socket_path);

    // Clean up leftover files from previous run
    if conductor.config.transport == protocol::TransportMode::Unix {
        conductor.cleanup_runtime_dir();
    }

    // Create reserve worker
    if let Err(e) = conductor.create_reserve_worker(None) {
        eprintln!("Failed to create reserve worker: {}", e);
    }

    // Set up signal handling via a pipe
    let (sig_r, sig_w) = {
        let mut fds = [0i32; 2];
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) };
        (fds[0], fds[1])
    };

    install_signal_handlers(sig_w);

    // Wrap conductor in Arc<Mutex> for potential future multi-threaded use
    let conductor = Arc::new(Mutex::new(conductor));

    // Spawn health check timer thread
    {
        let conductor_clone = Arc::clone(&conductor);
        let ping_interval = conductor.lock().unwrap().config.ping_interval;
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(ping_interval));
                if let Ok(mut c) = conductor_clone.lock() {
                    c.check_workers();
                }
            }
        });
    }

    // Main accept loop
    eprintln!("Conductor running");
    run_accept_loop(conductor, server, sig_r);
}

fn run_accept_loop(conductor: Arc<Mutex<Conductor>>, server: Server, sig_r: i32) {
    // Use select() to wait on both the server fd and signal pipe
    let server_fd = match &server {
        Server::Unix(l) => {
            use std::os::unix::io::AsRawFd;
            l.as_raw_fd()
        }
        Server::Tcp(l) => {
            use std::os::unix::io::AsRawFd;
            l.as_raw_fd()
        }
    };

    loop {
        let ready = wait_for_fd(server_fd, sig_r);
        if ready < 0 {
            // EINTR — check signals
        }

        // Check signal pipe first
        let mut sig_byte = [0u8; 16];
        loop {
            let n = unsafe { libc::read(sig_r, sig_byte.as_mut_ptr() as *mut _, sig_byte.len()) };
            if n <= 0 { break; }
            for &b in &sig_byte[..n as usize] {
                match b {
                    b'T' | b'I' => {
                        eprintln!("\nShutdown requested, stopping workers...");
                        if let Ok(mut c) = conductor.lock() {
                            c.graceful_shutdown();
                            c.cleanup_pid_file();
                            c.cleanup_socket();
                        }
                        std::process::exit(0);
                    }
                    b'U' => {
                        eprintln!("Recreating socket due to SIGUSR1");
                        if let Ok(c) = conductor.lock() {
                            c.cleanup_socket();
                            drop(c);
                        }
                        // Recreate server — for simplicity, just restart
                        if let Ok(c) = conductor.lock() {
                            match c.create_server() {
                                Ok(_new_server) => {
                                    eprintln!("Socket recreated");
                                    // Note: we'd need to update the accept loop's server here
                                    // For now, just log
                                }
                                Err(e) => eprintln!("Failed to recreate socket: {}", e),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if ready <= 0 { continue; }

        // Accept connection
        match server.accept() {
            Ok(conn) => {
                // Handle in the same thread (matches Zig's single-threaded design)
                if let Ok(mut c) = conductor.lock() {
                    c.handle_connection(conn);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("Accept error: {}", e);
                continue;
            }
        }
    }
}

fn wait_for_fd(server_fd: i32, sig_r: i32) -> i32 {
    unsafe {
        let max_fd = server_fd.max(sig_r) + 1;
        let mut read_fds: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(server_fd, &mut read_fds);
        libc::FD_SET(sig_r, &mut read_fds);
        libc::select(max_fd, &mut read_fds, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    }
}

static SIGNAL_PIPE_W: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn install_signal_handlers(sig_w: i32) {
    SIGNAL_PIPE_W.store(sig_w, std::sync::atomic::Ordering::SeqCst);

    extern "C" fn handle_shutdown(sig: libc::c_int) {
        let w = SIGNAL_PIPE_W.load(std::sync::atomic::Ordering::SeqCst);
        if w >= 0 {
            let b: u8 = if sig == libc::SIGTERM { b'T' } else { b'I' };
            unsafe { libc::write(w, &b as *const _ as *const _, 1); }
        }
    }
    extern "C" fn handle_usr1(_: libc::c_int) {
        let w = SIGNAL_PIPE_W.load(std::sync::atomic::Ordering::SeqCst);
        if w >= 0 {
            unsafe { libc::write(w, b"U".as_ptr() as *const _, 1); }
        }
    }

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_shutdown as libc::sighandler_t;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());

        sa.sa_sigaction = handle_usr1 as libc::sighandler_t;
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());

        // Ignore SIGPIPE
        sa.sa_sigaction = libc::SIG_IGN;
        libc::sigaction(libc::SIGPIPE, &sa, std::ptr::null_mut());

        // SIGCHLD: auto-reap children
        sa.sa_sigaction = libc::SIG_DFL;
        sa.sa_flags = libc::SA_NOCLDWAIT;
        libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut());
    }
}
