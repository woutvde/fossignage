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
#   3. Creates systemd services:
#        fossignage-server.service  -> Flask backend on :5000
#        fossignage-player.service  -> X session + fullscreen native player
#   4. Enables both to start on boot
#
# After install, the Pi shows a pairing code on screen. Enter that code on
# the operator console (http://<pi-ip>:5000/operator) to link the display.
# ---------------------------------------------------------------------------
set -euo pipefail

INSTALL_DIR=/opt/fossignage
SERVICE_USER=${SERVICE_USER:-fossignage}
SERVER_PORT=${SERVER_PORT:-5000}
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ $EUID -ne 0 ]]; then
  echo "Please run as root: sudo bash $0" >&2
  exit 1
fi

# Ensure sbin dirs are on PATH (minimal installs may omit them)
case ":$PATH:" in
  *":/usr/sbin:"*) ;;
  *) export PATH="$PATH:/usr/sbin:/sbin" ;;
esac

# Install all required packages first
echo "==> Installing system packages (this can take a few minutes)..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y --no-install-recommends \
  python3 python3-pip \
  passwd \
  xserver-xorg xinit xterm \
  feh fbi \
  imagemagick \
  fonts-dejavu-core \
  chromium \
  ffmpeg \
  vlc
# Dedicated system user to run the services (override with SERVICE_USER=x)
if ! id "$SERVICE_USER" &>/dev/null; then
  useradd --system --create-home --shell /bin/bash "$SERVICE_USER"
  echo "==> Created service user: $SERVICE_USER"
fi



# omxplayer is not available on newer (bookworm+) releases; that's fine,
# the player falls back to vlc/ffplay automatically.
apt-get install -y --no-install-recommends omxplayer 2>/dev/null \
  || echo "    (omxplayer unavailable - will use vlc/ffplay for video)"

# Debian 12+: python3-venv is versioned (e.g. python3.13-venv) and the
# meta-package may not pull the real one. Detect the running interpreter.
PYV=$(python3 -c 'import sys; print(f"python{sys.version_info.major}.{sys.version_info.minor}-venv")')
apt-get install -y "${PYV}" || apt-get install -y python3-venv

echo "==> Deploying code to ${INSTALL_DIR}..."
mkdir -p "$INSTALL_DIR/static/media" "$INSTALL_DIR/templates"
cp -r "$SRC_DIR"/app.py "$SRC_DIR"/player.py "$SRC_DIR"/requirements.txt "$INSTALL_DIR"/
[[ -d "$SRC_DIR/templates" ]] && cp -r "$SRC_DIR"/templates/. "$INSTALL_DIR/templates/"
[[ -d "$SRC_DIR/static" ]] && cp -r "$SRC_DIR"/static/. "$INSTALL_DIR/static/"

echo "==> Creating python venv..."
python3 -m venv "$INSTALL_DIR/venv"
"$INSTALL_DIR/venv/bin/pip" install --upgrade pip
"$INSTALL_DIR/venv/bin/pip" install -r "$INSTALL_DIR/requirements.txt"

chown -R "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR"

# ---------------------------------------------------------------------------
# Autologin on tty1 + player launch from shell profile
#
# Launching xinit directly from a systemd unit is unreliable (X needs a real
# login session for tty ownership and .Xauthority). Instead we configure
# getty to autologin the service user on tty1, and start X from the shell
# profile - the same approach Raspberry Pi OS uses.
# ---------------------------------------------------------------------------
echo "==> Configuring autologin on tty1..."
mkdir -p /etc/systemd/system/getty@tty1.service.d
cat > /etc/systemd/system/getty@tty1.service.d/autologin.conf <<EOF
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin ${SERVICE_USER} --noclear %I \$TERM
Restart=always
EOF

echo "==> Installing player launch hook..."
cat > /home/${SERVICE_USER}/.bash_profile <<'EOF'
# Fossignage kiosk: start X + fullscreen player on tty1 login.
# Skip when not on tty1 or when already running (e.g. SSH sessions).
if [ "$(tty)" = "/dev/tty1" ] && ! pgrep -f 'player.py' >/dev/null; then
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

# ---------------------------------------------------------------------------
# systemd: backend server
# ---------------------------------------------------------------------------
echo "==> Creating systemd services..."
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

systemctl daemon-reload
systemctl enable fossignage-server.service

# Drop the old player unit if it exists from a previous install
if systemctl list-unit-files | grep -q fossignage-player; then
  systemctl disable --now fossignage-player.service 2>/dev/null || true
  rm -f /etc/systemd/system/fossignage-player.service
  systemctl daemon-reload
fi

echo "==> Starting services..."
systemctl restart fossignage-server.service
# getty@tty1 restart triggers the autologin, which starts the player
systemctl restart getty@tty1.service

cat <<EOF

-------------------------------------------------------------
 Fossignage single-system install complete!

 Backend:      http://$(hostname -I | awk '{print $1}'):${SERVER_PORT}/operator
 Mode:         STANDALONE (no display linking; content plays directly)
 Player:       autologin on tty1 -> X + fullscreen player
 Server:       fossignage-server.service (Flask)

 Upload media and build a playlist on the operator console; the
 fullscreen player picks it up automatically.

 Useful commands:
   sudo systemctl status fossignage-server
   sudo systemctl restart getty@tty1      # restart the player session
   # forget pairing / re-pair:
   sudo rm ${INSTALL_DIR}/display_code && sudo systemctl restart getty@tty1
-------------------------------------------------------------
EOF
