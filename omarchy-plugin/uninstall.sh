#!/usr/bin/env bash
# Uninstaller for the Omarchy Gemini ADC Router Plugin (arch.gemini-router)
set -euo pipefail

PLUGIN_ID="arch.gemini-router"
PLUGIN_DEST="${HOME}/.config/omarchy/plugins/${PLUGIN_ID}"
BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
SHELL_SETTINGS="${HOME}/.config/omarchy/shell.json"

echo "==> Uninstalling Omarchy plugin: ${PLUGIN_ID}"

if [[ -x "${BIN_DIR}/omarchy-gemini-router-ctl" ]]; then
  "${BIN_DIR}/omarchy-gemini-router-ctl" disable --target both >/dev/null 2>&1 || true
  "${BIN_DIR}/omarchy-gemini-router-ctl" stop-router >/dev/null 2>&1 || true
fi

if command -v systemctl >/dev/null 2>&1; then
  systemctl --user disable --now claude-gemini-router.service >/dev/null 2>&1 || true
  rm -f "${SYSTEMD_USER_DIR}/claude-gemini-router.service"
  systemctl --user daemon-reload >/dev/null 2>&1 || true
fi

if [[ -f "${SHELL_SETTINGS}" ]]; then
  cp -f "${SHELL_SETTINGS}" "${SHELL_SETTINGS}.bak"
  python3 - "${SHELL_SETTINGS}" "${PLUGIN_ID}" <<'PY'
import json
import sys
from pathlib import Path

settings_path = Path(sys.argv[1])
plugin_id = sys.argv[2]

data = json.loads(settings_path.read_text(encoding="utf-8"))
right = data.get("bar", {}).get("layout", {}).get("right", [])
filtered = [w for w in right if not (isinstance(w, dict) and w.get("id") == plugin_id)]
if len(filtered) != len(right):
    data["bar"]["layout"]["right"] = filtered
    settings_path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    print(f"Removed {plugin_id} from {settings_path}")
PY
fi

rm -rf "${PLUGIN_DEST}"
rm -f "${BIN_DIR}/omarchy-gemini-router-ctl" "${BIN_DIR}/claude-gemini-router"

if command -v omarchy-shell >/dev/null 2>&1; then
  omarchy-shell shell reloadConfig >/dev/null 2>&1 || true
fi

echo "==> Uninstalled ${PLUGIN_ID}."
