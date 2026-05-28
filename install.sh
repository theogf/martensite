#!/usr/bin/env bash
# Install julia-conductor and juliaclient (Rust build).
# Mirrors what DaemonicCabal.install() does for the Zig binaries.
#
# Usage:
#   ./install.sh            # install
#   ./install.sh uninstall  # remove everything

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- Paths ---

XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
XDG_BIN_HOME="$HOME/.local/bin"

INSTALL_DIR="$XDG_DATA_HOME/julia-daemon"
WORKER_PROJECT_DST="$INSTALL_DIR/worker"
CONDUCTOR_DST="$INSTALL_DIR/julia-conductor"
CLIENT_DST="$INSTALL_DIR/juliaclient"
CLIENT_SYMLINK="$XDG_BIN_HOME/juliaclient"

SERVICE_NAME="julia-daemon"
SERVICE_FILE="$XDG_CONFIG_HOME/systemd/user/$SERVICE_NAME.service"

# Source of the DaemonWorker Julia project — adjust if your DaemonicCabal checkout differs
WORKER_PROJECT_SRC="${JULIA_DAEMONICABAL_DIR:-$HOME/.julia/dev/DaemonicCabal}/worker"

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

[[ -f "$CONDUCTOR_BIN" ]] || { echo "ERROR: $CONDUCTOR_BIN not found"; exit 1; }
[[ -f "$CLIENT_BIN" ]]    || { echo "ERROR: $CLIENT_BIN not found"; exit 1; }

# --- Install files ---

echo "Installing to $INSTALL_DIR"
rm -rf "$INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

# Worker project (read-only copy)
if [[ -d "$WORKER_PROJECT_SRC" ]]; then
    cp -r "$WORKER_PROJECT_SRC" "$WORKER_PROJECT_DST"
    chmod -R a-w "$WORKER_PROJECT_DST"
else
    echo "WARNING: Worker project not found at $WORKER_PROJECT_SRC"
    echo "  Set JULIA_DAEMONICABAL_DIR to your DaemonicCabal checkout."
    echo "  You must set JULIA_DAEMON_WORKER_PROJECT manually in the service."
fi

cp "$CONDUCTOR_BIN" "$CONDUCTOR_DST"
cp "$CLIENT_BIN"    "$CLIENT_DST"
chmod 755 "$CONDUCTOR_DST" "$CLIENT_DST"

# --- Systemd service ---

JULIA_BIN="${JULIA_DAEMON_WORKER_EXECUTABLE:-$(command -v julia || echo julia)}"
WORKER_PROJECT="${JULIA_DAEMON_WORKER_PROJECT:-$WORKER_PROJECT_DST}"

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
Environment="JULIA_DAEMON_WORKER_TTL=${JULIA_DAEMON_WORKER_TTL:-7200}"
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

echo ""
echo "Done. Make sure $XDG_BIN_HOME is on your PATH."
echo ""
echo "Daemon management:"
echo "  systemctl --user {start|stop|restart|status} $SERVICE_NAME"
