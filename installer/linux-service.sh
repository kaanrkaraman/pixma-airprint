#!/bin/bash
# Run the AirPrint + AirScan bridge as a systemd service (Ubuntu/Debian, needs sudo).
# Starts at boot, restarts if it exits, and is restarted with Avahi so the Bonjour
# adverts never go stale.
#
#   installer/linux-service.sh install [printer-ip-or-hostname]   # empty = auto-discover
#   installer/linux-service.sh uninstall
#
# Optional: PIXMA_NAME="Canon G3416" installer/linux-service.sh install ...
set -euo pipefail

UNIT=pixma-airprint
UNIT_FILE="/etc/systemd/system/$UNIT.service"
BIN=/usr/local/bin/pixma-bridge

case "${1:-}" in
install)
    cd "$(dirname "$0")/.."
    cargo build --release -p pixma-bridge
    command -v avahi-publish >/dev/null || sudo apt-get install -y avahi-daemon avahi-utils
    sudo install -m 755 target/release/pixma-bridge "$BIN"

    env_lines=""
    [ -n "${2:-}" ] && env_lines+="Environment=PIXMA_PRINTER=$2"$'\n'
    [ -n "${PIXMA_NAME:-}" ] && env_lines+="Environment=PIXMA_NAME=$PIXMA_NAME"$'\n'

    sudo tee "$UNIT_FILE" >/dev/null <<EOF
[Unit]
Description=Canon PIXMA AirPrint + AirScan bridge
After=network-online.target avahi-daemon.service
Wants=network-online.target
BindsTo=avahi-daemon.service

[Service]
User=$USER
${env_lines}ExecStart=$BIN
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

    sudo systemctl daemon-reload
    sudo systemctl enable --now "$UNIT"
    echo "Installed and running. Log: journalctl -u $UNIT -f"
    ;;
uninstall)
    sudo systemctl disable --now "$UNIT" 2>/dev/null || true
    sudo rm -f "$UNIT_FILE" "$BIN"
    sudo systemctl daemon-reload
    echo "Removed."
    ;;
*)
    echo "usage: $0 install [printer-ip-or-hostname] | uninstall" >&2
    exit 1
    ;;
esac
