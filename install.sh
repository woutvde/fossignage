#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Fossignage single-system installer for Raspberry Pi OS (Raspbian) Lite.
#
# Run on the Pi (as root or with sudo):
#   sudo bash install.sh
#
# What it does:
#   1. Installs the backend (Flask app) + builds the native Rust player
#   2. Deploys the code to /opt/fossignage
#   3. Configures autologin on tty1 + native fullscreen player launch
#      (DRM/KMS or /dev/fb0 directly - no X server needed)
#   4. Creates the fossignage-server systemd service
#
# After install, the screen shows the operator console URL. Upload media and
# build a playlist there; the fullscreen player picks it up automatically.
#
# Optional: FOSSIGNAGE_WITH_BROWSER=1 also installs X + chromium so that
# `url` (web page) playlist items can be shown via an external browser
# (--url-command). Images and videos never need it.
# ---------------------------------------------------------------------------
set -euo pipefail

INSTALL_DIR=/opt/fossignage
SERVICE_USER=${SERVICE_USER:-fossignage}
SERVER_PORT=${SERVER_PORT:-5000}
WITH_BROWSER=${FOSSIGNAGE_WITH_BROWSER:-0}
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- pretty output helpers --------------------------------------------------

if [[ -t 1 ]]; then
  C_BLUE=$'\e[1;34m' C_GREEN=$'\e[1;32m' C_RED=$'\e[1;31m' C_DIM=$'\e[2m' C_OFF=$'\e[0m'
else
  C_BLUE="" C_GREEN="" C_RED="" C_DIM="" C_OFF=""
fi

step()   { printf '\n%s== %s ==%s\n' "$C_BLUE" "$1" "$C_OFF"; }
info()   { printf '  %s\n' "$1"; }
ok()     { printf '%s  [ok]%s %s\n' "$C_GREEN" "$C_OFF" "$1"; }
fail()   { printf '%s  [fail]%s %s\n' "$C_RED" "$C_OFF" "$1" >&2; exit 1; }

# Run a command silently while showing an animated spinner.
# On failure, dump the captured output and exit.
spinner() {
  local msg="$1"; shift
  local tmp; tmp=$(mktemp)
  "$@" >"$tmp" 2>&1 &
  local pid=$!
  local frames='|/-\\' i=0 frame
  # Redraw the whole line with \r; pad with spaces to clear leftovers.
  while kill -0 "$pid" 2>/dev/null; do
    frame="${frames:i++%4:1}"
    printf '\r  %s %s   ' "$msg" "$frame"
    sleep 0.15
  done
  wait "$pid"; local rc=$?
  if [[ $rc -eq 0 ]]; then
    printf '\r  %s %s\n' "$msg" "${C_GREEN}done${C_OFF}"
  else
    printf '\r  %s %s\n' "$msg" "${C_RED}FAILED${C_OFF}"
    echo "${C_DIM}---- command output ----${C_OFF}" >&2
    cat "$tmp" >&2
    echo "${C_DIM}------------------------${C_OFF}" >&2
  fi
  rm -f "$tmp"
  return $rc
}

# --- preflight --------------------------------------------------------------

if [[ $EUID -ne 0 ]]; then
  fail "Please run as root: sudo bash $0"
fi

# Ensure sbin dirs are on PATH (minimal installs may omit them)
case ":$PATH:" in
  *":/usr/sbin:"*) ;;
  *) export PATH="$PATH:/usr/sbin:/sbin" ;;
esac

echo
echo "${C_BLUE}  Fossignage - single-system installer (native Rust player)${C_OFF}"
echo "${C_DIM}  target: ${INSTALL_DIR}   user: ${SERVICE_USER}   port: ${SERVER_PORT}${C_OFF}"

# --- packages ---------------------------------------------------------------

step "Installing required packages"
export DEBIAN_FRONTEND=noninteractive
spinner "updating package index"   apt-get update -y

# The native Rust player renders via DRM/KMS (/dev/dri) or the raw
# framebuffer (/dev/fb0) and decodes H.264 video through the V4L2 hardware
# decoder (/dev/video*) - so no X server, feh/fbi, ImageMagick, omxplayer,
# vlc or mpv are needed. ffmpeg stays for server-side transcode to H.264.
PKGS=(
  python3 python3-pip
  passwd
  ca-certificates curl
  ffmpeg
)
if [[ $WITH_BROWSER -eq 1 ]]; then
  TOTAL_MEM_KB=$(awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)
  if [[ ${TOTAL_MEM_KB:-0} -ge 1048576 ]]; then
    PKGS+=(xserver-xorg xinit x11-xserver-utils chromium unclutter)
    info "browser stack enabled (FOSSIGNAGE_WITH_BROWSER=1) - url items will use chromium"
  else
    info "skipping chromium (< 1 GB RAM) - web page items will not be playable"
  fi
else
  info "no browser stack (default) - url/web-page items are skipped by the native player"
fi

spinner "installing packages"      apt-get install -y --no-install-recommends "${PKGS[@]}"

# Debian 12+ and Ubuntu 24.04+: python3-venv is versioned
# (e.g. python3.13-venv) and the meta-package may not pull the real one.
# Detect the running interpreter; fall back to the meta-package.
PYV=$(python3 -c 'import sys; print(f"python{sys.version_info.major}.{sys.version_info.minor}-venv")')
spinner "installing ${PYV}" apt-get install -y "${PYV}" \
  || spinner "installing python3-venv" apt-get install -y python3-venv \
  || fail "could not install python3-venv"

# --- service user -----------------------------------------------------------

step "Creating service user"
if id "$SERVICE_USER" &>/dev/null; then
  ok "user '${SERVICE_USER}' already exists"
else
  spinner "creating user '${SERVICE_USER}'" \
    useradd --system --create-home --shell /bin/bash "$SERVICE_USER"
fi
# video group: access to /dev/dri/card*, /dev/fb0 and /dev/video* (hw decoder)
spinner "adding user to video group" usermod -aG video "$SERVICE_USER"

# --- code -------------------------------------------------------------------

step "Deploying code to ${INSTALL_DIR}"
mkdir -p "$INSTALL_DIR/static/media" "$INSTALL_DIR/templates" "$INSTALL_DIR/bin"
spinner "copying files" bash -c \
  "cp -r '$SRC_DIR'/app.py '$SRC_DIR'/player.py '$SRC_DIR'/requirements.txt '$INSTALL_DIR'/ \
   && { [[ ! -d '$SRC_DIR/templates' ]] || cp -r '$SRC_DIR'/templates/. '$INSTALL_DIR/templates/'; } \
   && { [[ ! -d '$SRC_DIR/static' ]] || cp -r '$SRC_DIR'/static/. '$INSTALL_DIR/static/'; } \
   && { [[ ! -d '$SRC_DIR/player-rs' ]] || cp -r '$SRC_DIR'/player-rs '$INSTALL_DIR/player-rs'; }"

step "Setting up python environment"
spinner "creating venv" python3 -m venv "$INSTALL_DIR/venv" \
  || fail "venv creation failed (is python3-venv installed?)"
spinner "upgrading pip" "$INSTALL_DIR/venv/bin/pip" install --upgrade pip --quiet \
  || fail "pip upgrade failed"
spinner "installing python dependencies" \
  "$INSTALL_DIR/venv/bin/pip" install -r "$INSTALL_DIR/requirements.txt" --quiet \
  || fail "python dependency install failed"

# --- native player (Rust) ---------------------------------------------------

step "Building the native player (Rust)"
export CARGO_HOME="$INSTALL_DIR/.cargo"
export RUSTUP_HOME="$INSTALL_DIR/.rustup"
if [[ ! -x "$CARGO_HOME/bin/cargo" ]]; then
  spinner "installing rust toolchain (rustup)" bash -c \
    "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
       CARGO_HOME='$CARGO_HOME' RUSTUP_HOME='$RUSTUP_HOME' \
       sh -s -- -y --profile minimal --default-toolchain stable"
fi
spinner "cargo build --release" env PATH="$CARGO_HOME/bin:$PATH" \
  CARGO_HOME="$CARGO_HOME" RUSTUP_HOME="$RUSTUP_HOME" \
  cargo build --release --manifest-path "$INSTALL_DIR/player-rs/Cargo.toml" \
  || fail "native player build failed"
spinner "installing player binary" install -m 0755 \
  "$INSTALL_DIR/player-rs/target/release/fossignage-player" "$INSTALL_DIR/bin/fossignage-player"
ok "native player at ${INSTALL_DIR}/bin/fossignage-player"

spinner "setting ownership" chown -R "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR"

# Log directory for the player (writable by the service user)
mkdir -p /var/log/fossignage
chown "$SERVICE_USER:$SERVICE_USER" /var/log/fossignage

# Logrotate: keep player logs bounded (the player also self-rotates at 1MB,
# but logrotate is the safety net for the directory as a whole)
cat > /etc/logrotate.d/fossignage <<EOF
/var/log/fossignage/*.log {
    daily
    rotate 7
    maxsize 5M
    missingok
    notifempty
    compress
    delaycompress
    copytruncate
}
EOF

# ---------------------------------------------------------------------------
# Autologin on tty1 + native player launch from shell profile.
#
# The player talks to the KMS/DRM driver directly, so it runs on the bare
# console (tty1) without starting X. getty autologins the service user on
# tty1; the profile starts the player and restarts it if it ever exits.
# ---------------------------------------------------------------------------
step "Configuring kiosk session"
mkdir -p /etc/systemd/system/getty@tty1.service.d
cat > /etc/systemd/system/getty@tty1.service.d/autologin.conf <<EOF
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin ${SERVICE_USER} --noclear %I \$TERM
Restart=always
EOF
ok "autologin enabled on tty1"

PLAYER_ARGS="--standalone --server http://127.0.0.1:${SERVER_PORT} --code-file ${INSTALL_DIR}/display_code"
if [[ $WITH_BROWSER -eq 1 ]] && command -v chromium >/dev/null 2>&1; then
  # With a browser stack installed, render url items in kiosk chromium.
  PLAYER_ARGS="$PLAYER_ARGS --url-command 'chromium --kiosk --noerrdialogs --disable-infobars --no-first-run --window-size={w}x{h} --window-position=0,0 --user-data-dir=/tmp/fossignage-chrome-{url} {url}'"
fi

cat > /home/${SERVICE_USER}/.bash_profile <<EOF
# Fossignage kiosk: run the native player fullscreen on tty1 (no X needed).
# Skip when not on tty1 or when already running (e.g. SSH sessions).
if [ "\$(tty)" = "/dev/tty1" ] && ! pgrep -f 'bin/fossignage-player' >/dev/null; then
    while true; do
        ${INSTALL_DIR}/bin/fossignage-player ${PLAYER_ARGS}
        echo "Player exited; restarting in 5s... (Ctrl+C within 5s to get a shell)"
        sleep 5
    done
fi
EOF
chown ${SERVICE_USER}:${SERVICE_USER} /home/${SERVICE_USER}/.bash_profile
ok "player launch hook installed"

# --- systemd ----------------------------------------------------------------

step "Creating systemd services"
cat > /etc/systemd/system/fossignage-server.service <<EOF
[Unit]
Description=Fossignage signage backend (Flask)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
WorkingDirectory=${INSTALL_DIR}
Environment=SIGNAGE_HOST=0.0.0.0
Environment=SIGNAGE_PORT=${SERVER_PORT}
ExecStart=${INSTALL_DIR}/venv/bin/python ${INSTALL_DIR}/app.py --standalone
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
ok "fossignage-server.service created"

spinner "reloading systemd" systemctl daemon-reload
spinner "enabling server"   systemctl enable fossignage-server.service

# Drop the old player unit if it exists from a previous install
if systemctl list-unit-files | grep -q fossignage-player; then
  systemctl disable --now fossignage-player.service 2>/dev/null || true
  rm -f /etc/systemd/system/fossignage-player.service
  systemctl daemon-reload
  ok "removed legacy fossignage-player.service"
fi

step "Starting services"
spinner "starting backend" systemctl restart fossignage-server.service
# getty@tty1 restart triggers the autologin, which starts the player
spinner "starting player session" systemctl restart getty@tty1.service

# --- done -------------------------------------------------------------------

IP=$(hostname -I | awk '{print $1}')
cat <<EOF

${C_GREEN}  Install complete!${C_OFF}

  Operator console:  ${C_BLUE}http://${IP}:${SERVER_PORT}/operator${C_OFF}
  Mode:              STANDALONE (no display linking; content plays directly)
  Player:            native Rust (${INSTALL_DIR}/bin/fossignage-player)
                     renders via DRM/KMS or /dev/fb0, decodes H.264 video
                     with the Pi's V4L2 hardware decoder - no X, mpv, vlc
                     or image viewer processes needed.

  Upload media and build a playlist on the operator console; the
  fullscreen player picks it up automatically.

${C_DIM}  Useful commands:
    sudo systemctl status fossignage-server
    sudo systemctl restart getty@tty1      # restart the player session
    sudo journalctl -u getty@tty1 -f       # follow player output
    sudo rm ${INSTALL_DIR}/display_code && sudo systemctl restart getty@tty1${C_OFF}

EOF
