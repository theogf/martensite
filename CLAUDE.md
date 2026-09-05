# martensite

Steel plugin for Helix that sends Julia code to a live REPL served by
[JuliaDaemon.jl](https://github.com/KristofferC/JuliaDaemon.jl) (`jld`).

The plugin is the whole product: `martensite.scm` is the only code in this repo.
There is no daemon, no binary, no service unit, and no install script — the
installation is `Pkg.app add` for `jld` plus one `(require ...)` line in Helix's
`init.scm`.

> Until 2026-09, this repo also carried a Rust rewrite of DaemonicCabal.jl's
> `julia-conductor`/`juliaclient` (a worker-pool daemon, a systemd unit, an
> `install.sh`, and a `DaemonWorker` Julia package copied out of a checkout).
> All of it was deleted in favour of `jld`. Anything referring to a conductor,
> a worker pool, `quench`/`temper` as installed scripts, or the `JULIA_DAEMON_*`
> environment variables is describing code that no longer exists — check
> `git log` before `7f68c6c` if you need it.

## Steel/Helix API Reference

When looking up Steel or Helix plugin APIs, refer to:
https://github.com/mattwparas/helix/blob/steel-event-system/STEEL.md

Steel's own primitives are easiest to look up in the vendored git db, which
holds the exact revision Helix was built against:

```sh
cd ~/.cargo/git/db/steel-*/ && git grep -n 'name = "split-once"' $(git rev-list --all -1)
```

`jld`'s source is likewise readable at `~/.julia/packages/JuliaDaemon/*/src/`.

## Architecture

Two paths reach the same `Main`; they differ in who evaluates the code, and so
in where the result comes out.

| Commands | Runs | Result appears | `ans` |
|---|---|---|---|
| `send-to-julia-repl`, `send-top-level-to-julia-repl` | `jld eval-repl` | the developer's terminal | set |
| `eval-in-julia`, `eval-top-level-in-julia` | `jld eval` | Helix popup | not set |

**Neither mode falls back to the other.** An early version retried a failed
paste as a captured eval; that blurred the single distinction the two commands
exist to express. With no REPL attached, `send-*` surfaces `jld`'s own error
(`no REPL attached to <id>; start one with jld connect`) and stops.

`jld eval` returns exactly what a REPL would show — streamed stdout/stderr plus
the rendered value, honoring `nothing` and a trailing `;` — so the popup path
needs nothing upstream.

`eval-repl` is a genuine paste: `JuliaDaemon/src/repl_input.jl` writes the code
into the REPL's `stdin` buffer wrapped in bracketed-paste markers, so LineEdit
cannot tell it from a terminal paste — echoed, evaluated by the REPL, prompt
redrawn, with any half-typed input stashed and restored around it.

### `jld connect` has two prompts, and pastes follow the active one

Verified by pasting `getpid()` in each: at `julia@<id>>` it reaches the daemon;
after a backspace to the plain `julia>` the identical paste evaluates in the
connect script's *own* local Julia (a different pid, no project, no Revise).

martensite cannot detect or fix this — `repl.sock` injects into the tty buffer
and has no mode awareness. `>` at the empty `julia>` returns to the daemon mode
(`install_mode`'s `enter_key` in `connect_repl.jl`, merged into the main mode
like `]`/`?`/`;`). Topology B (`JuliaDaemon.serve()`) has no such split and is
immune.

If a user reports "sends go somewhere wrong" or "UndefVarError for things that
are definitely loaded", check which prompt they are sitting at first.

### Session identity

A daemon is keyed on **project + name**. `jld` derives the project itself by
walking up from the subprocess cwd (inherited from Helix); the plugin supplies
only the name, resolved by `resolve-session`:

`MARTENSITE_SESSION` → `JLD_NAME` → `.juliasession` first line → Zellij tab →
tmux window → `"repl"`.

Two things that are easy to get wrong here:

- **`"repl"` is the fallback for a reason** — it is what `JuliaDaemon.serve()`
  picks with no arguments. A *spawned* daemon's default name is the empty
  string (`client.jl` `make_ctx`), which is a **different id**. So the name is
  always passed explicitly on both sides rather than omitted.
- **Do not pass `--project`.** `jld`'s `find_project` walks *up* from the cwd,
  whereas an explicit `--project=DIR` demands a `Project.toml` at exactly that
  directory and `die`s otherwise.

## Steel gotchas

Both of these were found by running the code, not by reading it — see Testing.

- **`wait` returns a `Result`, not an integer.** It prints as `(Ok 0)`.
  Comparing it to `0` directly is silently always false; unwrap via `Ok?` /
  `unwrap-ok` (`exit-code` in `martensite.scm` does this).
- **`zellij action current-tab-info` prints several `key: value` lines**
  (`name:`, `id:`, `position:`, ...). Take the first line *before* splitting on
  `": "`, or the tab name comes back with the rest of the report attached.
- **Piped output is not a pty**, so it carries bare `\n`. The popup's VTE is a
  faithful raw terminal emulator: `\n` is a linefeed only, and without rewriting
  to `\r\n` every line staircases further right.
- **Grab both child port handles before `wait`ing.** `wait->stdout` consumes
  the whole child handle internally, leaving nothing to pull stderr from.
  Piping stderr is mandatory — Julia writes errors there, and an unpiped stderr
  is inherited from Helix and writes straight past the TUI compositor.

## Testing

There is no test suite; the plugin is exercised directly. Two techniques cover
almost everything without launching Helix:

**Run the non-Helix half under the `steel` CLI.** The session-resolution and
`jld` layers require only `steel/process`, `steel/ports`, `steel/meta`,
`steel/filesystem` and `steel/result` — none of the `helix/*` modules — so they
can be sliced out and executed standalone:

```sh
sed -n '/^;; ─── Session resolution/,/^;; ─── Output popup/p' martensite.scm > /tmp/body.scm
# prepend the five require-builtins, append some displayln calls, then:
steel /tmp/probe.scm
```

**Drive a real REPL under a pty** to exercise the paste path end to end. `jld
connect` needs a tty, and `serve_input` only runs when `isinteractive()`:

```sh
mkfifo /tmp/in; (sleep 90 > /tmp/in) &          # holds stdin open
script -qfc "jld connect --name=<session>" /tmp/repl.log < /tmp/in &
jld --name=<session> eval-repl '6*7'            # expect exit 0
sed -e 's/\x1b\[[0-9;?]*[a-zA-Z]//g' -e 's/\r/\n/g' /tmp/repl.log   # expect 42
```

Clean up afterwards: `jld --id=<id> stop` for anything spawned, then `jld gc`.
Never `jld kill` a session that is a human's live REPL.

`test.jl` is a manual fixture for the popup — it ends in `error("oh no")` so a
send exercises stacktrace rendering.

## Upstream gaps

One, in JuliaDaemon.jl, worked around in `martensite.scm` rather than patched
locally. Filed as
[KristofferC/JuliaDaemon.jl#5](https://github.com/KristofferC/JuliaDaemon.jl/issues/5).

(`eval-repl` also never returns the evaluated result — `serve_input`
acknowledges as soon as the bytes are in the tty buffer, and `cmd_eval_repl`
reads only that `done` frame. This is deliberately *not* treated as a gap: it
is what makes the paste mode the paste mode. Don't build on the transcript to
work around it — that races the evaluation, and a session daemon records inputs
with empty output anyway.)

1. **`jld eval` output is always monochrome.** The request struct has a
   `color::Bool` field (`daemon.jl`), parsed from the request and threaded into
   both `render` and `format_error` as `IOContext(:color => ...)` — but only
   `connect_repl.jl` ever sets it true, and the CLI has no flag. The daemon side
   is complete; only a client flag is missing.

The popup's VTE machinery is kept despite this: it costs nothing, still does the
line-wrapping the popup relies on, and would light up for free if a `--color`
flag lands.
