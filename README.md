# julia-steel

A [Steel](https://github.com/mattwparas/steel) plugin for [Helix](https://helix-editor.com) that sends the current selection to a running [DaemonicCabal.jl](https://github.com/tecosaur/DaemonicCabal.jl) session.

## Prerequisites

### 1. Helix — steel branch

Follow the instructions [here](https://github.com/mattwparas/helix/blob/steel-event-system/STEEL.md) to install the correct version of Helix with the plugin system.

Point `HELIX_RUNTIME` at the runtime directory if Helix can't find its queries:

```sh
export HELIX_RUNTIME=/path/to/helix/runtime
```

### 2. DaemonicCabal.jl

Install DaemonicCabal into your global Julia environment so `juliaclient` is available system-wide:

```sh
julia --startup-file=no -e 'using Pkg; Pkg.dev(url="https://github.com/tecosaur/DaemonicCabal.jl")'
```

Make sure `juliaclient` ends up on your `PATH` (check `DaemonicCabal`'s README for the exact build/install step for the Zig client binary).

### 3. Starting a Julia session

Launch Julia through `juliaclient` so it runs inside a DaemonicCabal-managed worker for your project:

```sh
juliaclient --session $(pwd) --sync -i
```

This can be e.g. set up in your Zellij/Tmux layout.

## Installation

Require the plugin from your Helix `init.scm` (`~/.config/helix/init.scm`):

```scheme
(require "/path/to/julia-steel/julia-remoterepl.scm")
```

## Usage

Call `send-to-julia-repl` from a keybinding of your choice. For example, add to `~/.config/helix/config.toml`:

```toml
[keys.normal]
A-ret = ":send-to-julia-repl"

[keys.select]
A-ret = ":send-to-julia-repl"
```

Select any Julia expression in Helix and press the bound key. The selection is sent to the DaemonicCabal worker whose session name matches the current working directory.

On failure the status bar shows an error and copies a startup command to the clipboard.
