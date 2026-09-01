# Fossignage

Simple server-client digital signage software based on Python and Flask.

## Features

- Supports a wide range of images, videos and web pages
- Display to server linking via 4 character codes.
- Remembers previously linked displays via browser cookie.
- Operator page with media upload and playlist editor

## Single-system mode (Raspberry Pi)

The same Flask backend can run on the Pi itself, with a native fullscreen
player instead of a browser tab. No extra Python dependencies are needed for
the player (stdlib only).

### Quick install (Raspbian Lite)

```bash
sudo bash install.sh
```

This installs X, `feh`/`fbi`, `omxplayer`/`vlc`/`ffmpeg`, `chromium` and
ImageMagick, deploys the code to `/opt/fossignage`, and creates two systemd
services that start on boot in **standalone mode**:

- `fossignage-server.service` — Flask backend on port 5000 (`--standalone`)
- `fossignage-player.service` — X session + fullscreen native player
  (`--standalone`)

In standalone mode there is no display linking (like old Screenly OSE): the
idle screen shows the server's address, and you upload media / build the
playlist directly on the operator console at `http://<pi-ip>:5000/operator`.
Playback starts automatically.

### Manual run

```bash
# Standalone (no linking; player shows the server address when idle)
python3 app.py --standalone &
python3 player.py --standalone --server http://127.0.0.1:5000

# Classic client/server (pairing via 4-char codes)
python3 app.py &
python3 player.py --server http://127.0.0.1:5000
```

`--standalone` can also be enabled via the `SIGNAGE_STANDALONE=1` environment
variable on the server.

Playback backends, chosen automatically by availability:

| Media  | Players (in order of preference)        |
|--------|------------------------------------------|
| Video  | omxplayer, vlc, mpv, ffplay              |
| Images | feh (X), fbi (framebuffer)               |
| URLs   | chromium --kiosk                         |

To forget the pairing and re-pair: `rm /opt/fossignage/display_code &&
sudo systemctl restart fossignage-player`.
