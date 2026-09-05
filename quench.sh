#!/usr/bin/env bash
# Start juliaclient with the session name resolved via the same cascade as the Helix plugin:
#   1. .juliasession file in CWD
#   2. Zellij tab name
#   3. CWD

if [[ -f ".juliasession" ]]; then
    session=$(head -1 .juliasession)
elif [[ -n "$ZELLIJ" ]]; then
    session=$(zellij action current-tab-info | head -1 | cut -c7-)
elif [[ -n "$TMUX" ]]; then
    session=$(tmux display-message -p '#W')
else
    session=$(pwd)
fi


# --sync is required for this session to register as a SyncSession on the
# worker at all (worker/src/setup.jl: sync_session_label bails to nothing
# without it) — without it, `temper` calls targeting this same --session
# label create their own orphan session with no attached listeners, so their
# echoed input/result never reaches this terminal.
exec juliaclient --session="$session" --sync -i "$@"
