#!/usr/bin/env bash
# Automated test suite for omarchy-gemini-router-ctl and arch.gemini-router plugin
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

if [[ -n "${CTL_BIN:-}" ]]; then
  CTL="${CTL_BIN}"
elif [[ -x "${PLUGIN_DIR}/target/release/omarchy-gemini-router-ctl" ]]; then
  CTL="${PLUGIN_DIR}/target/release/omarchy-gemini-router-ctl"
elif [[ -x "${HOME}/.local/bin/omarchy-gemini-router-ctl" ]]; then
  CTL="${HOME}/.local/bin/omarchy-gemini-router-ctl"
else
  (cd "${PLUGIN_DIR}" && cargo build --release --locked --bin omarchy-gemini-router-ctl)
  CTL="${PLUGIN_DIR}/target/release/omarchy-gemini-router-ctl"
fi

TMP_ROOT="$(mktemp -d)"
trap 'rm -rf "${TMP_ROOT}"' EXIT

export OMARCHY_GEMINI_ROUTER_CONFIG_DIR="${TMP_ROOT}/config"
export CLAUDE_SETTINGS_PATH="${TMP_ROOT}/claude/settings.json"
export CODEX_CONFIG_PATH="${TMP_ROOT}/codex/config.toml"
export OMARCHY_MENU_CONFIG_PATH="${TMP_ROOT}/omarchy/extensions/omarchy-menu.jsonc"
export OMARCHY_GEMINI_ROUTER_MOCK_DAEMON=1
export OMARCHY_GEMINI_ROUTER_NO_NOTIFY=1
export ROUTER_PORT=49999
export ROUTER_URL="http://127.0.0.1:49999"

PASS_COUNT=0
FAIL_COUNT=0

assert_eq() {
  local actual="$1"
  local expected="$2"
  local msg="$3"
  if [[ "${actual}" == "${expected}" ]]; then
    echo "  [PASS] ${msg}"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "  [FAIL] ${msg}: expected '${expected}', got '${actual}'" >&2
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

assert_contains() {
  local haystack="$1"
  local needle="$2"
  local msg="$3"
  if [[ "${haystack}" == *"${needle}"* ]]; then
    echo "  [PASS] ${msg}"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "  [FAIL] ${msg}: expected to contain '${needle}'" >&2
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

json_field() {
  local json_str="$1"
  local expr="$2"
  python3 -c "import json, sys; data = json.loads(sys.stdin.read()); print(${expr})" <<<"${json_str}"
}

echo "==> 1. Seed realistic pre-existing Claude & Codex configs"
mkdir -p "$(dirname "${CLAUDE_SETTINGS_PATH}")" "$(dirname "${CODEX_CONFIG_PATH}")" "$(dirname "${OMARCHY_MENU_CONFIG_PATH}")"

cat > "${CLAUDE_SETTINGS_PATH}" <<'EOF'
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/home/user/.config/herdr/hooks/herdr-agent-state.sh"
          }
        ]
      }
    ]
  },
  "theme": "custom:omarchy",
  "env": {
    "CLAUDE_CODE_USE_VERTEX": "1",
    "ANTHROPIC_VERTEX_PROJECT_ID": "personal-testing-502307",
    "CLOUD_ML_REGION": "global",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-5-5"
  }
}
EOF
chmod 600 "${CLAUDE_SETTINGS_PATH}"

cat > "${CODEX_CONFIG_PATH}" <<'EOF'
[features]
hooks = true

[tui]
screen_reader_detection_done = true
EOF
chmod 600 "${CODEX_CONFIG_PATH}"

cat > "${OMARCHY_MENU_CONFIG_PATH}" <<'EOF'
{
  // Existing user menu extension
  "personal": {"icon":"","label":"Personal"}
}
EOF
chmod 600 "${OMARCHY_MENU_CONFIG_PATH}"

echo "==> 2. Verify initial status (routing off)"
STATUS_JSON="$("${CTL}" status)"
assert_eq "$(json_field "${STATUS_JSON}" "data['ok']")" "True" "Initial status ok=True"
assert_eq "$(json_field "${STATUS_JSON}" "data['claude_enabled']")" "False" "Claude initially disabled"
assert_eq "$(json_field "${STATUS_JSON}" "data['codex_enabled']")" "False" "Codex initially disabled"
assert_eq "$(json_field "${STATUS_JSON}" "data['menu_enabled']")" "False" "Omarchy menu initially disabled"
assert_eq "$(json_field "${STATUS_JSON}" "data['badge_text']")" "ADC Off" "Badge text is ADC Off"

echo "==> 3. Enable Claude routing only"
RES_CLAUDE="$("${CTL}" enable --target claude)"
assert_eq "$(json_field "${RES_CLAUDE}" "data['claude_enabled']")" "True" "Claude routing enabled"
assert_eq "$(json_field "${RES_CLAUDE}" "data['codex_enabled']")" "False" "Codex routing still disabled"
assert_eq "$(json_field "${RES_CLAUDE}" "data['badge_text']")" "Claude -> G3.8" "Badge shows Claude -> G3.8"

CLAUDE_FILE_CONTENT="$(cat "${CLAUDE_SETTINGS_PATH}")"
assert_eq "$(json_field "${CLAUDE_FILE_CONTENT}" "data['env']['ANTHROPIC_BASE_URL']")" "http://127.0.0.1:49999" "Claude ANTHROPIC_BASE_URL set"
assert_eq "$(json_field "${CLAUDE_FILE_CONTENT}" "data['env']['ANTHROPIC_AUTH_TOKEN']")" "adc-keyless" "Claude ANTHROPIC_AUTH_TOKEN set"
assert_eq "$(json_field "${CLAUDE_FILE_CONTENT}" "data['env']['ANTHROPIC_SMALL_FAST_MODEL']")" "claude-haiku-4-5" "Claude fast/background model uses claude-haiku-4-5"
assert_eq "$(json_field "${CLAUDE_FILE_CONTENT}" "data['env']['ANTHROPIC_DEFAULT_HAIKU_MODEL']")" "claude-haiku-4-5" "Claude Haiku tier uses claude-haiku-4-5"
assert_eq "$(json_field "${CLAUDE_FILE_CONTENT}" "data['env']['ANTHROPIC_DEFAULT_OPUS_MODEL']")" "gemini-3.8-flash" "Claude Opus tier tracks primary_model (gemini-3.8-flash)"
assert_eq "$(json_field "${CLAUDE_FILE_CONTENT}" "'CLAUDE_CODE_USE_VERTEX' in data['env']")" "False" "Conflicting CLAUDE_CODE_USE_VERTEX removed while routed"
assert_eq "$(json_field "${CLAUDE_FILE_CONTENT}" "'DISABLE_PROMPT_CACHING' in data['env']")" "False" "Prompt caching enforced (DISABLE_PROMPT_CACHING absent)"
assert_eq "$(json_field "${CLAUDE_FILE_CONTENT}" "data['theme']")" "custom:omarchy" "Claude theme preserved"
assert_eq "$(json_field "${CLAUDE_FILE_CONTENT}" "'Stop' in data['hooks']")" "True" "Claude hooks preserved"

echo "==> 4. Enable Codex routing"
RES_CODEX="$("${CTL}" enable --target codex)"
assert_eq "$(json_field "${RES_CODEX}" "data['claude_enabled']")" "True" "Claude remains enabled"
assert_eq "$(json_field "${RES_CODEX}" "data['codex_enabled']")" "True" "Codex routing enabled"
assert_eq "$(json_field "${RES_CODEX}" "data['both_enabled']")" "True" "both_enabled is True"
assert_eq "$(json_field "${RES_CODEX}" "data['badge_text']")" "C+X -> G3.8" "Badge shows C+X -> G3.8"

CODEX_FILE_CONTENT="$(cat "${CODEX_CONFIG_PATH}")"
assert_contains "${CODEX_FILE_CONTENT}" 'model_provider = "gemini_adc"' "Codex model_provider set to gemini_adc"
assert_contains "${CODEX_FILE_CONTENT}" 'model = "gemini-3.8-flash"' "Codex model set to gemini-3.8-flash"
assert_contains "${CODEX_FILE_CONTENT}" '[model_providers.gemini_adc]' "Codex provider block written"
assert_contains "${CODEX_FILE_CONTENT}" 'hooks = true' "Codex [features] preserved"
assert_contains "${CODEX_FILE_CONTENT}" 'screen_reader_detection_done = true' "Codex [tui] preserved"

echo "==> 5. Hot-swap model to Gemini 4.0 Argon & custom models"
RES_MODEL="$("${CTL}" set-model --model gemini-4-argon --effort high)"
assert_eq "$(json_field "${RES_MODEL}" "data['primary_model']")" "gemini-4-argon" "Primary model updated to gemini-4-argon"
assert_eq "$(json_field "${RES_MODEL}" "data['reasoning_effort']")" "high" "Reasoning effort updated to high"
assert_eq "$(json_field "${RES_MODEL}" "data['badge_text']")" "C+X -> G4" "Badge updated to C+X -> G4"

CLAUDE_SWAPPED="$(cat "${CLAUDE_SETTINGS_PATH}")"
assert_eq "$(json_field "${CLAUDE_SWAPPED}" "data['env']['ANTHROPIC_MODEL']")" "gemini-4-argon" "Claude ANTHROPIC_MODEL updated to gemini-4-argon"
assert_eq "$(json_field "${CLAUDE_SWAPPED}" "data['env']['ANTHROPIC_DEFAULT_OPUS_MODEL']")" "gemini-4-argon" "Claude ANTHROPIC_DEFAULT_OPUS_MODEL updated to gemini-4-argon"

CODEX_SWAPPED="$(cat "${CODEX_CONFIG_PATH}")"
assert_contains "${CODEX_SWAPPED}" 'model = "gemini-4-argon"' "Codex model updated to gemini-4-argon"
assert_contains "${CODEX_SWAPPED}" 'model_reasoning_effort = "high"' "Codex effort updated to high"

RES_FLIP="$("${CTL}" set-model --flip)"
assert_eq "$(json_field "${RES_FLIP}" "data['primary_model']")" "gemini-3.8-flash" "--flip toggled back to gemini-3.8-flash"

RES_CUSTOM="$("${CTL}" set-model --model gemini-2.5-pro)"
assert_eq "$(json_field "${RES_CUSTOM}" "data['primary_model']")" "gemini-2.5-pro" "Custom model gemini-2.5-pro accepted"
assert_eq "$(json_field "${RES_CUSTOM}" "data['primary_model_display']")" "Gemini 2.5 Pro" "Dynamic display name is Gemini 2.5 Pro"
assert_eq "$(json_field "${RES_CUSTOM}" "data['badge_text']")" "C+X -> G2.5P" "Dynamic badge shows C+X -> G2.5P"
assert_eq "$(json_field "${RES_CUSTOM}" "'gemini-2.5-pro' in data['available_models']")" "True" "gemini-2.5-pro added to available_models"

RES_FLIP_1="$("${CTL}" set-model --flip)"
assert_eq "$(json_field "${RES_FLIP_1}" "data['primary_model']")" "gemini-3.8-flash" "Multi-model flip cycles gemini-2.5-pro -> gemini-3.8-flash"
RES_FLIP_2="$("${CTL}" set-model --flip)"
assert_eq "$(json_field "${RES_FLIP_2}" "data['primary_model']")" "gemini-4-argon" "Multi-model flip cycles gemini-3.8-flash -> gemini-4-argon"
RES_FLIP_3="$("${CTL}" set-model --flip)"
assert_eq "$(json_field "${RES_FLIP_3}" "data['primary_model']")" "gemini-2.5-pro" "Multi-model flip cycles gemini-4-argon -> gemini-2.5-pro"

RES_REM="$("${CTL}" set-model --remove-model gemini-2.5-pro --model gemini-3.8-flash)"
assert_eq "$(json_field "${RES_REM}" "data['primary_model']")" "gemini-3.8-flash" "Removed custom model and reset to gemini-3.8-flash"

echo "==> 6. Disable both and verify lossless restoration of pre-existing configs"
RES_DISABLE="$("${CTL}" disable --target both)"
assert_eq "$(json_field "${RES_DISABLE}" "data['claude_enabled']")" "False" "Claude disabled"
assert_eq "$(json_field "${RES_DISABLE}" "data['codex_enabled']")" "False" "Codex disabled"
assert_eq "$(json_field "${RES_DISABLE}" "data['any_enabled']")" "False" "any_enabled is False"

CLAUDE_RESTORED="$(cat "${CLAUDE_SETTINGS_PATH}")"
assert_eq "$(json_field "${CLAUDE_RESTORED}" "data['env']['CLAUDE_CODE_USE_VERTEX']")" "1" "Restored original CLAUDE_CODE_USE_VERTEX=1"
assert_eq "$(json_field "${CLAUDE_RESTORED}" "data['env']['ANTHROPIC_DEFAULT_OPUS_MODEL']")" "claude-opus-5-5" "Restored original ANTHROPIC_DEFAULT_OPUS_MODEL"
assert_eq "$(json_field "${CLAUDE_RESTORED}" "'ANTHROPIC_BASE_URL' in data['env']")" "False" "Removed router ANTHROPIC_BASE_URL on restore"
assert_eq "$(json_field "${CLAUDE_RESTORED}" "data['theme']")" "custom:omarchy" "Preserved theme on restore"

CODEX_RESTORED="$(cat "${CODEX_CONFIG_PATH}")"
assert_contains "${CODEX_RESTORED}" 'hooks = true' "Preserved Codex [features] on restore"
assert_contains "${CODEX_RESTORED}" 'screen_reader_detection_done = true' "Preserved Codex [tui] on restore"
if [[ "${CODEX_RESTORED}" == *"gemini_adc"* ]]; then
  echo "  [FAIL] Expected gemini_adc to be removed from restored Codex config" >&2
  FAIL_COUNT=$((FAIL_COUNT + 1))
else
  echo "  [PASS] Removed gemini_adc from restored Codex config"
  PASS_COUNT=$((PASS_COUNT + 1))
fi

echo "==> 7. Test toggle, daemon & Omarchy Menu subcommands"
RES_TOGGLE_ON="$("${CTL}" toggle --target both)"
assert_eq "$(json_field "${RES_TOGGLE_ON}" "data['both_enabled']")" "True" "Toggle both -> ON"
RES_TOGGLE_OFF="$("${CTL}" toggle --target both)"
assert_eq "$(json_field "${RES_TOGGLE_OFF}" "data['any_enabled']")" "False" "Toggle both -> OFF"

RES_STOP="$("${CTL}" stop-router)"
assert_eq "$(json_field "${RES_STOP}" "data['router_running']")" "False" "stop-router sets router_running=False"
RES_START="$("${CTL}" start-router)"
assert_eq "$(json_field "${RES_START}" "data['router_running']")" "True" "start-router sets router_running=True"

RES_MENU_ON="$("${CTL}" menu --enable)"
assert_eq "$(json_field "${RES_MENU_ON}" "data['menu_enabled']")" "True" "menu --enable sets menu_enabled=True"
MENU_CONTENT="$(cat "${OMARCHY_MENU_CONFIG_PATH}")"
assert_contains "${MENU_CONTENT}" '"trigger.gemini-router"' "omarchy-menu.jsonc contains trigger.gemini-router"
assert_contains "${MENU_CONTENT}" '"personal"' "omarchy-menu.jsonc preserved existing personal entry"

RES_MENU_OFF="$("${CTL}" menu --toggle)"
assert_eq "$(json_field "${RES_MENU_OFF}" "data['menu_enabled']")" "False" "menu --toggle sets menu_enabled=False"
MENU_RESTORED="$(cat "${OMARCHY_MENU_CONFIG_PATH}")"
assert_contains "${MENU_RESTORED}" '"personal"' "omarchy-menu.jsonc preserved personal entry after removal"

echo "==> 8. Security & adversarial tests (Omaplug baseline)"
BAD_TARGET="$("${CTL}" enable --target '; rm -rf /')"
assert_eq "$(json_field "${BAD_TARGET}" "data['ok']")" "False" "Rejects shell-injection target"

BAD_MODEL="$("${CTL}" set-model --model '../../etc/passwd')"
assert_eq "$(json_field "${BAD_MODEL}" "data['ok']")" "False" "Rejects path-traversal model"

BAD_EFFORT="$("${CTL}" set-model --effort 'ultra_invalid')"
assert_eq "$(json_field "${BAD_EFFORT}" "data['ok']")" "False" "Rejects invalid effort"

# Symlink attack test on CLAUDE_SETTINGS_PATH
mv "${CLAUDE_SETTINGS_PATH}" "${CLAUDE_SETTINGS_PATH}.real"
ln -s "${CLAUDE_SETTINGS_PATH}.real" "${CLAUDE_SETTINGS_PATH}"
SYMLINK_RES="$("${CTL}" enable --target claude)"
assert_eq "$(json_field "${SYMLINK_RES}" "data['ok']")" "False" "Refuses to read/write symlinked config (O_NOFOLLOW)"
rm -f "${CLAUDE_SETTINGS_PATH}"
mv "${CLAUDE_SETTINGS_PATH}.real" "${CLAUDE_SETTINGS_PATH}"

echo "==> 9. Validate plugin manifest.json & QML entry point"
MANIFEST_JSON="$(cat "${PLUGIN_DIR}/manifest.json")"
assert_eq "$(json_field "${MANIFEST_JSON}" "data['id']")" "arch.gemini-router" "manifest.json id is arch.gemini-router"
assert_eq "$(json_field "${MANIFEST_JSON}" "data['entryPoints']['barWidget']")" "plugin/GeminiRouter.qml" "manifest.json barWidget entryPoint"
if [[ -f "${PLUGIN_DIR}/plugin/GeminiRouter.qml" ]]; then
  echo "  [PASS] plugin/GeminiRouter.qml exists"
  PASS_COUNT=$((PASS_COUNT + 1))
else
  echo "  [FAIL] plugin/GeminiRouter.qml missing" >&2
  FAIL_COUNT=$((FAIL_COUNT + 1))
fi

echo ""
echo "==================================================="
echo "Test Summary: ${PASS_COUNT} passed, ${FAIL_COUNT} failed"
echo "==================================================="

if [[ "${FAIL_COUNT}" -gt 0 ]]; then
  exit 1
fi
