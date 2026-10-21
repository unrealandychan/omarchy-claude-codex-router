#!/usr/bin/env bash
# Uninstaller for the Omarchy Gemini ADC Router Plugin (arch.gemini-router)
set -euo pipefail

PLUGIN_ID="arch.gemini-router"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
CANONICAL_SRC="$(realpath "$SCRIPT_DIR" 2>/dev/null || (cd "$SCRIPT_DIR" && pwd -P))"

BIN_DIR="$HOME/.local/bin"
ROUTER_DEST="$BIN_DIR/claude-gemini-router"
CTL_DEST="$BIN_DIR/omarchy-gemini-router-ctl"
CLAUDE_WRAPPER_DEST="$BIN_DIR/claude-gemini"
CODEX_WRAPPER_DEST="$BIN_DIR/codex-gemini"

PLUGIN_DEST="$HOME/.config/omarchy/plugins/${PLUGIN_ID}"
SYSTEMD_USER_DIR="$HOME/.config/systemd/user"
SERVICE_FILE="$SYSTEMD_USER_DIR/claude-gemini-router.service"
SHELL_CONFIG="$HOME/.config/omarchy/shell.json"
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/omarchy/${PLUGIN_ID}"
RECEIPT_FILE="$STATE_DIR/install-receipt.json"
CURRENT_UID="$(id -u)"

check_ownership() {
  local target="$1"
  if [ -e "$target" ] || [ -L "$target" ]; then
    local target_uid
    target_uid="$(stat -c %u "$target" 2>/dev/null || true)"
    if [ -n "$target_uid" ] && [ "$target_uid" -ne "$CURRENT_UID" ]; then
      echo "Error: $target is owned by UID $target_uid, not current user UID $CURRENT_UID. Refusing to modify." >&2
      exit 1
    fi
  fi
}

get_unused_backup_path() {
  local target="$1"
  local base
  base="${target}.bak.$(date +%s)"
  local backup="$base"
  local n=1
  while [ -e "$backup" ] || [ -L "$backup" ]; do
    backup="${base}-${n}"
    n=$((n + 1))
  done
  echo "$backup"
}

read_receipt_field() {
  local field="$1"
  if [ -f "$RECEIPT_FILE" ] && [ ! -L "$RECEIPT_FILE" ]; then
    check_ownership "$RECEIPT_FILE"
    RECEIPT_FILE="$RECEIPT_FILE" FIELD="$field" python3 -c "
import json, os
try:
    fd = os.open(os.environ['RECEIPT_FILE'], os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0))
    with os.fdopen(fd, 'r', encoding='utf-8') as f:
        data = json.load(f)
    val = data.get(os.environ['FIELD'], '')
    if isinstance(val, str):
        print(val)
except Exception:
    pass
" 2>/dev/null || true
  fi
}

RECEIPT_SRC="$(read_receipt_field "source_dir")"
RECEIPT_OWNED=0
if [ -n "$RECEIPT_SRC" ] && { [ "$RECEIPT_SRC" = "$SCRIPT_DIR" ] || [ "$RECEIPT_SRC" = "$CANONICAL_SRC" ]; }; then
  RECEIPT_OWNED=1
fi

remove_verified_file() {
  local target="$1"
  local receipt_key="$2"
  local repo_rel="$3"

  check_ownership "$target"
  if [ -L "$target" ]; then
    local link_target
    link_target="$(realpath "$target" 2>/dev/null || readlink "$target" || true)"
    if [ "$link_target" = "$SCRIPT_DIR/$repo_rel" ] || [ "$link_target" = "$CANONICAL_SRC/$repo_rel" ]; then
      echo "==> Removing symlink $target..."
      rm -f "$target"
    else
      echo "==> Symlink $target does not point to this checkout; leaving intact."
    fi
  elif [ -f "$target" ]; then
    local actual_hash expected_hash
    actual_hash="$(sha256sum "$target" | awk '{print $1}')"
    expected_hash="$(read_receipt_field "$receipt_key")"
    if [ -n "$expected_hash" ] && [ "$actual_hash" = "$expected_hash" ] && [ "$RECEIPT_OWNED" -eq 1 ]; then
      echo "==> Removing verified file $target..."
      rm -f "$target"
    elif [ -f "$SCRIPT_DIR/$repo_rel" ] && cmp -s "$SCRIPT_DIR/$repo_rel" "$target"; then
      echo "==> Removing checkout-matching file $target..."
      rm -f "$target"
    else
      echo "==> $target was not installed by this checkout; leaving intact."
    fi
  fi
}

echo "==> Uninstalling ${PLUGIN_ID}..."

if [ "$RECEIPT_OWNED" -eq 1 ] && [ -x "$CTL_DEST" ] && [ ! -L "$CTL_DEST" ]; then
  OMARCHY_GEMINI_ROUTER_NO_NOTIFY=1 "$CTL_DEST" disable --target both >/dev/null 2>&1 || true
  OMARCHY_GEMINI_ROUTER_NO_NOTIFY=1 "$CTL_DEST" menu --disable >/dev/null 2>&1 || true
  OMARCHY_GEMINI_ROUTER_NO_NOTIFY=1 "$CTL_DEST" stop-router >/dev/null 2>&1 || true
fi

if [ -f "$SERVICE_FILE" ] || [ -L "$SERVICE_FILE" ]; then
  check_ownership "$SERVICE_FILE"
  ACTUAL_SVC_HASH="$(sha256sum "$SERVICE_FILE" 2>/dev/null | awk '{print $1}' || true)"
  EXPECTED_SVC_HASH="$(read_receipt_field "service_sha256")"
  if [ "$RECEIPT_OWNED" -eq 1 ] && [ -n "$EXPECTED_SVC_HASH" ] && [ "$ACTUAL_SVC_HASH" = "$EXPECTED_SVC_HASH" ]; then
    if command -v systemctl >/dev/null 2>&1; then
      systemctl --user disable --now claude-gemini-router.service >/dev/null 2>&1 || true
    fi
    rm -f "$SERVICE_FILE"
    if command -v systemctl >/dev/null 2>&1; then
      systemctl --user daemon-reload >/dev/null 2>&1 || true
    fi
  fi
fi

remove_verified_file "$ROUTER_DEST" "router_sha256" "target/release/claude-gemini-router"
remove_verified_file "$CTL_DEST" "ctl_sha256" "target/release/omarchy-gemini-router-ctl"
remove_verified_file "$CLAUDE_WRAPPER_DEST" "claude_wrapper_sha256" "bin/claude-gemini"
remove_verified_file "$CODEX_WRAPPER_DEST" "codex_wrapper_sha256" "bin/codex-gemini"

if [ "$RECEIPT_OWNED" -eq 1 ] && [ -f "$RECEIPT_FILE" ] && [ ! -L "$RECEIPT_FILE" ]; then
  rm -f "$RECEIPT_FILE"
  rmdir "$STATE_DIR" 2>/dev/null || true
fi

check_ownership "$PLUGIN_DEST"
THIS_INSTALLATION_OWNS_PLUGIN=0

if [ -L "$PLUGIN_DEST" ]; then
  CANONICAL_DEST="$(realpath "$PLUGIN_DEST" 2>/dev/null || true)"
  DEST_TARGET="$(readlink "$PLUGIN_DEST" 2>/dev/null || true)"
  DEST_TARGET_CANONICAL="$(realpath -m "$PLUGIN_DEST" 2>/dev/null || true)"
  if [ "$CANONICAL_DEST" = "$SCRIPT_DIR" ] || [ "$CANONICAL_DEST" = "$CANONICAL_SRC" ] || \
     [ "$DEST_TARGET" = "$SCRIPT_DIR" ] || [ "$DEST_TARGET" = "$CANONICAL_SRC" ] || \
     [ "$DEST_TARGET_CANONICAL" = "$SCRIPT_DIR" ] || [ "$DEST_TARGET_CANONICAL" = "$CANONICAL_SRC" ]; then
    echo "==> Removing plugin symlink at $PLUGIN_DEST..."
    rm -f "$PLUGIN_DEST"
    THIS_INSTALLATION_OWNS_PLUGIN=1
  else
    echo "==> Plugin symlink at $PLUGIN_DEST points to $DEST_TARGET (not this checkout); leaving intact."
  fi
elif [ -d "$PLUGIN_DEST" ]; then
  DEST_REAL="$(cd "$PLUGIN_DEST" && pwd -P)"
  if [ "$SCRIPT_DIR" = "$DEST_REAL" ] || [ "$CANONICAL_SRC" = "$DEST_REAL" ]; then
    echo "==> Running uninstaller from destination directory; leaving source files intact."
    THIS_INSTALLATION_OWNS_PLUGIN=1
  else
    echo "==> Destination $PLUGIN_DEST is a directory (not a symlink to this checkout); leaving intact."
  fi
elif [ ! -e "$PLUGIN_DEST" ] && [ "$RECEIPT_OWNED" -eq 1 ]; then
  THIS_INSTALLATION_OWNS_PLUGIN=1
fi

if [ "$THIS_INSTALLATION_OWNS_PLUGIN" -eq 1 ]; then
  if [ -L "$SHELL_CONFIG" ]; then
    echo "Error: $SHELL_CONFIG is a symlink. Refusing to modify." >&2
    exit 1
  fi

  if [ -f "$SHELL_CONFIG" ]; then
    check_ownership "$SHELL_CONFIG"
    if grep -q "$PLUGIN_ID" "$SHELL_CONFIG"; then
      echo "==> Removing $PLUGIN_ID from $SHELL_CONFIG..."
      BACKUP="$(get_unused_backup_path "$SHELL_CONFIG")"
      cp "$SHELL_CONFIG" "$BACKUP"
      SHELL_CONFIG="$SHELL_CONFIG" PLUGIN_ID="$PLUGIN_ID" python3 -c "
import json, os, tempfile

config_path = os.environ['SHELL_CONFIG']
plugin_id = os.environ['PLUGIN_ID']
dirname = os.path.dirname(config_path)
basename = os.path.basename(config_path)
current_uid = os.getuid()

dir_fd = os.open(dirname, os.O_RDONLY | os.O_DIRECTORY | getattr(os, 'O_NOFOLLOW', 0))
try:
    dir_stat = os.fstat(dir_fd)
    if dir_stat.st_uid != current_uid:
        raise PermissionError(f'Directory {dirname} is owned by UID {dir_stat.st_uid}, not {current_uid}')

    fd = os.open(basename, os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0), dir_fd=dir_fd)
    try:
        st = os.fstat(fd)
        if st.st_uid != current_uid:
            raise PermissionError(f'File {basename} is owned by UID {st.st_uid}, not {current_uid}')
        orig_mode = st.st_mode & 0o777
        with os.fdopen(fd, 'r', encoding='utf-8') as f:
            cfg = json.load(f)
    except Exception:
        try:
            os.close(fd)
        except OSError:
            pass
        raise

    for section in ['left', 'center', 'right']:
        items = cfg.get('bar', {}).get('layout', {}).get(section, [])
        if isinstance(items, list):
            cfg['bar']['layout'][section] = [
                w for w in items if not (isinstance(w, dict) and w.get('id') == plugin_id)
            ]

    tmp_fd, tmp_path = tempfile.mkstemp(prefix=f'.{basename}-', suffix='.tmp', dir=dirname)
    tmp_base = os.path.basename(tmp_path)
    try:
        with os.fdopen(tmp_fd, 'w', encoding='utf-8') as f:
            json.dump(cfg, f, indent=2)
            f.write('\n')
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp_path, orig_mode)
        os.replace(tmp_base, basename, src_dir_fd=dir_fd, dst_dir_fd=dir_fd)
        tmp_path = None
    finally:
        if tmp_path and os.path.exists(tmp_path):
            try:
                os.unlink(tmp_path)
            except OSError:
                pass
finally:
    os.close(dir_fd)
"
    fi
  fi

  omarchy-shell shell rescanPlugins 2>/dev/null || true
fi

echo "==> Uninstallation complete."
