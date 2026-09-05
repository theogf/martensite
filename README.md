# martensite

[![Docs](https://img.shields.io/badge/docs-architecture-blue?style=flat-square)](https://theogf.dev/martensite/architecture.html)

> *Martensite* is the hardest phase of steel, formed by rapidly quenching austenite — a metaphor for JIT-compiled Julia hardened through the Steel scripting layer.

A [Steel](https://github.com/mattwparas/steel) plugin for [Helix](https://helix-editor.com) that sends Julia code to a live REPL served by [JuliaDaemon.jl](https://github.com/KristofferC/JuliaDaemon.jl).

The whole plugin is one file, `martensite.scm`. There is no daemon to install, no
systemd unit, no binary to build, and no install script to run.

## Setup

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

### 3. martensite

The repo is a Steel package (`cog.scm`), so it goes on the cog path and is
required by name:

```sh
forge pkg install --git https://github.com/theogf/martensite
```

```scheme
;; ~/.config/helix/init.scm
(require "martensite/martensite.scm")
```

That is the whole installation — one file of Steel, no daemon, no service, no
build step.

<details>
<summary>Installing a checkout instead, to hack on the plugin</summary>

`forge` clones a snapshot into the cogs directory, so edits to a checkout
elsewhere would not be picked up. Symlink it instead:

```sh
ln -s /path/to/martensite ~/.local/share/steel/cogs/martensite
```

The require stays the same, and the loaded plugin then follows your working
tree — including whichever branch is checked out.

Requiring the file by absolute path also still works and needs no install at
all: `(require "/path/to/martensite/martensite.scm")`.
</details>

### 4. A Julia session named from context

The package ships `quench`: a dependency-free POSIX `sh` script that starts a
plain Julia REPL which serves *itself* as a jld session. One process, one prompt,
and its state dies with the terminal.

It lives in the cogs directory — the same path whether you installed with `forge`
or symlinked a checkout:

```
~/.local/share/steel/cogs/martensite/quench
```

Naming it as a pane command directly is why it is a script rather than a shell
function: a layout execs the command instead of going through a shell. That also
means the layout gets no shell expansion, so put it on your `PATH` and refer to
it by name:

```sh
ln -s ~/.local/share/steel/cogs/martensite/quench ~/.local/bin/quench
```

```kdl
pane {
    command "quench"
    name "Julia"
}
```

Spelling out the full absolute path in `command` works too. A leading `~` may
not — a layout is not a shell, and I have not verified that Zellij expands it
there.

Run it by hand the same way, passing any extra Julia flags straight through:

```sh
quench --threads=auto
```

<details>
<summary>Alternative: <code>jld connect</code>, if you want state that outlives the terminal</summary>

`jld connect --name=<session>` puts `Main` in a daemon instead, so closing and
reattaching **resumes the previous session's state**.

> [!WARNING]
> That form gives you two prompts, and sends follow whichever is active.
> Backspace at an empty `julia@<id>>` prompt drops you to a plain `julia>` —
> the connect script's *own* local Julia, with no project, no Revise and none
> of your packages. A send pasted there evaluates in that process instead of
> your session, silently and wrongly.
>
> **Press `>` at the empty `julia>` prompt to get back.** It is a mode key like
> `]`, `?` and `;`; the connect banner mentions how to leave but not how to
> return.
</details>

The `eval-*` commands work whether or not a REPL is running — with none, `jld`
starts a daemon on the first send. The `send-*` commands need one by definition:
with none, they report `jld`'s own error rather than quietly evaluating somewhere
you weren't looking.

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

The two pairs are not interchangeable, and neither falls back to the other —
pick the one that matches where you want the answer.

The `send-*` pair is a real paste into the prompt: bracketed-paste injection, so
the code is echoed, evaluated by the REPL itself, and sets `ans` — and any
half-typed input of yours is stashed and put back afterwards. Because the REPL
owns the evaluation, its output goes to your terminal, not back to Helix.

The `eval-*` pair is the opposite trade: `jld` evaluates and hands back exactly
what a REPL would show — streamed output plus the rendered value, with the usual
semantics (`nothing` and a trailing `;` print nothing). Your prompt is never
touched, and `ans` is not set.

Where the answer lands depends on its size:

| result | shown as |
|---|---|
| nothing printed | a status-line message |
| one short line (`42`) | the status line, `julia: 42` |
| anything longer, or an error | a floating popup, sized to the content |

The popup is bordered, anchored below the cursor, and dismissed with any
keypress. Its frame carries the signal the text can't: an error draws the
border in your theme's error colour and titles it ` error `, and when the output
is taller than the box a ` ⋯ +N more ` badge on the bottom border says how much
was cut (`jld trace` in your REPL gives the full backtrace).

## Session resolution

`jld` keys a daemon on the *project* — the nearest `Project.toml` walking up
from Helix's working directory — so the session name only has to disambiguate
several sessions on one project. The plugin resolves it as:

1. **`MARTENSITE_SESSION`** environment variable, if set.
2. **`JLD_NAME`** — `jld`'s own override, honored here too so that one variable
   set per-pane in a layout names the session on both sides at once.
3. **`.juliasession` file** — its first line, if the file exists in the project root.
4. **`repl`** — the default, which is also what `JuliaDaemon.serve()` picks with
   no arguments.

Most of the time you need none of these: one REPL per project resolves to `repl`
on both sides and just works. Reach for `.juliasession` (or `JLD_NAME`) only to
run *several* REPLs against one project.

> [!NOTE]
> Earlier versions also probed the Zellij tab name and the tmux window name.
> That was removed. The plugin and the REPL resolve the name independently, so a
> probe that works on one side and fails on the other produces two different
> daemon ids with nothing to show for it — which is exactly how it broke in
> practice. `zellij action` also requires `ZELLIJ_SESSION_NAME` and, without it,
> prints a session-picker message to stdout instead of failing, so the caller
> parses that as a tab name. Every source above is an environment variable or a
> file, and cannot half-work.

## Agents

Agents share the session without touching your prompt. A `quench` session shows
up in `jld list` with state **`idle/repl`**, which is how an agent recognises a
human's live REPL rather than a daemon it may restart or stop. Given its id:

```sh
jld --id=<id> eval '<code>'      # captured output; your prompt is untouched
jld --id=<id> eval --scratch ... # throwaway module that can't clobber Main's bindings
jld --id=<id> transcript         # read what you have been doing
```

`jld install` drops an agent skill into `~/.agents/skills` (and the skills
directories of installed agents — `~/.claude`, `~/.codex`) documenting exactly
this: spot `idle/repl`, read `jld transcript` first for context, eval into the
session, show results with `jld eval-repl`, and never `jld kill` a human's REPL
(`jld stop` refuses it automatically). So agents do not need to be told any of
this per-project — it travels with `jld`, not with martensite.
