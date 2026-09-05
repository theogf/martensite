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

### 3. A Julia session named from context

Both sides derive the session name from the same context, so they find each
other without either being told. The plugin does its half itself; give your
shell the matching half once, in your own config.

`quench` starts a plain Julia REPL that serves *itself* as a jld session — no
separate daemon process, no second prompt, and its state dies with the terminal:

```fish
# ~/.config/fish/functions/quench.fish
function quench --description "Start a Julia REPL that serves itself as a jld session"
    # Session name. Every source is an env var or a file — never a probe of the
    # terminal multiplexer. `zellij action current-tab-info` was tried and
    # removed: the Helix plugin resolves the name independently, so one side
    # could succeed while the other failed, silently producing two different
    # daemon ids. (It also needs ZELLIJ_SESSION_NAME and, without it, prints a
    # session-picker message to stdout instead of failing.) jld keys the daemon
    # on the project anyway, so the name only disambiguates several REPLs on one
    # project — use .juliasession or JLD_NAME for that.
    set -l session
    if test -n "$MARTENSITE_SESSION"
        set session $MARTENSITE_SESSION
    else if test -n "$JLD_NAME"
        set session $JLD_NAME
    else if test -f .juliasession
        set session (head -1 .juliasession | string trim)
    else
        set session repl
    end

    # jld hashes the name into the daemon id but sanitizes it in only one of the
    # two code paths (serve_session does, make_ctx does not), so do it up front
    # and both sides hash the same string.
    set session (string replace -ra '[^A-Za-z0-9_.-]' '-' -- $session)

    # A branch above can yield an empty LIST rather than an empty string (a blank
    # .juliasession). In fish that makes `JLD_NAME=$session` vanish from the env
    # call entirely instead of setting an empty value, leaving JLD_NAME unset.
    if test -z "$session"
        set session repl
    end

    # JULIA_LOAD_PATH: the apps env makes JuliaDaemon importable without adding
    # it to any of your environments (Pkg.app add installs it off the default
    # load path).
    #
    # --project=@.: mandatory. jld walks up to the nearest Project.toml; plain
    # julia does not and would default to @v#.#, registering the session under
    # the wrong project so the plugin never finds it.
    #
    # JLD_NAME is exported so any `jld` run from inside this REPL targets this
    # same session.
    env JULIA_LOAD_PATH="@:@v#.#:@stdlib:$HOME/.julia/environments/apps/JuliaDaemon" \
        JLD_NAME=$session \
        julia --project=@. $argv -i \
        -e 'using JuliaDaemon; JuliaDaemon.serve(name = get(ENV, "JLD_NAME", "repl"))'
end
```

<details>
<summary>bash / zsh</summary>

```bash
quench() {
    # Every source is an env var or a file — never a probe of the multiplexer.
    # See the note under "Session resolution" for why.
    local session
    if   [[ -n "$MARTENSITE_SESSION" ]]; then session="$MARTENSITE_SESSION"
    elif [[ -n "$JLD_NAME" ]];           then session="$JLD_NAME"
    elif [[ -f .juliasession ]];         then session=$(head -1 .juliasession)
    else session=repl
    fi
    # jld sanitizes the name in only one of the two places it derives an id.
    session=$(printf '%s' "$session" | tr -c 'A-Za-z0-9_.-' '-')
    [[ -z "$session" ]] && session=repl

    JULIA_LOAD_PATH="@:@v#.#:@stdlib:$HOME/.julia/environments/apps/JuliaDaemon" \
    JLD_NAME="$session" \
    julia --project=@. "$@" -i \
        -e 'using JuliaDaemon; JuliaDaemon.serve(name = get(ENV, "JLD_NAME", "repl"))'
}
```
</details>

Make `quench` your Julia pane's command in your Zellij/Tmux layout. It prints
the session id and the commands agents can use against it. `jld list` shows it
as `idle/repl`, which marks it a human's live REPL.

Setting `JLD_NAME` also means any `jld` you run *from inside* that pane targets
the same session automatically. If you set `JLD_NAME` per-pane in the layout
instead, the cascade short-circuits to it on both sides and you need no wrapper
at all.

<details>
<summary>Alternative: <code>jld connect</code>, if you want state that outlives the terminal</summary>

Swap the last three lines for `jld connect --name=$session $argv`. `Main` then
lives in a daemon, so closing and reattaching **resumes the previous session's
state** rather than starting clean.

> [!WARNING]
> This form gives you two prompts, and sends follow whichever is active.
> Backspace at an empty `julia@<id>>` prompt drops you to a plain `julia>` —
> the connect script's *own* local Julia, with no project, no Revise and none
> of your packages. A `C-j` from Helix pasted there evaluates in that process
> instead of your session, silently and wrongly.
>
> **Press `>` at the empty `julia>` prompt to get back.** It is a mode key like
> `]`, `?` and `;`; the connect banner mentions how to leave but not how to
> return.
</details>

The `eval-*` commands work whether or not a REPL is running — with none, `jld`
starts a daemon on the first send. The `send-*` commands need one by definition:
with none, they report `jld`'s own error rather than quietly evaluating
somewhere you weren't looking.

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

The two pairs are not interchangeable, and neither falls back to the other —
pick the one that matches where you want the answer.

The `send-*` pair is a real paste into the prompt: bracketed-paste injection, so
the code is echoed, evaluated by the REPL itself, and sets `ans` — and any
half-typed input of yours is stashed and put back afterwards. Because the REPL
owns the evaluation, its output goes to your terminal, not back to Helix.

The `eval-*` pair is the opposite trade: `jld` evaluates and hands back exactly
what a REPL would show — streamed output plus the rendered value, with the usual
semantics (`nothing` and a trailing `;` print nothing) — which lands in a
floating popup (dismiss with any keypress). Your prompt is never touched, and
`ans` is not set.

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
