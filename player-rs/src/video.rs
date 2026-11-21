//! Media workers: background threads that decode/produce frames for the
//! current playlist item and report back through the player's event channel.
//! Keeping decode off the render thread is what makes transitions gapless —
//! the previous item stays on screen until the next one has its first frame.

use crate::api::ApiClient;
use crate::cache::MediaCache;
use crate::classify::Kind;
use crate::decode;
use crate::frame::{scale_rgba, yuv_to_rgba_scaled, Rgba, YuvFrame};
use crate::mp4;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Events workers send to the player loop.
#[derive(Debug)]
pub enum Event {
    /// Current item's still image decoded and scaled to display size.
    ImageReady {
        token: u64,
        frame: Option<Rgba>,
    },
    /// A video/GIF frame, with its presentation time relative to stream start.
    MediaFrame {
        token: u64,
        yuv: YuvFrame,
        pts: Duration,
    },
    /// GIF frame (already RGBA, scaled to display size).
    GifFrame {
        token: u64,
        frame: Rgba,
        delay: Duration,
    },
    /// The media worker finished (natural end or failure).
    MediaEnded {
        token: u64,
        ok: bool,
    },
    /// Prefetch of the next image finished (downloaded + decoded + scaled).
    Prefetched {
        url: String,
        frame: Option<Rgba>,
    },
    /// Poll result from the controller thread.
    Poll(crate::api::PollResult),
    /// (Re)registered with the server.
    Registered {
        code: String,
        reconnected: bool,
    },
    Shutdown,
}

/// Everything a media worker needs. Bundled to keep the spawn signature small.
pub struct WorkerSpec {
    pub kind: Kind,
    pub url: String,
    pub loop_play: bool,
    pub display_size: (u32, u32),
    pub api: Arc<ApiClient>,
    pub cache: Arc<MediaCache>,
    pub tx: Sender<Event>,
    pub current_token: Arc<AtomicU64>,
    pub video_enabled: bool,
    pub url_command: Option<String>,
}

/// Spawns the worker for a playlist item. `token` tags its events.
pub fn spawn_item_worker(token: u64, spec: WorkerSpec) {
    let WorkerSpec {
        kind,
        url,
        loop_play,
        display_size,
        api,
        cache,
        tx,
        current_token,
        video_enabled,
        url_command,
    } = spec;
    std::thread::spawn(move || match kind {
        Kind::Image => {
            let frame = load_scaled_image(&api, &cache, &url, display_size);
            let _ = tx.send(Event::ImageReady { token, frame });
        }
        Kind::Gif => {
            gif_worker(
                token,
                &url,
                loop_play,
                display_size,
                &api,
                &cache,
                &tx,
                &current_token,
            );
        }
        Kind::VideoH264 => {
            if !video_enabled {
                crate::log!("video playback disabled (--video none): skipping {url}");
                let _ = tx.send(Event::MediaEnded { token, ok: false });
                return;
            }
            video_worker(token, &url, loop_play, &api, &cache, &tx, &current_token);
        }
        Kind::VideoUnsupported => {
            crate::log!(
                "video container not supported natively (use mp4/mov h264 or transcode on the server): {url}"
            );
            let _ = tx.send(Event::MediaEnded { token, ok: false });
        }
        Kind::Url => match url_command {
            Some(cmd) => {
                url_worker(token, &url, &cmd, &tx, &current_token);
            }
            None => {
                crate::log!("no --url-command configured; skipping url item {url}");
                let _ = tx.send(Event::MediaEnded { token, ok: false });
            }
        },
    });
}

/// Downloads (if needed) and decodes+scales an image to display size.
pub fn load_scaled_image(
    api: &ApiClient,
    cache: &MediaCache,
    url: &str,
    display_size: (u32, u32),
) -> Option<Rgba> {
    let path = cache.ensure_local(api, url)?;
    let img = decode::decode_image(&path).ok()?;
    Some(scale_rgba(&img, display_size.0, display_size.1))
}

/// Spawns a prefetch worker for the next image (download + decode + scale).
pub fn spawn_prefetch(
    url: String,
    display_size: (u32, u32),
    api: Arc<ApiClient>,
    cache: Arc<MediaCache>,
    tx: Sender<Event>,
) {
    std::thread::spawn(move || {
        let frame = load_scaled_image(&api, &cache, &url, display_size);
        let _ = tx.send(Event::Prefetched { url, frame });
    });
}

#[allow(clippy::too_many_arguments)]
fn gif_worker(
    token: u64,
    url: &str,
    loop_play: bool,
    display_size: (u32, u32),
    api: &ApiClient,
    cache: &MediaCache,
    tx: &Sender<Event>,
    current_token: &Arc<AtomicU64>,
) {
    let stopped = || current_token.load(Ordering::Relaxed) != token;
    let Some(path) = cache.ensure_local(api, url) else {
        let _ = tx.send(Event::MediaEnded { token, ok: false });
        return;
    };
    let Ok(seq) = decode::decode_gif(&path) else {
        let _ = tx.send(Event::MediaEnded { token, ok: false });
        return;
    };
    let frames: Vec<Rgba> = seq
        .frames
        .iter()
        .map(|f| scale_rgba(f, display_size.0, display_size.1))
        .collect();
    let delays = &seq.delays;
    let n = frames.len();
    let mut i = 0;
    loop {
        for _ in 0..n {
            if stopped() {
                return;
            }
            let _ = tx.send(Event::GifFrame {
                token,
                frame: frames[i].clone(),
                delay: delays[i],
            });
            i = (i + 1) % n;
            std::thread::sleep(delays[(i + n - 1) % n]);
        }
        if !loop_play {
            break;
        }
    }
    let _ = tx.send(Event::MediaEnded { token, ok: true });
}

fn video_worker(
    token: u64,
    url: &str,
    loop_play: bool,
    api: &ApiClient,
    cache: &MediaCache,
    tx: &Sender<Event>,
    current_token: &Arc<AtomicU64>,
) {
    let stopped = || current_token.load(Ordering::Relaxed) != token;
    let Some(path) = cache.ensure_local(api, url) else {
        let _ = tx.send(Event::MediaEnded { token, ok: false });
        return;
    };
    let Ok(data) = std::fs::read(&path) else {
        let _ = tx.send(Event::MediaEnded { token, ok: false });
        return;
    };
    let mp4 = match mp4::parse(&data) {
        Ok(m) => m,
        Err(e) => {
            crate::log!("cannot demux {url}: {e}");
            let _ = tx.send(Event::MediaEnded { token, ok: false });
            return;
        }
    };
    #[cfg(target_os = "linux")]
    let dev_path = crate::v4l2dec::find_h264_decoder();
    #[cfg(not(target_os = "linux"))]
    let dev_path: Option<std::path::PathBuf> = None;
    let Some(dev_path) = dev_path else {
        crate::log!("no V4L2 M2M H.264 decoder found (/dev/video*); skipping {url}");
        let _ = tx.send(Event::MediaEnded { token, ok: false });
        return;
    };
    crate::log!(
        "decoding {} ({}x{}, {} samples) via {}",
        url,
        mp4.width,
        mp4.height,
        mp4.samples.len(),
        dev_path.display()
    );
    loop {
        let tx2 = tx.clone();
        // In loop mode the worker restarts the stream itself; only report
        // MediaEnded when the item should advance (non-loop playback).
        let send_end = !loop_play;
        let emit = move |ev: crate::v4l2dec::DecEvent| match ev {
            crate::v4l2dec::DecEvent::Frame(f) => {
                let _ = tx2.send(Event::MediaFrame {
                    token,
                    yuv: f.yuv,
                    pts: f.pts,
                });
            }
            crate::v4l2dec::DecEvent::Ended => {
                if send_end {
                    let _ = tx2.send(Event::MediaEnded { token, ok: true });
                }
            }
        };
        match crate::v4l2dec::decode_mp4_h264(&dev_path, &mp4, &data, emit, stopped) {
            Ok(()) => {
                if stopped() {
                    return;
                }
                if loop_play {
                    continue;
                }
                return; // MediaEnded(true) already sent by the decoder
            }
            Err(e) => {
                crate::log!("video decode failed for {url}: {e}");
                let _ = tx.send(Event::MediaEnded { token, ok: false });
                return;
            }
        }
    }
}

/// Runs an external command for a `url` item (e.g. a kiosk browser), then
/// reports MediaEnded after `duration`. Killed when superseded.
fn url_worker(
    token: u64,
    url: &str,
    cmd_template: &str,
    tx: &Sender<Event>,
    current_token: &Arc<AtomicU64>,
) {
    let cmd = cmd_template.replace("{url}", url);
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            crate::log!("failed to launch url command '{cmd}': {e}");
            let _ = tx.send(Event::MediaEnded { token, ok: false });
            return;
        }
    };
    // The player arms a duration timer for url items; when it fires the token
    // changes and we kill the child. Also exit if the child dies on its own.
    loop {
        if current_token.load(Ordering::Relaxed) != token {
            kill_child(&mut child);
            return;
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let _ = tx.send(Event::MediaEnded { token, ok: true });
}

fn kill_child(child: &mut std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    let _ = child.wait();
}

/// Converts a video frame to display-size RGBA at present time.
pub fn present_video_frame(yuv: &YuvFrame, display_size: (u32, u32)) -> Rgba {
    yuv_to_rgba_scaled(yuv, display_size.0, display_size.1)
}

#[allow(dead_code)]
pub fn unused_instant_marker() -> Instant {
    Instant::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_debug_works() {
        let e = Event::MediaEnded { token: 3, ok: true };
        assert!(format!("{e:?}").contains("MediaEnded"));
    }
}
