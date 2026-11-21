//! The player event loop: one thread renders, one polls the server, workers
//! decode media. Playlist semantics mirror player.py:
//! - poll /api/display/<code>; 404 -> re-register
//! - playlist changes (by signature) restart at item 0
//! - single-item playlists loop (video/gif loop in the worker, images re-arm)
//! - the next image is prefetched (download+decode+scale) while the current
//!   item is on screen, so swaps are gapless

use crate::api::{ApiClient, ApiError, MediaItem, PollResp};
use crate::cache::MediaCache;
use crate::classify::{self, Kind};
use crate::config::Config;
use crate::display::{self, Display};
use crate::frame::{scale_rgba, yuv_to_rgba_scaled, Rgba, YuvFrame};
use crate::idle;
use crate::video::{self, Event};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct PlayerError(pub String);

impl std::fmt::Display for PlayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for PlayerError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Ready,
    Playing,
}

enum Current {
    /// A still image on screen until `deadline`.
    Image { deadline: Instant },
    /// Image decode in flight; previous content stays on screen.
    PendingImage { token: u64 },
    /// Video streaming; `pending` is the next frame (YUV + pts) to show.
    Media {
        token: u64,
        stream_start: Option<Instant>,
        pending: Option<(YuvFrame, Duration)>,
    },
    /// External command (url item) running until `deadline`.
    Url { token: u64, deadline: Instant },
}

pub struct Player {
    cfg: Config,
    api: Arc<ApiClient>,
    cache: Arc<MediaCache>,
    display: Box<dyn Display>,
    rx: Receiver<Event>,
    tx: Sender<Event>,
    code: Option<String>,
    state: State,
    items: Vec<MediaItem>,
    index: usize,
    signature: Option<String>,
    current: Option<Current>,
    prefetched: HashMap<String, Rgba>,
    token: u64,
    current_token: Arc<AtomicU64>,
    size: (u32, u32),
}

fn load_code(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim().to_uppercase();
    if s.len() == 4 {
        Some(s)
    } else {
        None
    }
}

fn save_code(path: &Path, code: &str) {
    if let Err(e) = std::fs::write(path, code) {
        crate::log!("could not persist code: {e}");
    }
}

/// Best-effort LAN IP (no packets sent; just picks the outgoing route).
fn lan_ip() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

fn display_addr(server: &str) -> String {
    // Replace loopback hosts with the LAN IP so the address is usable from
    // another machine (same as player.py).
    if let Ok(u) = crate::http::parse_url(server) {
        if matches!(
            u.host.as_str(),
            "127.0.0.1" | "localhost" | "0.0.0.0" | "::1"
        ) {
            return format!("http://{}:{}", lan_ip(), u.port);
        }
    }
    server.to_string()
}

fn signature_of(resp: &PollResp) -> String {
    serde_json::to_string(&serde_json::json!([resp.active_playlist_id, resp.media]))
        .unwrap_or_default()
}

impl Player {
    pub fn new(cfg: Config, display: Box<dyn Display>) -> Player {
        let (tx, rx) = channel();
        let api = Arc::new(ApiClient::new(&cfg.server));
        let cache = Arc::new(MediaCache::new(&cfg.cache_dir));
        let size = display.size();
        let code = load_code(&cfg.code_file);
        Player {
            cfg,
            api,
            cache,
            display,
            rx,
            tx,
            code,
            state: State::Idle,
            items: Vec::new(),
            index: 0,
            signature: None,
            current: None,
            prefetched: HashMap::new(),
            token: 0,
            current_token: Arc::new(AtomicU64::new(0)),
            size,
        }
    }

    fn present(&mut self, frame: &Rgba) {
        if let Err(e) = self.display.present(frame) {
            crate::log!("present failed: {e}");
        }
    }

    fn show_idle(&mut self) {
        let code = self.code.clone();
        let addr = if self.cfg.standalone {
            Some(display_addr(&self.cfg.server))
        } else {
            None
        };
        let card = idle::render_idle(self.size.0, self.size.1, code.as_deref(), addr.as_deref());
        self.present(&card);
    }

    fn set_state(&mut self, new: State) {
        if new == self.state {
            return;
        }
        self.state = new;
        if new != State::Playing {
            self.signature = None;
            self.current = None;
            self.current_token.fetch_add(1, Ordering::Relaxed); // stop workers
            self.show_idle();
        }
    }

    fn handle_poll(&mut self, resp: PollResp) {
        if !resp.linked {
            self.set_state(State::Idle);
            return;
        }
        if resp.media.is_empty() {
            self.set_state(State::Ready);
            return;
        }
        let sig = signature_of(&resp);
        if Some(sig.clone()) != self.signature {
            crate::log!("playlist changed ({} items)", resp.media.len());
            self.signature = Some(sig);
            self.items = resp.media;
            self.index = 0;
            self.prefetched.clear();
            self.cache.clear();
            self.play_current();
        } else if self.state != State::Playing {
            // same playlist, playback interrupted (e.g. restart): resume
            self.play_current();
        }
    }

    fn play_current(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let item = self.items[self.index].clone();
        let single = self.items.len() == 1;
        crate::log!(
            "playing [{}/{}] {} ({})",
            self.index + 1,
            self.items.len(),
            item.name,
            item.mtype
        );
        self.state = State::Playing;
        self.token += 1;
        let token = self.token;
        self.current_token.store(token, Ordering::Relaxed);
        let kind = classify::classify(&item.mtype, &item.url);
        let duration = Duration::from_secs(item.duration.max(1));
        match kind {
            Kind::Image => {
                if let Some(frame) = self.prefetched.remove(&item.url) {
                    self.present(&frame);
                    self.current = Some(Current::Image {
                        deadline: Instant::now() + duration,
                    });
                } else {
                    self.current = Some(Current::PendingImage { token });
                    video::spawn_item_worker(token, self.worker_spec(kind, &item.url, single));
                }
            }
            Kind::Gif | Kind::VideoH264 | Kind::VideoUnsupported => {
                self.current = Some(Current::Media {
                    token,
                    stream_start: None,
                    pending: None,
                });
                video::spawn_item_worker(token, self.worker_spec(kind, &item.url, single));
            }
            Kind::Url => {
                self.current = Some(Current::Url {
                    token,
                    deadline: Instant::now() + duration,
                });
                video::spawn_item_worker(token, self.worker_spec(kind, &item.url, single));
            }
        }
        self.prefetch_next();
    }

    fn prefetch_next(&mut self) {
        if !self.cfg.prefetch || self.items.len() < 2 {
            return;
        }
        let next = &self.items[(self.index + 1) % self.items.len()];
        let kind = classify::classify(&next.mtype, &next.url);
        if kind == Kind::Image && !self.prefetched.contains_key(&next.url) {
            video::spawn_prefetch(
                next.url.clone(),
                self.size,
                self.api.clone(),
                self.cache.clone(),
                self.tx.clone(),
            );
        }
    }

    fn worker_spec(&self, kind: Kind, url: &str, loop_play: bool) -> video::WorkerSpec {
        video::WorkerSpec {
            kind,
            url: url.to_string(),
            loop_play,
            display_size: self.size,
            api: self.api.clone(),
            cache: self.cache.clone(),
            tx: self.tx.clone(),
            current_token: self.current_token.clone(),
            video_enabled: self.cfg.video == crate::config::VideoMode::Auto,
            url_command: self.cfg.url_command.clone(),
        }
    }

    fn advance(&mut self) {
        if self.state != State::Playing || self.items.is_empty() {
            return;
        }
        if self.items.len() == 1 {
            // single-item playlist: loop in place (mirrors player.py)
            let item = self.items[0].clone();
            match &mut self.current {
                Some(Current::Image { deadline }) => {
                    *deadline = Instant::now() + Duration::from_secs(item.duration.max(1));
                }
                Some(Current::Url { deadline, .. }) => {
                    *deadline = Instant::now() + Duration::from_secs(item.duration.max(1));
                }
                _ => {} // media workers loop internally when single
            }
            return;
        }
        self.index = (self.index + 1) % self.items.len();
        self.play_current();
    }

    fn next_timeout(&self) -> Option<Duration> {
        let now = Instant::now();
        match &self.current {
            Some(Current::Image { deadline }) | Some(Current::Url { deadline, .. }) => {
                Some(deadline.saturating_duration_since(now))
            }
            Some(Current::Media {
                pending: Some((_, pts)),
                stream_start,
                ..
            }) => {
                let due = stream_start.unwrap_or(now) + *pts;
                Some(due.saturating_duration_since(now))
            }
            Some(Current::PendingImage { .. }) => Some(Duration::from_millis(100)),
            _ => None,
        }
    }

    fn present_due(&mut self) {
        let now = Instant::now();
        let due = match &self.current {
            Some(Current::Media {
                pending: Some((_, pts)),
                stream_start,
                ..
            }) => stream_start.unwrap_or(now) + *pts <= now,
            _ => false,
        };
        if !due {
            return;
        }
        let to_present = match &mut self.current {
            Some(Current::Media { pending, .. }) => pending.take(),
            _ => None,
        };
        if let Some((yuv, _)) = to_present {
            let frame = yuv_to_rgba_scaled(&yuv, self.size.0, self.size.1);
            self.present(&frame);
        }
    }

    fn on_timeout(&mut self) {
        match &self.current {
            Some(Current::Image { .. }) | Some(Current::Url { .. }) => self.advance(),
            Some(Current::Media { .. }) => self.present_due(),
            _ => {}
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Shutdown => {}
            Event::Registered { code, reconnected } => {
                if reconnected {
                    crate::log!("registered as display {code} (reconnected)");
                } else {
                    crate::log!("registered as display {code} (new pairing code)");
                }
                self.code = Some(code.clone());
                save_code(&self.cfg.code_file, &code);
            }
            Event::Poll(Ok(resp)) => self.handle_poll(resp),
            Event::Poll(Err(ApiError::Status(404))) => {
                // controller thread handles re-registration
            }
            Event::Poll(Err(e)) => {
                if self.cfg.debug {
                    crate::log!("poll error: {e}");
                }
            }
            Event::ImageReady { token, frame } => {
                let matches = matches!(&self.current, Some(Current::PendingImage { token: t }) if *t == token);
                if matches && self.state == State::Playing {
                    match frame {
                        Some(f) => {
                            self.present(&f);
                            let dur = self
                                .items
                                .get(self.index)
                                .map(|i| i.duration.max(1))
                                .unwrap_or(8);
                            self.current = Some(Current::Image {
                                deadline: Instant::now() + Duration::from_secs(dur),
                            });
                        }
                        None => {
                            crate::log!("failed to decode image; advancing");
                            self.advance();
                        }
                    }
                }
            }
            Event::GifFrame { token, frame, .. } => {
                let matches =
                    matches!(&self.current, Some(Current::Media { token: t, .. }) if *t == token);
                if matches {
                    self.present(&frame);
                }
            }
            Event::MediaFrame { token, yuv, pts } => {
                let is_current =
                    matches!(&self.current, Some(Current::Media { token: t, .. }) if *t == token);
                if is_current {
                    if let Some(Current::Media { stream_start, .. }) = &mut self.current {
                        if stream_start.is_none() {
                            *stream_start = Some(Instant::now());
                        }
                    }
                    if let Some(Current::Media { pending, .. }) = &mut self.current {
                        *pending = Some((yuv, pts));
                    }
                    self.present_due();
                }
            }
            Event::MediaEnded { token, ok } => {
                let is_current = matches!(&self.current, Some(Current::Media { token: t, .. }) if *t == token)
                    || matches!(&self.current, Some(Current::Url { token: t, .. }) if *t == token);
                if is_current {
                    if !ok {
                        crate::log!("media worker for item {} ended with an error", token);
                    }
                    self.advance();
                }
            }
            Event::Prefetched { url, frame } => {
                if self.prefetched.len() > 4 {
                    self.prefetched.clear();
                }
                self.prefetched.insert(
                    url,
                    frame.unwrap_or_else(|| Rgba::filled(self.size.0, self.size.1, [0, 0, 0])),
                );
                // keep the map tidy: only frames for current items matter
                let keep: Vec<String> = self.items.iter().map(|i| i.url.clone()).collect();
                self.prefetched.retain(|u, _| keep.contains(u));
            }
        }
    }

    /// Runs the event loop until `stop` is set (tests) — returns Ok then.
    pub fn run_loop(&mut self, stop: &AtomicBool) -> Result<(), PlayerError> {
        self.show_idle();
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            // cap the wait so the stop flag is noticed promptly even when
            // nothing is scheduled (no current item, no pending frame)
            let timeout = self
                .next_timeout()
                .unwrap_or(Duration::from_millis(200))
                .min(Duration::from_millis(200));
            let ev = self.rx.recv_timeout(timeout);
            match ev {
                Ok(Event::Shutdown) => return Ok(()),
                Ok(ev) => self.handle(ev),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => self.on_timeout(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(PlayerError("event channel closed".into()));
                }
            }
        }
    }
}

/// Spawns the controller thread (register + poll loop) and runs the player.
pub fn run_with_shutdown(cfg: Config, stop: Arc<AtomicBool>) -> Result<(), PlayerError> {
    let display = display::open(&cfg).map_err(PlayerError)?;
    let mut player = Player::new(cfg, display);
    let cfg = player.cfg.clone();
    let api = player.api.clone();
    let tx = player.tx.clone();
    let code_file = cfg.code_file.clone();
    let poll_interval = cfg.poll_interval;
    let standalone = cfg.standalone;
    let debug = cfg.debug;
    let stop2 = stop.clone();
    std::thread::spawn(move || {
        let mut code = load_code(&code_file);
        loop {
            if stop2.load(Ordering::Relaxed) {
                return;
            }
            if code.is_none() {
                match api.register(code.as_deref(), standalone) {
                    Ok(r) => {
                        let reconnected = r.reconnected;
                        code = Some(r.code.clone());
                        save_code(&code_file, &r.code);
                        let _ = tx.send(Event::Registered {
                            code: r.code,
                            reconnected,
                        });
                    }
                    Err(e) => {
                        crate::log!("server unreachable ({e}); retrying...");
                        sleep_interruptible(Duration::from_secs(3), &stop2);
                        continue;
                    }
                }
            }
            let c = code.clone().unwrap();
            match api.poll(&c) {
                Ok(resp) => {
                    let _ = tx.send(Event::Poll(Ok(resp)));
                }
                Err(ApiError::Status(404)) => {
                    crate::log!("server lost our code; re-registering");
                    code = None;
                    let _ = std::fs::remove_file(&code_file);
                    continue;
                }
                Err(e) => {
                    let _ = tx.send(Event::Poll(Err(e)));
                }
            }
            let _ = debug;
            sleep_interruptible(poll_interval, &stop2);
        }
    });
    player.run_loop(&stop).map_err(|e| {
        crate::log!("player loop ended: {e}");
        e
    })
}

fn sleep_interruptible(total: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(
            Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(unix)]
static SIGINT_STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn install_sigint() {
    unsafe extern "C" fn handler(_sig: libc::c_int) {
        SIGINT_STOP.store(true, Ordering::Relaxed);
    }
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
    }
}

/// Blocking run used by the binary: returns on SIGINT/SIGTERM.
pub fn run_forever(cfg: Config) -> Result<(), PlayerError> {
    #[cfg(unix)]
    install_sigint();
    let stop = Arc::new(AtomicBool::new(false));
    // bridge the static signal flag into `stop`
    let stop2 = stop.clone();
    std::thread::spawn(move || loop {
        #[cfg(unix)]
        if SIGINT_STOP.load(Ordering::Relaxed) {
            stop2.store(true, Ordering::Relaxed);
        }
        if stop2.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    });
    run_with_shutdown(cfg, stop)
}

// keep scale_rgba import used (idle card path uses Rgba directly)
#[allow(dead_code)]
fn _unused(f: &Rgba) -> Rgba {
    scale_rgba(f, f.w, f.h)
}
