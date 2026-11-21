# Fossignage

Simple server-client digital signage software based on Python and Flask.

## Features

- Supports a wide range of images, videos and web pages
- Display to server linking via 4 character codes.
- Remembers previously linked displays via browser cookie.
- Operator page with media upload and playlist editor

## Single-system mode (Raspberry Pi)

The same Flask backend can run on the Pi itself, with a **native Rust
fullscreen player** (`player-rs/`, binary `fossignage-player`) instead of a
browser tab or external player processes.

### Native player (`player-rs/`) — no mpv/vlc/omxplayer/feh/fbi/ImageMagick

The Rust player talks to the same REST API as the browser client and renders
everything itself, as low-level as the Pi allows:

| Concern | Implementation |
|---------|----------------|
| Display output | DRM/KMS (`/dev/dri/card*`, dumb buffers + vsync page flip), fallback `/dev/fb0` — **no X server** |
| Images (png/jpg/jpeg/webp) | native decode (pure-Rust `image` crate), fast integer scaler, direct framebuffer write |
| Animated GIF | native frame-paced decoding with disposal compositing |
| Video (H.264 in mp4/mov) | native MP4 demux + **V4L2 M2M hardware decode** (`/dev/video*`: bcm2835-codec on Pi ≤ 4, rpivid on Pi 5) -> NV12 -> fused colorspace+scale -> display |
| Idle/pairing screen | rendered natively with an embedded 8x8 bitmap font |
| URL/web-page items | skipped by default; optional external `--url-command` (e.g. kiosk chromium) |

Playlist semantics match `player.py` exactly: 4-char pairing codes (persisted
to the code file), poll-based updates, gapless transitions (the next item is
prefetched, decoded and scaled while the current one is still on screen),
single-item looping, and `--unlink`.

Other video containers (webm/mkv/avi/ogg) are skipped natively — use the
operator console's "transcode" button (server-side ffmpeg to H.264) for
those, same as for non-H.264 codecs.

### Quick install (Raspbian Lite)

```bash
sudo bash install.sh
```

This installs Python/Flask + ffmpeg, builds the native player with rustup,
deploys everything to `/opt/fossignage`, and starts on boot in **standalone
mode**:

- `fossignage-server.service` — Flask backend on port 5000 (`--standalone`)
- getty autologin on tty1 launches `bin/fossignage-player --standalone`
  directly on the console (DRM/KMS, no X)

In standalone mode there is no display linking (like old Screenly OSE): the
idle screen shows the server's address, and you upload media / build the
playlist directly on the operator console at `http://<pi-ip>:5000/operator`.
Playback starts automatically.

Set `FOSSIGNAGE_WITH_BROWSER=1` to additionally install X + chromium so that
`url` playlist items can be shown (passed to the player as `--url-command`).

### Manual run

```bash
# Standalone (no linking; player shows the server address when idle)
python3 app.py --standalone &
player-rs/target/release/fossignage-player --standalone --server http://127.0.0.1:5000

# Classic client/server (pairing via 4-char codes)
python3 app.py &
player-rs/target/release/fossignage-player --server http://127.0.0.1:5000

# The old Python player (shells out to omxplayer/vlc/feh/...) still works:
python3 player.py --server http://127.0.0.1:5000
```

`--standalone` can also be enabled via the `SIGNAGE_STANDALONE=1` environment
variable on the server.

Player CLI (see `fossignage-player --help`):

```
--backend auto|drm|fb|headless   output backend (default auto: drm -> fb)
--video auto|none                V4L2 hw decode on/off (default auto)
--url-command CMD                render url items with CMD ({url} placeholder)
--server/--code-file/--poll-interval/--standalone/--unlink/--debug ...
```

### Building and testing the native player

```bash
cd player-rs
cargo build --release          # binary at target/release/fossignage-player
cargo test                     # 48 unit tests + 10 integration tests (mock backend)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cd .. && python3 tests/e2e_player_rs.py   # real Flask backend + player, loopback
```

The test suite covers: HTTP/JSON client, media classification, download cache
(path-traversal safe), RGBA scaling, NV12->RGB conversion, bitmap-font idle
card, PNG/JPEG/GIF decode, MP4 demux + H.264 SPS parsing (real ffmpeg
fixtures), DRM/V4L2/fb ioctl numbers and struct layouts (verified against the
kernel UAPI headers), and full player-loop integration tests against a mock
backend plus an end-to-end run against the real Flask server.

To forget the pairing and re-pair: `rm /opt/fossignage/display_code &&
sudo systemctl restart getty@tty1`.
