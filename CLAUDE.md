# martensite

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

The Rust binaries track DaemonicCabal.jl 0.5.0's wire protocol, including the additions from that release: `query_clients`/`clients`/`drop_session` worker messages, the `client_interrupt` notification, and the `executing` signal.

### Key env vars (conductor)

| Variable | Default | Description |
|----------|---------|-------------|
| `JULIA_DAEMON_WORKER_PROJECT` | `~/.local/share/julia-daemon/worker` | Julia project/environment for workers |
| `JULIA_DAEMON_SERVER` | `<runtime_dir>/conductor.sock` | Socket path or `tcp://host:port` |
| `JULIA_DAEMON_RUNTIME` | `/run/user/$UID/julia-daemon` | Runtime directory |
| `JULIA_DAEMON_WORKER_EXECUTABLE` | `julia` | Julia binary |
| `JULIA_DAEMON_WORKER_MAXCLIENTS` | `1` | Max clients per worker |
| `JULIA_DAEMON_MIN_TTL` | `120` | Idle floor (seconds): a worker younger than this is never pressure-evicted |
| `JULIA_DAEMON_MAX_TTL` | `7200` | Idle ceiling (seconds): always culled once idle past this, pressure or not. Supersedes the old `JULIA_DAEMON_WORKER_TTL`, which is still read as its fallback default |
| `JULIA_DAEMON_MEMORY_PRESSURE` | `1` | Master switch for pressure-reactive eviction; `0` disables it (flat MIN/MAX_TTL culling still applies) |
| `JULIA_DAEMON_PSI_THRESHOLD` | `10.0` | PSI `some avg10` percent that counts as pressure, when `/proc/pressure/memory` is available |
| `JULIA_DAEMON_MEMFREE_LOW` / `_HIGH` | `10%` / `15%` | Free-memory enter/exit thresholds (fraction of total, or bytes with a `K`/`M`/`G` suffix), used as a fallback when PSI isn't available |

The actual idle budget a given worker gets is adaptive, not a flat MIN/MAX_TTL cutoff — see `idle_budget` in `conductor/src/conductor.rs` and the recency/frequency + occupancy tracking in `conductor/src/worker.rs` (`Crf`, `Ewma`).

### Installation

```bash
./install.sh            # build release, install service + symlinks
./install.sh uninstall  # remove everything
```

What it does:
1. `cargo build --release`
2. Copies `worker/` from `$DAEMONIC_CABAL_SRC` (default `~/.julia/dev/DaemonicCabal`) to `~/.local/share/julia-daemon/worker` — this is `DaemonWorker`, a self-contained Julia package (stdlib deps only) that isn't a registered package, so it can't be `Pkg.add`ed; it has to be copied from a DaemonicCabal.jl checkout, exactly like DaemonicCabal.jl's own installer does (`src/installers/common.jl`)
3. Copies binaries to `~/.local/share/julia-daemon/`
4. Writes `~/.config/systemd/user/julia-daemon.service` (with `JULIA_DAEMON_WORKER_PROJECT=~/.local/share/julia-daemon/worker`) and enables it
5. Symlinks `juliaclient` → `~/.local/bin/juliaclient`
6. Copies `quench.sh`/`temper.sh` → `~/.local/bin/quench`/`~/.local/bin/temper`

### DaemonicCabal patches

As of DaemonicCabal.jl 0.5.0, the previously-required local patch is upstream and no longer needs manual reapplication:

**`dup: Bad file descriptor` on REPL exit** — on Julia < 1.11, `redirect_stdio` cleanup calls `dup` on a file descriptor that is already closed when the client disconnects. Upstream now carries the guard itself (`worker/src/setup.jl`, inside the `teardown_client` cleanup):

```julia
catch err
    err isa Base.SystemError && occursin("dup", err.msg) && return
    isopen(client_stdout) && rethrow()
end
```

If `~/.julia/dev/DaemonicCabal` still has an uncommitted local copy of this patch from before the 0.5.0 upgrade, it's safe to drop — just confirm the checkout is actually on 0.5.0+ first.

### Design notes

- **No async runtime** — synchronous I/O matches the Zig single-threaded model and keeps `fork()` safe for sandbox spawning.
- **Conductor** uses a `select()`-based accept loop, with background threads for periodic ping health checks, min/max-TTL idle culling, and (when a pressure source is detected) memory-pressure eviction. Worker teardown is staged (`soft_exit` → grace → `SIGTERM` → grace → `SIGKILL`), tracked in a pending-kill list swept by those same timers rather than killed in place.
- **Idle budget** is adaptive per worker, not a flat TTL: a recency/frequency score per pool key (`Crf`, an LRFU-style value plus a Jacobson/RFC6298 inter-summon interval estimate) combines with a decayed busy-fraction estimate per worker (`Ewma`) to size how long it's worth keeping a given worker around — see `Conductor::idle_budget`.
- **`--threads`/`-t` is part of a worker's pool identity** — Julia fixes thread counts at process startup, so a worker spawned with a different `--threads` spec can't be reused for a request wanting a different one (same treatment as a project or Julia-channel mismatch).
- **`--status`/`--status=live`** renders a tree of workers/clients/pressure state from the conductor's own in-memory data (no worker-protocol round trip). Live mode redraws in place from a background thread that re-locks the conductor briefly per frame, so it never blocks the main accept loop; teardown is notification-driven (`client_exit`/`client_interrupt`), not a bespoke keypress handler. Unlike upstream, this port renders with flat ANSI colors rather than an OSC-probed truecolor palette — a deliberate scope cut, not a compatibility gap.
- **Client** uses `epoll` (Linux) or `poll` (fallback) to multiplex stdin/stdout/stderr/signals. `Ctrl-C` is delivered as a literal `\x03` on stdin when the worker is at a raw prompt, or as a `client_interrupt` notification (conductor sends the worker process `SIGINT` directly) when mid-eval — gated by the worker's `executing` signal.
- **Sandbox** (Linux only) uses unprivileged user namespaces (`unshare`/`pivot_root`/bind mounts) via raw `libc` syscalls — no root required.
