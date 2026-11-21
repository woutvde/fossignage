//! Native fossignage display player.
//!
//! Talks to the same Flask backend as the Python `player.py`, but renders
//! everything itself: no mpv/vlc/omxplayer/feh/fbi/ImageMagick/chromium
//! required for images and video.
//!
//! Rendering targets (auto-selected, see [`display`]):
//! - DRM/KMS (`/dev/dri/card*`, dumb buffers + page flip) — preferred on Pi
//! - raw framebuffer (`/dev/fb0`)
//! - headless (writes frames to disk; used by tests)
//!
//! Media:
//! - images (png/jpg/jpeg/webp): decoded natively, scaled, presented
//! - animated gif: decoded natively, frame-paced
//! - h264 in mp4/mov: demuxed natively, decoded by the V4L2 M2M hardware
//!   decoder (`/dev/video*`, e.g. bcm2835-codec on Pi <= 4, rpivid on Pi 5)
//! - url items: only via an optional external `--url-command` (a real web
//!   engine is out of scope for a native player)

pub mod api;
pub mod cache;
pub mod classify;
pub mod config;
pub mod decode;
pub mod display;
pub mod frame;
pub mod h264;
pub mod http;
pub mod idle;
pub mod log;
pub mod mp4;
pub mod player;
pub mod text;
pub mod video;

mod font8x8;

#[cfg(target_os = "linux")]
pub mod v4l2dec;

pub use config::Config;
pub use player::{run_forever, run_with_shutdown, PlayerError};
