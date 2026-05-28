# julia-steel

A [Steel](https://github.com/mattwparas/steel) plugin for [Helix](https://helix-editor.com) that sends the current selection to a running [DaemonicCabal.jl](https://github.com/tecosaur/DaemonicCabal.jl) session.

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

### 2. DaemonicCabal.jl

Install DaemonicCabal into your global Julia environment so `juliaclient` is available system-wide:

```sh
julia --startup-file=no -e 'using Pkg; Pkg.dev(url="https://github.com/tecosaur/DaemonicCabal.jl")'
```

Make sure `juliaclient` ends up on your `PATH` (check `DaemonicCabal`'s README for the exact build/install step for the Zig client binary).

### 3. Starting a Julia session

Launch Julia through `juliaclient` so it runs inside a DaemonicCabal-managed worker for your project. The session name must match what the plugin resolves (see [Session resolution](#session-resolution) below).

```sh
juliaclient --session <name> --sync -i
```

This can be e.g. set up in your Zellij/Tmux layout. With Zellij, use the tab name:

```sh
juliaclient --session "$(zellij action current-tab-info | head -1 | cut -c7-)" --sync -i
```

## Installation

Require the plugin from your Helix `init.scm` (`~/.config/helix/init.scm`):

```scheme
(require "/path/to/julia-steel/julia-remoterepl.scm")
```

## Usage

Two commands are available:

| Command | Description |
|---|---|
| `send-to-julia-repl` | Send the current selection |
| `send-top-level-to-julia-repl` | Send the top-level form under the cursor (uses tree-sitter) |

Bind them in `~/.config/helix/config.toml`:

```toml
[keys.normal]
C-j = ":send-to-julia-repl"
C-S-j = ":send-top-level-to-julia-repl"

[keys.select]
C-j = ":send-to-julia-repl"
```

`send-to-julia-repl` sends whatever is selected. `send-top-level-to-julia-repl` walks the tree-sitter parse tree upward from the cursor until it reaches the top-level node (e.g. a full function definition or `begin` block), then sends that — no manual selection required.

Output from the Julia session is shown in a vsplit buffer.

On failure the status bar shows an error and copies a startup command to the clipboard.

## Session resolution

The plugin resolves the session name using the following cascade:

1. **`.juliasession` file** — if a `.juliasession` file exists in the project root, its first line is used as the session name. Useful for projects that always connect to a named session regardless of environment.
2. **Zellij tab name** — if running inside Zellij, the current tab name is used. Name your tabs meaningfully and start Julia with the matching name.
3. **Working directory** — fallback to the CWD of the Helix process.
