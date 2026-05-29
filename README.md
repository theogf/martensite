# martensite

[![Docs](https://img.shields.io/badge/docs-architecture-blue?style=flat-square)](https://theogf.dev/martensite/architecture.html)

> *Martensite* is the hardest phase of steel, formed by rapidly quenching austenite — a metaphor for JIT-compiled Julia hardened through the Steel scripting layer.

A [Steel](https://github.com/mattwparas/steel) plugin for [Helix](https://helix-editor.com) that sends Julia code to a running [DaemonicCabal.jl](https://github.com/tecosaur/DaemonicCabal.jl) session.

> [!WARNING]
> Getting this setup requires advanced knowledge in git and linux setup

## Prerequisites

### 1. Helix — steel branch

Follow the instructions [here](https://github.com/mattwparas/helix/blob/steel-event-system/STEEL.md) to install the correct version of Helix with the plugin system.

> [!NOTE]
> Point `HELIX_RUNTIME` at the runtime directory if Helix can't find its queries:
> ```sh
> export HELIX_RUNTIME=/path/to/helix/runtime
> ```

### 2. Julia conductor/client

This repo ships Rust rewrites of the [DaemonicCabal](https://github.com/tecosaur/DaemonicCabal.jl) conductor and client binaries, derived directly from the Zig implementation and wire-compatible with the Julia worker. Build and install them with:

```sh
./install.sh
```

This builds the binaries, installs the systemd user service, and symlinks `juliaclient` and `quench` to `~/.local/bin/`.

See [CLAUDE.md](CLAUDE.md) for details on the Rust binaries and protocol.

### 3. Starting a Julia session

Use the provided `quench` script, which resolves the session name automatically (see [Session resolution](#session-resolution)):

```sh
quench
```

Set this as the command for your Julia pane in your Zellij/Tmux layout.

## Installation

Require the plugin from your Helix `init.scm` (`~/.config/helix/init.scm`):

```scheme
(require "/path/to/martensite/martensite.scm")
```

## Usage

Two commands are available:

| Command | Description |
|---|---|
| `send-to-julia-repl` | Send the last-yanked text (`"` register) to the session |
| `send-top-level-to-julia-repl` | Send the top-level tree-sitter form under the cursor |

Bind them in `~/.config/helix/config.toml`:

```toml
[keys.normal]
C-j = ":send-to-julia-repl"
C-S-j = ":send-top-level-to-julia-repl"

[keys.select]
C-j = ":send-to-julia-repl"
```

**Workflow:**
- `send-to-julia-repl` — yank the code you want to send (`y`), then press `C-j`
- `send-top-level-to-julia-repl` — place the cursor anywhere inside a function/block and press `C-S-j`; it walks the tree-sitter parse tree up to the top-level node and sends it automatically

Output from the Julia session is shown in a vsplit buffer.

## Session resolution

Both the Helix plugin and `quench`/`temper` resolve the session name using the same cascade:

1. **`.juliasession` file** — if a `.juliasession` file exists in the project root, its first line is used as the session name.
2. **Zellij tab name** — if running inside Zellij, the current tab name is used.
3. **Tmux window name** — if running inside tmux, the current window name is used.
4. **Working directory** — fallback to the CWD of the Helix process.

Name your Zellij tabs or tmux windows meaningfully and both sides will find each other automatically.
