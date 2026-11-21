# fossignage-player (native Rust player)

Drop-in native replacement for `player.py`: same REST API, same CLI shape
(`--server`, `--standalone`, `--code-file`, `--poll-interval`, `--unlink`,
`--debug`, `--log-dir`), but renders everything itself — no mpv/vlc/omxplayer,
no feh/fbi/ImageMagick, no X server.

## Layout

- `src/main.rs` — CLI entry point
- `src/player.rs` — event loop / playlist state machine (mirrors player.py)
- `src/http.rs`, `src/api.rs` — minimal std-only HTTP/1.1 client + JSON API
- `src/cache.rs` — media download cache (path-traversal-safe)
- `src/decode.rs` — native PNG/JPEG/WebP/GIF decoding
- `src/frame.rs` — RGBA buffers, integer scaler, fused NV12/I420→RGB+scale
- `src/idle.rs`, `src/text.rs`, `src/font8x8.rs` — native idle/pairing card
- `src/mp4.rs`, `src/h264.rs` — ISO-BMFF demux + AVCC/Annex-B + SPS parsing
- `src/v4l2dec.rs` — V4L2 M2M hardware H.264 decoder (Linux)
- `src/video.rs` — media workers (image/gif/video/url), gapless prefetch
- `src/display/` — backends: `drm.rs` (KMS, page flip), `fb.rs` (/dev/fb0),
  `headless.rs` (writes PPM frames; used by tests)

## Build & test

```bash
cargo build --release
cargo test                          # unit + integration (mock server, headless)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

See the top-level README for the end-to-end test
(`tests/e2e_player_rs.py`, real Flask backend on loopback).

## Notes for Pi

- Video: H.264 in mp4/mov is decoded by the SoC via V4L2 M2M
  (`/dev/video*`); other containers/codecs should be transcoded server-side
  to H.264 (the operator console has a transcode button).
- Run as a user in the `video` group for `/dev/dri`, `/dev/fb0`, `/dev/video*`.
- `--backend headless --out-dir DIR --width W --height H` renders to PPM
  files instead of hardware (used by CI/tests).
