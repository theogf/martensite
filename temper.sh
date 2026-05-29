#!/usr/bin/env bash
# Send code to the current Julia session (session resolved same as quench).
#
# Usage:
#   temper "expr"       evaluate expression
#   temper script.jl    run file
#   echo "expr" | temper  read from stdin

if [[ -f ".juliasession" ]]; then
    session=$(head -1 .juliasession)
elif [[ -n "$ZELLIJ" ]]; then
    session=$(zellij action current-tab-info | head -1 | cut -c7-)
elif [[ -n "$TMUX" ]]; then
    session=$(tmux display-message -p '#W')
else
    session=$(pwd)
fi

if [[ $# -eq 1 && -f "$1" ]]; then
    exec juliaclient --session="$session" --sync -- "$1"
elif [[ $# -ge 1 ]]; then
    exec juliaclient --session="$session" --sync --eval "$*"
else
    exec juliaclient --session="$session" --sync --eval "$(cat)"
fi
