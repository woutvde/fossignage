#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Fossignage single-system installer for Raspberry Pi OS (Raspbian) Lite.
#
# Run on the Pi (as root or with sudo):
#   sudo bash install.sh
#
# What it does:
#   1. Installs the backend (Flask app) + native player + X server packages
#   2. Deploys the fossignage code to /opt/fossignage
#   3. Configures autologin on tty1 + fullscreen player launch
#   4. Creates the fossignage-server systemd service
#
# After install, the screen shows the operator console URL. Upload media and
# build a playlist there; the fullscreen player picks it up automatically.
# ---------------------------------------------------------------------------
set -euo pipefail

INSTALL_DIR=/opt/fossignage
SERVICE_USER=${SERVICE_USER:-fossignage}
SERVER_PORT=${SERVER_PORT:-5000}
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
  local frames='|/-\' i=0 frame
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
echo "${C_BLUE}  Fossignage - single-system installer${C_OFF}"
echo "${C_DIM}  target: ${INSTALL_DIR}   user: ${SERVICE_USER}   port: ${SERVER_PORT}${C_OFF}"

# --- packages ---------------------------------------------------------------

step "Installing required packages"
export DEBIAN_FRONTEND=noninteractive
spinner "updating package index"   apt-get update -y

# Video player choice depends on OS generation:
#   - Raspberry Pi OS / Raspbian Bullseye or older: omxplayer (hw-accelerated)
#   - Debian/Ubuntu Bullseye+ and Pi OS Bookworm+: VLC + ffplay
#     (omxplayer is discontinued and absent from these repos)
OMX_AVAILABLE=0
if grep -qs 'raspbian\|raspberry' /etc/os-release; then
  . /etc/os-release
  if [[ ${VERSION_ID:-0} -le 11 ]]; then
    OMX_AVAILABLE=1
  fi
fi

# Chromium is a memory hog - skip it on machines with < 1 GB RAM (URL/web
# page items won't be playable there; images and video still work).
TOTAL_MEM_KB=$(awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)
CHROMIUM_PKG=()
if [[ ${TOTAL_MEM_KB:-0} -ge 1048576 ]]; then
  CHROMIUM_PKG=(chromium)
else
  info "skipping chromium (< 1 GB RAM) - web page items will not be playable"
fi

if [[ $OMX_AVAILABLE -eq 1 ]]; then
  spinner "installing omxplayer (hw-accelerated video)" \
    apt-get install -y --no-install-recommends omxplayer \
    || info "omxplayer install failed - falling back to vlc/ffplay"
else
  info "skipping omxplayer (not available on this OS) - video will use vlc/ffplay"
fi

spinner "installing packages"      apt-get install -y --no-install-recommends \
  python3 python3-pip \
  passwd \
  xserver-xorg xinit xterm \
  x11-xserver-utils \
  feh fbi \
  imagemagick \
  fonts-dejavu-core \
  "${CHROMIUM_PKG[@]}" \
  ffmpeg \
  mpv \
  vlc \
  unclutter

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

# --- code -------------------------------------------------------------------

step "Deploying code to ${INSTALL_DIR}"
mkdir -p "$INSTALL_DIR/static/media" "$INSTALL_DIR/templates"
spinner "copying files" bash -c \
  "cp -r '$SRC_DIR'/app.py '$SRC_DIR'/player.py '$SRC_DIR'/requirements.txt '$INSTALL_DIR'/ \
   && { [[ ! -d '$SRC_DIR/templates' ]] || cp -r '$SRC_DIR'/templates/. '$INSTALL_DIR/templates/'; } \
   && { [[ ! -d '$SRC_DIR/static' ]] || cp -r '$SRC_DIR'/static/. '$INSTALL_DIR/static/'; }"

step "Setting up python environment"
spinner "creating venv" python3 -m venv "$INSTALL_DIR/venv" \
  || fail "venv creation failed (is python3-venv installed?)"
spinner "upgrading pip" "$INSTALL_DIR/venv/bin/pip" install --upgrade pip --quiet \
  || fail "pip upgrade failed"
spinner "installing python dependencies" \
  "$INSTALL_DIR/venv/bin/pip" install -r "$INSTALL_DIR/requirements.txt" --quiet \
  || fail "python dependency install failed"

spinner "setting ownership" chown -R "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR"

# Log directory for the player (writable by the service user)
mkdir -p /var/log/fossignage
chown "$SERVICE_USER:$SERVICE_USER" /var/log/fossignage

# Logrotate: keep player logs bounded (player.py also self-rotates at 1MB,
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
# Autologin on tty1 + player launch from shell profile
#
# Launching xinit directly from a systemd unit is unreliable (X needs a real
# login session for tty ownership and .Xauthority). Instead we configure
# getty to autologin the service user on tty1, and start X from the shell
# profile - the same approach Raspberry Pi OS uses.
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

cat > /home/${SERVICE_USER}/.bash_profile <<'EOF'
# Fossignage kiosk: start X + fullscreen player on tty1 login.
# Skip when not on tty1 or when already running (e.g. SSH sessions).
if [ "$(tty)" = "/dev/tty1" ] && ! pgrep -f 'player.py' >/dev/null; then
    # Hide the mouse cursor on the kiosk screen
    unclutter -idle 0 -root -noevents &
    while true; do
        xinit /opt/fossignage/venv/bin/python /opt/fossignage/player.py \
            --standalone \
            --server http://127.0.0.1:5000 \
            --code-file /opt/fossignage/display_code \
            -- :0 -nolisten tcp vt1
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

  Upload media and build a playlist on the operator console; the
  fullscreen player picks it up automatically.

${C_DIM}  Useful commands:
    sudo systemctl status fossignage-server
    sudo systemctl restart getty@tty1      # restart the player session
    sudo rm ${INSTALL_DIR}/display_code && sudo systemctl restart getty@tty1${C_OFF}

EOF
