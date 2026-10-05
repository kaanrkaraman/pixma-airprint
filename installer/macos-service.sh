#!/bin/bash
# Run the AirPrint + AirScan bridge as a per-user background service (LaunchAgent).
# No sudo needed; starts at login and restarts if it exits.
#
#   installer/macos-service.sh install [printer-ip-or-hostname]   # empty = auto-discover
#   installer/macos-service.sh uninstall
#
# Optional: PIXMA_NAME="Canon G3416" installer/macos-service.sh install ...
set -euo pipefail

LABEL=com.pixma-airprint.bridge
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
BIN_DIR="$HOME/Library/Application Support/pixma-airprint"
LOG="$HOME/Library/Logs/pixma-airprint.log"

case "${1:-}" in
install)
    cd "$(dirname "$0")/.."
    cargo build --release -p pixma-bridge
    mkdir -p "$BIN_DIR" "$(dirname "$PLIST")"
    cp target/release/pixma-bridge "$BIN_DIR/"

    env_xml=""
    [ -n "${2:-}" ] && env_xml+="<key>PIXMA_PRINTER</key><string>$2</string>"
    [ -n "${PIXMA_NAME:-}" ] && env_xml+="<key>PIXMA_NAME</key><string>$PIXMA_NAME</string>"

    cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array><string>$BIN_DIR/pixma-bridge</string></array>
  <key>EnvironmentVariables</key><dict>$env_xml</dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
EOF

    launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$UID" "$PLIST"
    echo "Installed and running. Log: $LOG"
    ;;
uninstall)
    launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
    rm -f "$PLIST"
    rm -rf "$BIN_DIR"
    echo "Removed. (Log kept at $LOG)"
    ;;
*)
    echo "usage: $0 install [printer-ip-or-hostname] | uninstall" >&2
    exit 1
    ;;
esac
