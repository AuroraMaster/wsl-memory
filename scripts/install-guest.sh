#!/usr/bin/env sh
set -eu

RELEASE_BASE="${WSL_MEMORY_RELEASE_BASE:-https://github.com/wsl-memory-agent/wsl-memory-agent/releases/latest/download}"
GUEST_URL="${WSL_MEMORY_GUEST_URL:-$RELEASE_BASE/wsl-memory-guest}"
INSTALL_PATH="${WSL_MEMORY_GUEST_INSTALL_PATH:-/usr/local/bin/wsl-memory-guest}"
CONFIG_DIR="${WSL_MEMORY_CONFIG_DIR:-/usr/local/etc/wsl-memory-agent}"
CONFIG_FILE="$CONFIG_DIR/config.yaml"
SERVICE_FILE="/etc/systemd/system/wsl-memory-guest.service"
TOKEN_PATH="${WSL_MEMORY_TOKEN_PATH:-/mnt/c/Users/Public/wsl_agent_token}"

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root, for example: curl -fsSL <url> | sudo sh" >&2
    exit 1
fi

if ! command -v systemctl >/dev/null 2>&1; then
    echo "systemd is required. Enable systemd in WSL before installing." >&2
    exit 1
fi

mkdir -p "$(dirname "$INSTALL_PATH")" "$CONFIG_DIR"

tmp="$(mktemp)"
cleanup() {
    rm -f "$tmp"
}
trap cleanup EXIT

if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$GUEST_URL" -o "$tmp"
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$tmp" "$GUEST_URL"
else
    echo "curl or wget is required." >&2
    exit 1
fi

install -m 0755 "$tmp" "$INSTALL_PATH"

if [ ! -f "$TOKEN_PATH" ]; then
    echo "Warning: token file not found at $TOKEN_PATH" >&2
    echo "Run the Windows host installer first, or set WSL_MEMORY_TOKEN_PATH." >&2
fi

cat > "$CONFIG_FILE" <<EOF
host: auto:multi
token_path: $TOKEN_PATH
interval: 4
allow_drop: false
multi_path: true
tcp: false
EOF

cat > "$SERVICE_FILE" <<EOF
[Unit]
Description=WSL Memory Guest Agent
Documentation=https://github.com/microsoft/WSL/issues/4166
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$INSTALL_PATH --config $CONFIG_FILE
Restart=always
RestartSec=5
User=root
StandardOutput=journal
StandardError=journal
SyslogIdentifier=wsl-memory-guest
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/sys/fs/cgroup /proc/sys/vm/compact_memory /proc/sys/vm/drop_caches

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now wsl-memory-guest

echo "WSL Memory Guest is installed and running."
echo "Status: sudo systemctl status wsl-memory-guest --no-pager"
