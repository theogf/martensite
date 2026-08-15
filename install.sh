#!/usr/bin/env bash
# Install julia-conductor and juliaclient (Rust build).
# Mirrors what DaemonicCabal.install() does for the Zig binaries.
#
# Usage:
#   ./install.sh            # install
#   ./install.sh uninstall  # remove everything

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Local checkout of DaemonicCabal.jl — its worker/ subdirectory is a
# self-contained Julia package (DaemonWorker, stdlib deps only) that gets
# copied verbatim into the install dir, matching what DaemonicCabal.jl's own
# installer does (src/installers/common.jl: install_files). There is no
# registered "DaemonicCabal" package to Pkg.add — it must come from here.
DAEMONIC_CABAL_SRC="${DAEMONIC_CABAL_SRC:-$HOME/.julia/dev/DaemonicCabal}"

# --- Paths ---

XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
XDG_BIN_HOME="$HOME/.local/bin"

INSTALL_DIR="$XDG_DATA_HOME/julia-daemon"
CONDUCTOR_DST="$INSTALL_DIR/julia-conductor"
CLIENT_DST="$INSTALL_DIR/juliaclient"
CLIENT_SYMLINK="$XDG_BIN_HOME/juliaclient"
SESSION_SCRIPT_SRC="$SCRIPT_DIR/quench.sh"
SESSION_SCRIPT_DST="$XDG_BIN_HOME/quench"
TEMPER_SCRIPT_SRC="$SCRIPT_DIR/temper.sh"
TEMPER_SCRIPT_DST="$XDG_BIN_HOME/temper"

SERVICE_NAME="julia-daemon"
SERVICE_FILE="$XDG_CONFIG_HOME/systemd/user/$SERVICE_NAME.service"

# --- Uninstall ---

do_uninstall() {
    echo "Stopping and disabling service..."
    systemctl --user disable --now "$SERVICE_NAME" 2>/dev/null || true

    if [[ -f "$SERVICE_FILE" ]]; then
        echo "Removing $SERVICE_FILE"
        rm -f "$SERVICE_FILE"
        systemctl --user daemon-reload
    fi

    if [[ -L "$CLIENT_SYMLINK" || -f "$CLIENT_SYMLINK" ]]; then
        echo "Removing $CLIENT_SYMLINK"
        rm -f "$CLIENT_SYMLINK"
    fi

    if [[ -L "$SESSION_SCRIPT_DST" || -f "$SESSION_SCRIPT_DST" ]]; then
        echo "Removing $SESSION_SCRIPT_DST"
        rm -f "$SESSION_SCRIPT_DST"
    fi

    if [[ -L "$TEMPER_SCRIPT_DST" || -f "$TEMPER_SCRIPT_DST" ]]; then
        echo "Removing $TEMPER_SCRIPT_DST"
        rm -f "$TEMPER_SCRIPT_DST"
    fi

    if [[ -d "$INSTALL_DIR" ]]; then
        echo "Removing $INSTALL_DIR"
        rm -rf "$INSTALL_DIR"
    fi

    echo "Uninstall done."
}

if [[ "${1:-}" == "uninstall" ]]; then
    do_uninstall
    exit 0
fi

# --- Build ---

echo "Building release binaries..."
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"

CONDUCTOR_BIN="$SCRIPT_DIR/target/release/julia-conductor"
CLIENT_BIN="$SCRIPT_DIR/target/release/juliaclient"
WORKER_SRC="$DAEMONIC_CABAL_SRC/worker"

[[ -f "$CONDUCTOR_BIN" ]] || { echo "ERROR: $CONDUCTOR_BIN not found"; exit 1; }
[[ -f "$CLIENT_BIN" ]]    || { echo "ERROR: $CLIENT_BIN not found"; exit 1; }
[[ -f "$WORKER_SRC/Project.toml" ]] || {
    echo "ERROR: $WORKER_SRC/Project.toml not found."
    echo "Set DAEMONIC_CABAL_SRC to your DaemonicCabal.jl checkout (expected a worker/ subdirectory)."
    exit 1
}

# --- Install files ---
# Validated above before touching anything, so a missing/misconfigured
# DaemonicCabal checkout fails loudly instead of wiping a working install.

echo "Installing to $INSTALL_DIR"
chmod -R u+w "$INSTALL_DIR" 2>/dev/null || true
rm -rf "$INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

echo "Copying worker package from $WORKER_SRC..."
cp -r "$WORKER_SRC" "$INSTALL_DIR/worker"

cp "$CONDUCTOR_BIN" "$CONDUCTOR_DST"
cp "$CLIENT_BIN"    "$CLIENT_DST"
chmod 755 "$CONDUCTOR_DST" "$CLIENT_DST"

# --- Systemd service ---

JULIA_BIN="${JULIA_DAEMON_WORKER_EXECUTABLE:-$(command -v julia || echo julia)}"
WORKER_PROJECT="${JULIA_DAEMON_WORKER_PROJECT:-$INSTALL_DIR/worker}"

mkdir -p "$(dirname "$SERVICE_FILE")"

cat > "$SERVICE_FILE" <<EOF
[Unit]
Description=Julia (DaemonicCabal) daemon conductor service

[Service]
Type=simple
ExecStart=$CONDUCTOR_DST
Environment="JULIA_DAEMON_WORKER_EXECUTABLE=$JULIA_BIN"
Environment="JULIA_DAEMON_WORKER_PROJECT=$WORKER_PROJECT"
Environment="JULIA_DAEMON_WORKER_MAXCLIENTS=${JULIA_DAEMON_WORKER_MAXCLIENTS:-1}"
Environment="JULIA_DAEMON_WORKER_ARGS=${JULIA_DAEMON_WORKER_ARGS:---startup-file=no}"
Environment="JULIA_DAEMON_MIN_TTL=${JULIA_DAEMON_MIN_TTL:-120}"
Environment="JULIA_DAEMON_MAX_TTL=${JULIA_DAEMON_MAX_TTL:-${JULIA_DAEMON_WORKER_TTL:-7200}}"
Restart=on-failure

[Install]
WantedBy=default.target
EOF

echo "Installing systemd service: $SERVICE_FILE"
systemctl --user daemon-reload
systemctl --user enable --now "$SERVICE_NAME"

# --- Client symlink ---

mkdir -p "$XDG_BIN_HOME"
ln -sf "$CLIENT_DST" "$CLIENT_SYMLINK"
echo "Symlinked juliaclient → $CLIENT_SYMLINK"

cp "$SESSION_SCRIPT_SRC" "$SESSION_SCRIPT_DST"
chmod 755 "$SESSION_SCRIPT_DST"
echo "Installed quench → $SESSION_SCRIPT_DST"

cp "$TEMPER_SCRIPT_SRC" "$TEMPER_SCRIPT_DST"
chmod 755 "$TEMPER_SCRIPT_DST"
echo "Installed temper → $TEMPER_SCRIPT_DST"

echo ""
echo "Done. Make sure $XDG_BIN_HOME is on your PATH."
echo ""
echo "Daemon management:"
echo "  systemctl --user {start|stop|restart|status} $SERVICE_NAME"
