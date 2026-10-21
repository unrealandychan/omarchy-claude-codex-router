#!/usr/bin/env bash
# Installer for the Omarchy Gemini ADC Router Plugin (arch.gemini-router)
set -euo pipefail

PLUGIN_ID="arch.gemini-router"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

PLUGIN_DEST="${HOME}/.config/omarchy/plugins/${PLUGIN_ID}"
CONFIG_DIR="${HOME}/.config/omarchy/gemini-router"
BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
SHELL_SETTINGS="${HOME}/.config/omarchy/shell.json"

echo "==> Installing Omarchy plugin: ${PLUGIN_ID} (Rust edition)"

chmod +x "${SCRIPT_DIR}/install.sh" "${SCRIPT_DIR}/uninstall.sh" "${SCRIPT_DIR}/tests/test-ctl.sh" 2>/dev/null || true

mkdir -p "${PLUGIN_DEST}/plugin" "${PLUGIN_DEST}/bin"
mkdir -p "${CONFIG_DIR}" "${BIN_DIR}" "${SYSTEMD_USER_DIR}"

# Build or locate Rust release binaries
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${PROJECT_DIR}/target}"
if command -v cargo >/dev/null 2>&1; then
  echo "==> Building Rust release binaries in ${PROJECT_DIR}..."
  (cd "${PROJECT_DIR}" && cargo build --release --locked --offline --target-dir "${CARGO_TARGET_DIR}")
fi

cp -f "${CARGO_TARGET_DIR}/release/claude-gemini-router" "${BIN_DIR}/claude-gemini-router"
chmod 755 "${BIN_DIR}/claude-gemini-router"

cp -f "${CARGO_TARGET_DIR}/release/omarchy-gemini-router-ctl" "${SCRIPT_DIR}/bin/omarchy-gemini-router-ctl"
chmod 700 "${SCRIPT_DIR}/bin/omarchy-gemini-router-ctl"

cp -f "${SCRIPT_DIR}/manifest.json" "${PLUGIN_DEST}/manifest.json"
cp -f "${SCRIPT_DIR}/plugin/GeminiRouter.qml" "${PLUGIN_DEST}/plugin/GeminiRouter.qml"
cp -f "${SCRIPT_DIR}/bin/omarchy-gemini-router-ctl" "${PLUGIN_DEST}/bin/omarchy-gemini-router-ctl"
chmod 700 "${PLUGIN_DEST}/bin/omarchy-gemini-router-ctl"

cp -f "${SCRIPT_DIR}/bin/omarchy-gemini-router-ctl" "${BIN_DIR}/omarchy-gemini-router-ctl"
chmod 700 "${BIN_DIR}/omarchy-gemini-router-ctl"

if [[ -f "${PROJECT_DIR}/bin/claude-gemini" ]]; then
  cp -f "${PROJECT_DIR}/bin/claude-gemini" "${BIN_DIR}/claude-gemini"
  chmod 755 "${BIN_DIR}/claude-gemini"
fi

if [[ -f "${PROJECT_DIR}/bin/codex-gemini" ]]; then
  cp -f "${PROJECT_DIR}/bin/codex-gemini" "${BIN_DIR}/codex-gemini"
  chmod 755 "${BIN_DIR}/codex-gemini"
fi

if [[ ! -f "${CONFIG_DIR}/router.env" ]]; then
  cat > "${CONFIG_DIR}/router.env" <<EOF
GEMINI_PRIMARY_MODEL=gemini-3.8-flash
GEMINI_REASONING_EFFORT=medium
ROUTER_PORT=4000
EOF
  chmod 600 "${CONFIG_DIR}/router.env"
fi

# Install systemd user unit for the local Rust ADC router daemon
cat > "${SYSTEMD_USER_DIR}/claude-gemini-router.service" <<EOF
[Unit]
Description=Claude Code & OpenAI Codex -> Vertex AI Gemini ADC Router (Rust)
After=network-online.target

[Service]
Type=simple
WorkingDirectory=${PROJECT_DIR}
EnvironmentFile=-%h/.config/omarchy/gemini-router/router.env
ExecStart=${BIN_DIR}/claude-gemini-router --host 127.0.0.1 --port 4000
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF

if command -v systemctl >/dev/null 2>&1; then
  systemctl --user daemon-reload >/dev/null 2>&1 || true
  systemctl --user enable --now claude-gemini-router.service >/dev/null 2>&1 || true
  systemctl --user restart claude-gemini-router.service >/dev/null 2>&1 || true
fi

# Register widget in ~/.config/omarchy/shell.json
if [[ -f "${SHELL_SETTINGS}" ]]; then
  cp -f "${SHELL_SETTINGS}" "${SHELL_SETTINGS}.bak"
  python3 - "${SHELL_SETTINGS}" "${PLUGIN_ID}" <<'PY'
import json
import sys
from pathlib import Path

settings_path = Path(sys.argv[1])
plugin_id = sys.argv[2]

data = json.loads(settings_path.read_text(encoding="utf-8"))
bar = data.setdefault("bar", {})
layout = bar.setdefault("layout", {})
right = layout.setdefault("right", [])

if not any(isinstance(w, dict) and w.get("id") == plugin_id for w in right):
    entry = {"id": plugin_id}
    insert_idx = next(
        (i + 1 for i, w in enumerate(right) if isinstance(w, dict) and w.get("id") == "arch.backup-manager"),
        len(right),
    )
    right.insert(insert_idx, entry)
    settings_path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    print(f"Added {plugin_id} to bar.layout.right in {settings_path}")
else:
    print(f"{plugin_id} already registered in {settings_path}")
PY
fi

if command -v omarchy-shell >/dev/null 2>&1; then
  omarchy-shell shell reloadConfig >/dev/null 2>&1 || true
fi

echo "==> Successfully installed ${PLUGIN_ID}!"
echo "    CLI:    ${BIN_DIR}/omarchy-gemini-router-ctl status"
echo "    Plugin: ${PLUGIN_DEST}"
