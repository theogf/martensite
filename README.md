# martensite

[![Docs](https://img.shields.io/badge/docs-architecture-blue?style=flat-square)](https://theogf.dev/martensite/architecture.html)

> *Martensite* is the hardest phase of steel, formed by rapidly quenching austenite — a metaphor for JIT-compiled Julia hardened through the Steel scripting layer.

A [Steel](https://github.com/mattwparas/steel) plugin for [Helix](https://helix-editor.com) that sends Julia code to a live REPL served by [JuliaDaemon.jl](https://github.com/KristofferC/JuliaDaemon.jl).

The whole plugin is one file, `martensite.scm`. There is no daemon to install, no
systemd unit, no binary to build, and no install script to run.

## Prerequisites

### 1. Helix — steel branch

Follow the instructions [here](https://github.com/mattwparas/helix/blob/steel-event-system/STEEL.md) to install the correct version of Helix with the plugin system.

> [!NOTE]
> Point `HELIX_RUNTIME` at the runtime directory if Helix can't find its queries:
> ```sh
> export HELIX_RUNTIME=/path/to/helix/runtime
> ```

### 2. `jld`

```sh
julia -e 'using Pkg; Pkg.app add url="https://github.com/KristofferC/JuliaDaemon.jl"'
```

That installs `jld` into `~/.julia/bin`. Put it on your `PATH` (`jld install`
will symlink it into `~/.local/bin` if it isn't already).

### 3. A named Julia session

Both sides have to agree on a session name — `repl` unless you override it (see
[Session resolution](#session-resolution)). Either topology works:

**A — daemon owns `Main`, thin REPL attached** (survives closing the terminal):

```sh
jld connect --name=repl
```

**B — your own `julia` serves itself** (dies with the terminal):

```julia
using JuliaDaemon
JuliaDaemon.serve(name="repl")
```

Set whichever you prefer as the command for your Julia pane in your Zellij/Tmux
layout. Neither is required to be running before you send code: if no REPL is
attached, sends fall back to a captured evaluation in the same daemon and the
result is shown in the popup instead of the terminal.

## Installation

Require the plugin from your Helix `init.scm` (`~/.config/helix/init.scm`):

```scheme
(require "/path/to/martensite/martensite.scm")
```

That's the whole installation.

## Usage

| Command | Sends | Result appears |
|---|---|---|
| `send-to-julia-repl` | last-yanked text (`.` register) | in your REPL, at the prompt, `ans` set |
| `send-top-level-to-julia-repl` | top-level tree-sitter form under the cursor | in your REPL, at the prompt, `ans` set |
| `eval-in-julia` | last-yanked text | in a popup; your prompt is untouched |
| `eval-top-level-in-julia` | top-level form under the cursor | in a popup; your prompt is untouched |

Bind them in `~/.config/helix/config.toml`:

```toml
[keys.normal]
C-j = ":send-to-julia-repl"
C-S-j = ":send-top-level-to-julia-repl"
A-j = ":eval-in-julia"

[keys.select]
C-j = ":send-to-julia-repl"
```

**Workflow:**
- `send-to-julia-repl` — yank the code you want to send (`y`), then press `C-j`
- `send-top-level-to-julia-repl` — place the cursor anywhere inside a function/block and press `C-S-j`; it walks the tree-sitter parse tree up to the top-level node and sends it automatically

The `send-*` pair is a real paste into the prompt: bracketed-paste injection, so
the code is echoed, evaluated by the REPL itself, and sets `ans` — and any
half-typed input of yours is stashed and put back afterwards. Because the REPL
owns the evaluation, its output goes to your terminal, not back to Helix.

The `eval-*` pair is the opposite trade: output comes back and lands in a
floating popup (dismiss with any keypress), and your prompt is never touched.

## Session resolution

`jld` keys a daemon on the *project* — the nearest `Project.toml` walking up
from Helix's working directory — so the session name only has to disambiguate
several sessions on one project. The plugin resolves it as:

1. **`MARTENSITE_SESSION`** environment variable, if set.
2. **`.juliasession` file** — its first line, if the file exists in the project root.
3. **Zellij tab name** — if Helix is running inside Zellij.
4. **Tmux window name** — if Helix is running inside tmux.
5. **`repl`** — the default, which is also what `JuliaDaemon.serve()` picks with
   no arguments.

Whatever it resolves to must match the `--name=` you gave `jld connect`, or the
`name=` you gave `JuliaDaemon.serve`. Name your Zellij tabs or tmux windows
meaningfully and both sides find each other automatically.

## Agents

Agents share the session without touching your prompt. Given the id from
`jld list`:

```sh
jld --id=<id> eval '<code>'      # captured output; your prompt is untouched
jld --id=<id> eval --scratch ... # throwaway module that can't clobber Main's bindings
jld --id=<id> transcript         # read what you have been doing
```

`jld install` drops an agent skill into `~/.agents/skills` (and the skills
directories of installed agents) documenting this, including the rule never to
kill a session that is a human's live REPL.
