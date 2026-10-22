#!/usr/bin/env bash
# Installer for the Omarchy Gemini ADC Router Plugin (arch.gemini-router)
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
CONFIG_DIR="$HOME/.config/omarchy/gemini-router"
ENV_FILE="$CONFIG_DIR/router.env"
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

install_file_with_backup() {
  local src="$1"
  local dst="$2"
  local mode="$3"
  check_ownership "$dst"
  if [ -e "$dst" ] || [ -L "$dst" ]; then
    if [ ! -L "$dst" ] && cmp -s "$src" "$dst"; then
      chmod "$mode" "$dst" 2>/dev/null || true
    else
      local backup
      backup="$(get_unused_backup_path "$dst")"
      echo "==> Preserving existing file at $dst: moving to $backup..."
      mv "$dst" "$backup"
      install -m "$mode" "$src" "$dst"
    fi
  else
    install -m "$mode" "$src" "$dst"
  fi
}

echo "==> Building Rust release binaries..."
cd "$SCRIPT_DIR" || exit 1
cargo build --locked --release
# Remove any build-script symlinks inside target/ so omarchy-plugin-validate stays clean
find "$SCRIPT_DIR/target" -type l -delete 2>/dev/null || true

mkdir -p "$BIN_DIR"
check_ownership "$BIN_DIR"

echo "==> Installing binaries to $BIN_DIR..."
install_file_with_backup "$SCRIPT_DIR/target/release/claude-gemini-router" "$ROUTER_DEST" 755
install_file_with_backup "$SCRIPT_DIR/target/release/omarchy-gemini-router-ctl" "$CTL_DEST" 700
install_file_with_backup "$SCRIPT_DIR/bin/claude-gemini" "$CLAUDE_WRAPPER_DEST" 755
install_file_with_backup "$SCRIPT_DIR/bin/codex-gemini" "$CODEX_WRAPPER_DEST" 755

echo "==> Configuring $CONFIG_DIR..."
if [ -L "$CONFIG_DIR" ]; then
  echo "Error: $CONFIG_DIR is a symlink. Refusing to modify." >&2
  exit 1
fi
mkdir -p "$CONFIG_DIR"
chmod 700 "$CONFIG_DIR" 2>/dev/null || true
check_ownership "$CONFIG_DIR"

if [ ! -e "$ENV_FILE" ] && [ ! -L "$ENV_FILE" ]; then
  ENV_FILE="$ENV_FILE" python3 -c "
import os, tempfile

env_path = os.environ['ENV_FILE']
dirname = os.path.dirname(env_path)
basename = os.path.basename(env_path)
content = 'GEMINI_PRIMARY_MODEL=gemini-3.8-flash\nGEMINI_REASONING_EFFORT=medium\nROUTER_PORT=4000\n'

dir_fd = os.open(dirname, os.O_RDONLY | os.O_DIRECTORY | getattr(os, 'O_NOFOLLOW', 0))
try:
    tmp_fd, tmp_path = tempfile.mkstemp(prefix=f'.{basename}-', suffix='.tmp', dir=dirname)
    tmp_base = os.path.basename(tmp_path)
    try:
        with os.fdopen(tmp_fd, 'w', encoding='utf-8') as f:
            f.write(content)
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp_path, 0o600)
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

echo "==> Installing systemd user service..."
if [ -L "$SYSTEMD_USER_DIR" ]; then
  echo "Error: $SYSTEMD_USER_DIR is a symlink. Refusing to modify." >&2
  exit 1
fi
mkdir -p "$SYSTEMD_USER_DIR"
check_ownership "$SYSTEMD_USER_DIR"
check_ownership "$SERVICE_FILE"
if [ -L "$SERVICE_FILE" ]; then
  echo "Error: $SERVICE_FILE is a symlink. Refusing to overwrite." >&2
  exit 1
fi

SERVICE_FILE="$SERVICE_FILE" python3 -c "
import os, tempfile

service_path = os.environ['SERVICE_FILE']
dirname = os.path.dirname(service_path)
basename = os.path.basename(service_path)
current_uid = os.getuid()
content = '''[Unit]
Description=Claude Code & OpenAI Codex -> Vertex AI Gemini ADC Router (Rust)
After=network-online.target

[Service]
Type=simple
WorkingDirectory=%h
EnvironmentFile=-%h/.config/omarchy/gemini-router/router.env
ExecStart=%h/.local/bin/claude-gemini-router --host 127.0.0.1
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
'''

dir_fd = os.open(dirname, os.O_RDONLY | os.O_DIRECTORY | getattr(os, 'O_NOFOLLOW', 0))
try:
    st = os.fstat(dir_fd)
    if st.st_uid != current_uid:
        raise PermissionError(f'Directory {dirname} is not owned by UID {current_uid}')
    tmp_fd, tmp_path = tempfile.mkstemp(prefix=f'.{basename}-', suffix='.tmp', dir=dirname)
    tmp_base = os.path.basename(tmp_path)
    try:
        with os.fdopen(tmp_fd, 'w', encoding='utf-8') as f:
            f.write(content)
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp_path, 0o644)
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

if command -v systemctl >/dev/null 2>&1; then
  systemctl --user daemon-reload >/dev/null 2>&1 || true
  systemctl --user enable --now claude-gemini-router.service >/dev/null 2>&1 || true
  systemctl --user restart claude-gemini-router.service >/dev/null 2>&1 || true
fi

echo "==> Recording installation receipt..."
if [ -L "$STATE_DIR" ]; then
  echo "Error: $STATE_DIR is a symlink. Refusing to write receipt." >&2
  exit 1
fi
mkdir -p "$STATE_DIR"
chmod 700 "$STATE_DIR" 2>/dev/null || true
if [ ! -d "$STATE_DIR" ] || [ -L "$STATE_DIR" ]; then
  echo "Error: $STATE_DIR is not a directory. Refusing to write receipt." >&2
  exit 1
fi
check_ownership "$STATE_DIR"

if [ -L "$RECEIPT_FILE" ]; then
  rm -f "$RECEIPT_FILE"
fi

ROUTER_HASH="$(sha256sum "$ROUTER_DEST" | awk '{print $1}')"
CTL_HASH="$(sha256sum "$CTL_DEST" | awk '{print $1}')"
CLAUDE_WRAP_HASH="$(sha256sum "$CLAUDE_WRAPPER_DEST" | awk '{print $1}')"
CODEX_WRAP_HASH="$(sha256sum "$CODEX_WRAPPER_DEST" | awk '{print $1}')"
SERVICE_HASH="$(sha256sum "$SERVICE_FILE" | awk '{print $1}')"

RECEIPT_FILE="$RECEIPT_FILE" \
ROUTER_HASH="$ROUTER_HASH" \
CTL_HASH="$CTL_HASH" \
CLAUDE_WRAP_HASH="$CLAUDE_WRAP_HASH" \
CODEX_WRAP_HASH="$CODEX_WRAP_HASH" \
SERVICE_HASH="$SERVICE_HASH" \
CANONICAL_SRC="$CANONICAL_SRC" \
python3 -c "
import json, os, tempfile

receipt_path = os.environ['RECEIPT_FILE']
state_dir = os.path.dirname(receipt_path)
data = {
    'router_sha256': os.environ['ROUTER_HASH'],
    'ctl_sha256': os.environ['CTL_HASH'],
    'claude_wrapper_sha256': os.environ['CLAUDE_WRAP_HASH'],
    'codex_wrapper_sha256': os.environ['CODEX_WRAP_HASH'],
    'service_sha256': os.environ['SERVICE_HASH'],
    'source_dir': os.environ['CANONICAL_SRC'],
}
fd, tmp_path = tempfile.mkstemp(prefix='.receipt-', suffix='.tmp', dir=state_dir)
try:
    with os.fdopen(fd, 'w', encoding='utf-8') as f:
        json.dump(data, f, indent=2)
        f.write('\n')
        f.flush()
        os.fsync(f.fileno())
    os.chmod(tmp_path, 0o600)
    os.replace(tmp_path, receipt_path)
    tmp_path = None
finally:
    if tmp_path and os.path.exists(tmp_path):
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
"

echo "==> Linking plugin to $PLUGIN_DEST..."
mkdir -p "$HOME/.config/omarchy/plugins"
check_ownership "$PLUGIN_DEST"

if [ -d "$PLUGIN_DEST" ] && [ ! -L "$PLUGIN_DEST" ]; then
  DEST_REAL="$(cd "$PLUGIN_DEST" && pwd -P)"
  if [ "$CANONICAL_SRC" = "$DEST_REAL" ]; then
    echo "==> Running directly from $PLUGIN_DEST; no symlink needed."
  else
    BACKUP="$(get_unused_backup_path "$PLUGIN_DEST")"
    echo "==> Preserving existing directory at $PLUGIN_DEST: moving to $BACKUP..."
    mv "$PLUGIN_DEST" "$BACKUP"
    ln -s "$CANONICAL_SRC" "$PLUGIN_DEST"
  fi
elif [ -L "$PLUGIN_DEST" ]; then
  CURRENT_TARGET="$(realpath "$PLUGIN_DEST" 2>/dev/null || readlink "$PLUGIN_DEST" || true)"
  if [ "$CURRENT_TARGET" = "$CANONICAL_SRC" ]; then
    echo "==> Symlink at $PLUGIN_DEST already points to $CANONICAL_SRC."
  else
    BACKUP="$(get_unused_backup_path "$PLUGIN_DEST")"
    echo "==> Preserving existing symlink at $PLUGIN_DEST: moving to $BACKUP..."
    mv "$PLUGIN_DEST" "$BACKUP"
    ln -s "$CANONICAL_SRC" "$PLUGIN_DEST"
  fi
elif [ -e "$PLUGIN_DEST" ]; then
  BACKUP="$(get_unused_backup_path "$PLUGIN_DEST")"
  echo "==> Preserving existing file at $PLUGIN_DEST: moving to $BACKUP..."
  mv "$PLUGIN_DEST" "$BACKUP"
  ln -s "$CANONICAL_SRC" "$PLUGIN_DEST"
else
  ln -s "$CANONICAL_SRC" "$PLUGIN_DEST"
fi

echo "==> Registering Omarchy Menu (SUPER+SPACE) actions..."
OMARCHY_GEMINI_ROUTER_NO_NOTIFY=1 "$CTL_DEST" menu --enable >/dev/null 2>&1 || true

if [ -L "$SHELL_CONFIG" ]; then
  echo "Error: $SHELL_CONFIG is a symlink. Refusing to modify." >&2
  exit 1
fi

if [ -f "$SHELL_CONFIG" ]; then
  check_ownership "$SHELL_CONFIG"
  if ! grep -q "$PLUGIN_ID" "$SHELL_CONFIG"; then
    echo "==> Adding $PLUGIN_ID to $SHELL_CONFIG..."
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

    right = cfg.setdefault('bar', {}).setdefault('layout', {}).setdefault('right', [])
    if not any(isinstance(w, dict) and w.get('id') == plugin_id for w in right):
        insert_idx = next(
            (i + 1 for i, w in enumerate(right) if isinstance(w, dict) and w.get('id') == 'arch.backup-manager'),
            len(right),
        )
        right.insert(insert_idx, {'id': plugin_id})

    disabled = cfg.get('disabledPlugins')
    if isinstance(disabled, list) and plugin_id in disabled:
        cfg['disabledPlugins'] = [p for p in disabled if p != plugin_id]

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
  else
    echo "==> $PLUGIN_ID already present in $SHELL_CONFIG."
  fi
fi

echo "==> Reloading Omarchy Shell plugins..."
omarchy-shell shell rescanPlugins 2>/dev/null || true
if command -v omarchy >/dev/null 2>&1; then
  if ! grep -q "$PLUGIN_ID" "$SHELL_CONFIG" 2>/dev/null; then
    omarchy plugin enable "$PLUGIN_ID" 2>/dev/null || true
  fi
  omarchy restart shell 2>/dev/null || true
fi

echo "==> Installed ${PLUGIN_ID} v1.1.0."
