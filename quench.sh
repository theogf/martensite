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

exec juliaclient --session="$session" -i "$@"
