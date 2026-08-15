// Linux unprivileged user namespace sandbox for Julia worker processes.
// Uses CLONE_NEWUSER + CLONE_NEWNS + CLONE_NEWPID (no root required).

#[cfg(not(target_os = "linux"))]
compile_error!("sandbox.rs is Linux-only");

use std::collections::HashMap;

const MS_RDONLY: u32  = 0x0001;
const MS_NOSUID: u32  = 0x0002;
const MS_NODEV: u32   = 0x0004;
const MS_NOEXEC: u32  = 0x0008;
const MS_REMOUNT: u32 = 0x0020;
const MS_SILENT: u32  = 0x8000;
const MS_BIND: u32    = 0x1000;
const MS_REC: u32     = 0x4000;
const MS_SLAVE: u32   = 0x80000;
const MNT_DETACH: u32 = 0x2;

const CLONE_NEWNS:   libc::c_int = 0x00020000;
const CLONE_NEWPID:  libc::c_int = 0x20000000;
const CLONE_NEWUSER: libc::c_int = 0x10000000;

#[derive(Debug)]
pub enum SandboxError {
    ForkFailed,
    UnshareFailed,
    UidMapFailed,
    GidMapFailed,
    SetgroupsFailed,
    MountFailed,
    MkdirFailed,
    PivotRootFailed,
    ChdirFailed,
    ExecFailed,
    PathTooLong,
}

pub struct SandboxConfig {
    pub julia_executable: String,
    pub julia_channel: Option<String>,
    pub worker_project: String,
    pub worker_args: String,
    pub threads_arg: Option<String>, // rendered "--threads=N,M" value, if any
    pub eval_expr: String,
    pub host_environ: HashMap<String, String>,
    pub setup_socket_path: String,
    pub worker_id: u32,
    pub host_home: String,
    pub extra_ro_binds: Vec<String>,
    pub extra_rw_binds: Vec<String>,
    pub empty_environment: bool,
    pub max_memory: Option<String>,
    pub max_cpu: Option<u32>,
}

/// Build argv/envp then fork+exec inside a user namespace sandbox.
/// Returns the intermediate process PID (child1) on success.
pub fn spawn_sandboxed(cfg: &SandboxConfig) -> Result<libc::pid_t, SandboxError> {
    let argv = build_argv(cfg)?;
    let envp = build_envp(cfg)?;
    exec_in_sandbox(&argv, &envp, cfg)
}

fn exec_in_sandbox(
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    cfg: &SandboxConfig,
) -> Result<libc::pid_t, SandboxError> {
    let orig_uid = unsafe { libc::getuid() };
    let orig_gid = unsafe { libc::getgid() };

    let pid1 = unsafe { libc::fork() };
    if pid1 < 0 { return Err(SandboxError::ForkFailed); }
    if pid1 != 0 { return Ok(pid1); }

    // --- Child 1: create namespaces ---
    unsafe {
        if libc::unshare(CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUSER) != 0 {
            fatal("unshare");
        }
        setup_id_maps(orig_uid, orig_gid);

        let pid2 = libc::fork();
        if pid2 < 0 { fatal("inner fork"); }

        if pid2 != 0 {
            // Child 1 waits for child 2 then exits with its code
            let mut status = 0;
            libc::waitpid(pid2, &mut status, 0);
            libc::_exit((status >> 8) & 0xff);
        }

        // --- Child 2: PID 1 inside namespace, set up filesystem then exec ---
        if setup_filesystem(cfg).is_err() { fatal("filesystem setup"); }
        if cfg.max_memory.is_some() || cfg.max_cpu.is_some() {
            let _ = setup_cgroup(cfg);
        }

        // Build null-terminated C arrays
        let mut c_argv: Vec<*const libc::c_char> = argv.iter()
            .map(|v| v.as_ptr() as *const libc::c_char)
            .collect();
        c_argv.push(std::ptr::null());
        let mut c_envp: Vec<*const libc::c_char> = envp.iter()
            .map(|v| v.as_ptr() as *const libc::c_char)
            .collect();
        c_envp.push(std::ptr::null());

        libc::execve(c_argv[0], c_argv.as_ptr(), c_envp.as_ptr());
        fatal("execve");
    }
}

unsafe fn setup_id_maps(uid: libc::uid_t, gid: libc::gid_t) {
    write_file_cstr(b"/proc/self/setgroups\0", b"deny");
    let uid_map = format!("0 {} 1\n", uid);
    write_file_cstr(b"/proc/self/uid_map\0", uid_map.as_bytes());
    let gid_map = format!("0 {} 1\n", gid);
    write_file_cstr(b"/proc/self/gid_map\0", gid_map.as_bytes());
}

unsafe fn setup_filesystem(cfg: &SandboxConfig) -> Result<(), SandboxError> {
    let home = &cfg.host_home;

    // Make existing mounts private so our changes don't propagate
    mount_flags(b"/\0", libc::MS_SLAVE as u32 | MS_REC);

    // Staging tmpfs
    mount_tmpfs(b"/tmp\0", MS_NOSUID | MS_NODEV, b"\0")?;
    libc::chdir(b"/tmp\0".as_ptr() as *const libc::c_char);

    mkdire(b"newroot\0")?;
    mount_bind(b"newroot\0", b"newroot\0")?;
    mkdire(b"oldroot\0")?;
    mkdire(b"ovl-upper\0")?;
    mkdire(b"ovl-work\0")?;

    let pivot_rc = libc::syscall(libc::SYS_pivot_root, b"/tmp\0".as_ptr(), b"oldroot\0".as_ptr());
    if pivot_rc != 0 { return Err(SandboxError::PivotRootFailed); }
    libc::chdir(b"/\0".as_ptr() as *const libc::c_char);

    // /dev
    mkdire(b"/newroot/dev\0")?;
    mount_tmpfs(b"/newroot/dev\0", MS_NOSUID | MS_NODEV, b"mode=0755\0")?;
    for name in &["null", "zero", "full", "random", "urandom", "tty"] {
        let src = format!("/oldroot/dev/{}\0", name);
        let dst = format!("/newroot/dev/{}\0", name);
        let src_bytes = src.as_bytes();
        let dst_bytes = dst.as_bytes();
        // touch then bind
        let fd = libc::openat(libc::AT_FDCWD, dst_bytes.as_ptr() as *const _,
            libc::O_WRONLY | libc::O_CREAT, 0o644);
        if fd >= 0 { libc::close(fd); }
        let _ = mount_bind_raw(src_bytes, dst_bytes);
    }
    libc::symlink(b"/proc/self/fd/0\0".as_ptr() as *const _, b"/newroot/dev/stdin\0".as_ptr() as *const _);
    libc::symlink(b"/proc/self/fd/1\0".as_ptr() as *const _, b"/newroot/dev/stdout\0".as_ptr() as *const _);
    libc::symlink(b"/proc/self/fd/2\0".as_ptr() as *const _, b"/newroot/dev/stderr\0".as_ptr() as *const _);
    libc::symlink(b"/proc/self/fd\0".as_ptr() as *const _, b"/newroot/dev/fd\0".as_ptr() as *const _);

    // /proc
    mkdire(b"/newroot/proc\0")?;
    mount_proc(b"/newroot/proc\0")?;

    // /tmp
    mkdire(b"/newroot/tmp\0")?;
    mount_tmpfs(b"/newroot/tmp\0", MS_NOSUID | MS_NODEV, b"mode=1777\0")?;

    // System dirs ro
    robind(b"/oldroot/usr\0", b"/newroot/usr\0")?;
    robind(b"/oldroot/etc\0", b"/newroot/etc\0")?;
    override_etc_file(b"/newroot/etc/passwd\0",
        b"root:x:0:0:root:/root:/bin/sh\nsandbox:x:0:0:sandbox:/home/sandbox:/bin/sh\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n");
    override_etc_file(b"/newroot/etc/group\0",
        b"root:x:0:\nsandbox:x:0:\nnogroup:x:65534:\n");
    override_etc_file(b"/newroot/etc/nsswitch.conf\0",
        b"passwd: files\ngroup:  files\nhosts:  files dns\n");

    robind_optional(b"/oldroot/usr/lib\0", b"/newroot/lib\0");
    robind_optional(b"/oldroot/usr/lib64\0", b"/newroot/lib64\0");
    robind_optional(b"/oldroot/usr/bin\0", b"/newroot/bin\0");
    robind_optional(b"/oldroot/opt\0", b"/newroot/opt\0");

    // /home
    mkdire(b"/newroot/home\0")?;
    mount_tmpfs(b"/newroot/home\0", MS_NOSUID | MS_NODEV, b"mode=0755\0")?;
    if !home.is_empty() {
        // juliaup config (rw for lockfile)
        let juliaup_src = format!("/oldroot{}/.julia/juliaup\0", home);
        mkdirp(b"/newroot/root/.julia/juliaup\0");
        let _ = mount_bind_raw(juliaup_src.as_bytes(), b"/newroot/root/.julia/juliaup\0");

        // Depot overlay
        let depot = format!("/newroot{}/.julia\0", home);
        mkdirp(depot.as_bytes());
        let opts = format!("upperdir=/ovl-upper,workdir=/ovl-work,lowerdir=/oldroot{}/.julia,userxattr\0", home);
        if mount_overlay(depot.as_bytes(), opts.as_bytes()).is_err() {
            eprintln!("Sandbox: overlay failed, falling back to bind");
            let src = format!("/oldroot{}/.julia\0", home);
            robind_optional(src.as_bytes(), depot.as_bytes());
        } else if cfg.empty_environment {
            let env_path = format!("/newroot{}/.julia/environments\0", home);
            mkdirp(env_path.as_bytes());
            let _ = mount_tmpfs(env_path.as_bytes(), MS_NOSUID | MS_NODEV, b"mode=0755\0");
        }

        mkdire(b"/newroot/home/sandbox\0")?;
        let link_target = format!("{}/.julia\0", home);
        libc::symlink(link_target.as_ptr() as *const _, b"/newroot/home/sandbox/.julia\0".as_ptr() as *const _);
    }

    // Extra ro binds
    for path in &cfg.extra_ro_binds {
        if path.is_empty() { continue; }
        let src = format!("/oldroot{}\0", path);
        let dst = format!("/newroot{}\0", path);
        mkdirp(dst.as_bytes());
        robind_optional(src.as_bytes(), dst.as_bytes());
    }

    // Extra rw binds
    for path in &cfg.extra_rw_binds {
        if path.is_empty() { continue; }
        let src = format!("/oldroot{}\0", path);
        let dst = format!("/newroot{}\0", path);
        mkdirp(dst.as_bytes());
        let _ = mount_bind_raw(src.as_bytes(), dst.as_bytes());
    }

    // Per-worker socket dir (for setup socket)
    if !cfg.setup_socket_path.is_empty() {
        if let Some(sep) = cfg.setup_socket_path.rfind('/') {
            let runtime_dir = &cfg.setup_socket_path[..sep];
            let src = format!("/oldroot{}\0", runtime_dir);
            let dst = format!("/newroot{}\0", runtime_dir);
            mkdirp(dst.as_bytes());
            let _ = mount_bind_raw(src.as_bytes(), dst.as_bytes());
        }
    }

    // Final pivot
    mount_flags(b"oldroot\0", MS_REC | 0x40000 /* MS_PRIVATE */);
    if libc::chdir(b"/newroot\0".as_ptr() as *const libc::c_char) != 0 {
        return Err(SandboxError::ChdirFailed);
    }
    let pivot_rc = libc::syscall(libc::SYS_pivot_root, b".\0".as_ptr(), b".\0".as_ptr());
    if pivot_rc != 0 { return Err(SandboxError::PivotRootFailed); }
    if libc::chdir(b"/\0".as_ptr() as *const libc::c_char) != 0 {
        return Err(SandboxError::ChdirFailed);
    }
    libc::umount2(b".\0".as_ptr() as *const libc::c_char, MNT_DETACH as libc::c_int);
    libc::chdir(b"/home/sandbox\0".as_ptr() as *const libc::c_char);

    Ok(())
}

unsafe fn setup_cgroup(cfg: &SandboxConfig) -> Result<(), ()> {
    let cg = format!("/sys/fs/cgroup/julia-sandbox-{}\0", cfg.worker_id);
    let cg_path = &cg[..cg.len()-1]; // without trailing null for mkdir
    std::fs::create_dir_all(cg_path).map_err(|_| ())?;

    if let Some(mem) = &cfg.max_memory {
        let path = format!("{}/memory.max", cg_path);
        std::fs::write(&path, mem).ok();
    }
    if let Some(cpu) = cfg.max_cpu {
        let path = format!("{}/cpu.max", cg_path);
        let val = format!("{} 100000", cpu as u64 * 1000);
        std::fs::write(&path, &val).ok();
    }
    let procs = format!("{}/cgroup.procs", cg_path);
    std::fs::write(&procs, "0").map_err(|_| ())?;
    Ok(())
}

// --- Mount helpers ---

unsafe fn mount_flags(target: &[u8], flags: u32) {
    libc::mount(
        std::ptr::null(),
        target.as_ptr() as *const libc::c_char,
        std::ptr::null(),
        flags as libc::c_ulong,
        std::ptr::null(),
    );
}

unsafe fn mount_bind(source: &[u8], target: &[u8]) -> Result<(), SandboxError> {
    mount_bind_raw(source, target).map_err(|_| SandboxError::MountFailed)
}

unsafe fn mount_bind_raw(source: &[u8], target: &[u8]) -> Result<(), ()> {
    let rc = libc::mount(
        source.as_ptr() as *const libc::c_char,
        target.as_ptr() as *const libc::c_char,
        std::ptr::null(),
        (MS_BIND | MS_REC | MS_SILENT) as libc::c_ulong,
        std::ptr::null(),
    );
    if rc != 0 { Err(()) } else { Ok(()) }
}

unsafe fn mount_tmpfs(target: &[u8], flags: u32, opts: &[u8]) -> Result<(), SandboxError> {
    let rc = libc::mount(
        b"tmpfs\0".as_ptr() as *const libc::c_char,
        target.as_ptr() as *const libc::c_char,
        b"tmpfs\0".as_ptr() as *const libc::c_char,
        flags as libc::c_ulong,
        opts.as_ptr() as *const libc::c_void,
    );
    if rc != 0 { Err(SandboxError::MountFailed) } else { Ok(()) }
}

unsafe fn mount_proc(target: &[u8]) -> Result<(), SandboxError> {
    let rc = libc::mount(
        b"proc\0".as_ptr() as *const libc::c_char,
        target.as_ptr() as *const libc::c_char,
        b"proc\0".as_ptr() as *const libc::c_char,
        (MS_NOSUID | MS_NODEV | MS_NOEXEC) as libc::c_ulong,
        std::ptr::null(),
    );
    if rc != 0 { Err(SandboxError::MountFailed) } else { Ok(()) }
}

unsafe fn mount_overlay(target: &[u8], opts: &[u8]) -> Result<(), ()> {
    let rc = libc::mount(
        b"overlay\0".as_ptr() as *const libc::c_char,
        target.as_ptr() as *const libc::c_char,
        b"overlay\0".as_ptr() as *const libc::c_char,
        0,
        opts.as_ptr() as *const libc::c_void,
    );
    if rc != 0 { Err(()) } else { Ok(()) }
}

unsafe fn robind(source: &[u8], target: &[u8]) -> Result<(), SandboxError> {
    mkdire(target)?;
    mount_bind(source, target)?;
    remount_readonly(target);
    Ok(())
}

unsafe fn robind_optional(source: &[u8], target: &[u8]) {
    if mkdire(target).is_err() { return; }
    if mount_bind_raw(source, target).is_err() { return; }
    remount_readonly(target);
}

unsafe fn remount_readonly(target: &[u8]) {
    libc::mount(
        b"none\0".as_ptr() as *const libc::c_char,
        target.as_ptr() as *const libc::c_char,
        std::ptr::null(),
        (MS_RDONLY | MS_NOSUID | MS_NODEV | MS_REMOUNT | MS_BIND | MS_SILENT) as libc::c_ulong,
        std::ptr::null(),
    );
}

unsafe fn mkdire(path: &[u8]) -> Result<(), SandboxError> {
    let rc = libc::mkdir(path.as_ptr() as *const libc::c_char, 0o755);
    if rc != 0 && *libc::__errno_location() != libc::EEXIST {
        Err(SandboxError::MkdirFailed)
    } else {
        Ok(())
    }
}

unsafe fn mkdirp(path: &[u8]) {
    let path_str = std::str::from_utf8(path).unwrap_or("").trim_end_matches('\0');
    let _ = std::fs::create_dir_all(path_str);
}

static ETC_COUNTER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

unsafe fn override_etc_file(target: &[u8], content: &[u8]) {
    let idx = ETC_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let src = format!("/etc-override-{}\0", idx);
    create_file(src.as_bytes(), content);
    let _ = mount_bind_raw(src.as_bytes(), target);
    remount_readonly(target);
}

unsafe fn create_file(path: &[u8], data: &[u8]) {
    let fd = libc::openat(libc::AT_FDCWD, path.as_ptr() as *const _,
        libc::O_WRONLY | libc::O_CREAT, 0o644);
    if fd >= 0 {
        libc::write(fd, data.as_ptr() as *const _, data.len());
        libc::close(fd);
    }
}

unsafe fn write_file_cstr(path: &[u8], data: &[u8]) {
    let fd = libc::openat(libc::AT_FDCWD, path.as_ptr() as *const _, libc::O_WRONLY, 0);
    if fd >= 0 {
        libc::write(fd, data.as_ptr() as *const _, data.len());
        libc::close(fd);
    }
}

unsafe fn fatal(msg: &str) -> ! {
    eprintln!("Sandbox fatal: {}", msg);
    libc::_exit(126);
}

// --- Argv/envp construction ---

const ENV_ALLOWLIST: &[&str] = &[
    "LANG", "LC_CTYPE", "LC_ALL", "TERM", "COLORTERM",
    "PATH", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME",
    "OPENBLAS_MAIN_FREE", "OPENBLAS_DEFAULT_NUM_THREADS", "CUDA_CACHE_PATH",
];

const ENV_MANAGED: &[&str] = &[
    "HOME", "USER", "LOGNAME", "PATH", "JULIA_DEPOT_PATH", "JULIA_DAEMON_REVISE",
];

pub fn env_allowed(key: &str) -> bool {
    if key.starts_with("JULIA_") { return true; }
    ENV_ALLOWLIST.iter().any(|&k| k == key)
}

fn build_argv(cfg: &SandboxConfig) -> Result<Vec<Vec<u8>>, SandboxError> {
    let mut argv = Vec::new();
    let push = |v: &mut Vec<Vec<u8>>, s: &str| {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        v.push(b);
    };

    push(&mut argv, &cfg.julia_executable);
    if let Some(ch) = &cfg.julia_channel { push(&mut argv, ch); }
    if !cfg.worker_project.is_empty() {
        push(&mut argv, &format!("--project={}", cfg.worker_project));
    }
    for arg in cfg.worker_args.split_whitespace() { push(&mut argv, arg); }
    if let Some(t) = &cfg.threads_arg { push(&mut argv, &format!("--threads={}", t)); }
    push(&mut argv, "--eval");
    push(&mut argv, &cfg.eval_expr);
    Ok(argv)
}

fn build_envp(cfg: &SandboxConfig) -> Result<Vec<Vec<u8>>, SandboxError> {
    let mut envp = Vec::new();
    let push = |v: &mut Vec<Vec<u8>>, s: &str| {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        v.push(b);
    };

    for (key, value) in &cfg.host_environ {
        if ENV_MANAGED.iter().any(|&m| m == key) { continue; }
        if env_allowed(key) {
            push(&mut envp, &format!("{}={}", key, value));
        }
    }

    push(&mut envp, "HOME=/home/sandbox");
    push(&mut envp, "USER=sandbox");
    push(&mut envp, "LOGNAME=sandbox");
    if !cfg.host_home.is_empty() {
        push(&mut envp, &format!("PATH={}/.julia/juliaup/bin:/usr/local/bin:/usr/bin:/bin", cfg.host_home));
        push(&mut envp, &format!("JULIA_DEPOT_PATH={}/.julia", cfg.host_home));
    } else {
        push(&mut envp, "PATH=/usr/local/bin:/usr/bin:/bin");
    }
    push(&mut envp, "JULIA_DAEMON_REVISE=no");
    Ok(envp)
}

pub fn cleanup_cgroup(worker_id: u32) {
    let path = format!("/sys/fs/cgroup/julia-sandbox-{}", worker_id);
    let cpath = format!("{}\0", path);
    unsafe {
        libc::unlinkat(
            libc::AT_FDCWD,
            cpath.as_ptr() as *const _,
            libc::AT_REMOVEDIR,
        );
    }
}
