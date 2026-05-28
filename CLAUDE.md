# julia-steel

Steel plugin for Helix that sends Julia code to a DaemonicCabal.jl session.

## Steel/Helix API Reference

When looking up Steel or Helix plugin APIs, refer to:
https://github.com/mattwparas/helix/blob/steel-event-system/STEEL.md

## Rust binaries

This repo contains a Cargo workspace with Rust rewrites of the Zig `julia-conductor` and `juliaclient` binaries originally from `~/.julia/dev/DaemonicCabal/`.

```
cargo build                        # builds both binaries
target/debug/julia-conductor       # conductor daemon
target/debug/juliaclient           # client (drop-in for `julia`)
```

### Structure

| Crate | Binary | Role |
|-------|--------|------|
| `conductor/` | `julia-conductor` | Worker pool daemon — accepts client connections, spawns Julia workers, routes stdio |
| `client/` | `juliaclient` | Client — connects to conductor, proxies stdin/stdout/stderr/signals to assigned worker |

### Protocol compatibility

Wire format is identical to the Zig implementation:

- `CLIENT_MAGIC = 0x4A444301` ("JDC\x01")
- `WORKER_MAGIC = 0x4A445701` ("JDW\x01")
- `NOTIFICATION_MAGIC = 0x4A444E01` ("JDN\x01")
- All integers little-endian, strings length-prefixed with u16

The Rust binaries are fully compatible with DaemonicCabal's Julia worker (`DaemonWorker.jl`).

### Key env vars (conductor)

| Variable | Default | Description |
|----------|---------|-------------|
| `JULIA_DAEMON_WORKER_PROJECT` | *(required)* | Path to DaemonWorker project |
| `JULIA_DAEMON_SERVER` | `<runtime_dir>/conductor.sock` | Socket path or `tcp://host:port` |
| `JULIA_DAEMON_RUNTIME` | `/run/user/$UID/julia-daemon` | Runtime directory |
| `JULIA_DAEMON_WORKER_EXECUTABLE` | `julia` | Julia binary |
| `JULIA_DAEMON_WORKER_MAXCLIENTS` | `1` | Max clients per worker |
| `JULIA_DAEMON_WORKER_TTL` | `7200` | Worker idle timeout (seconds) |

### Design notes

- **No async runtime** — synchronous I/O matches the Zig single-threaded model and keeps `fork()` safe for sandbox spawning.
- **Conductor** uses a `select()`-based accept loop with a background thread for periodic ping health checks.
- **Client** uses `epoll` (Linux) or `poll` (fallback) to multiplex stdin/stdout/stderr/signals.
- **Sandbox** (Linux only) uses unprivileged user namespaces (`unshare`/`pivot_root`/bind mounts) via raw `libc` syscalls — no root required.
