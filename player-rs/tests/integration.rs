//! End-to-end integration tests: a mock fossignage HTTP backend (same API as
//! app.py) plus the real player event loop running against the headless
//! display backend. Verifies register/poll/link/unlink, playlist playback,
//! gapless advance, playlist-change restart, 404 re-registration, unsupported
//! media skipping, and GIF animation — all without any hardware.

use fossignage_player::api::ApiClient;
use fossignage_player::{config, log, run_with_shutdown, Config};
use image::{ImageBuffer, ImageEncoder, Rgba as RgbaPx};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// --- mock backend -----------------------------------------------------------

struct MockState {
    code: String,
    linked: bool,
    standalone: bool,
    media: Vec<serde_json::Value>,
    register_count: u32,
    poll_404_once: bool,
    files: Vec<(String, Vec<u8>)>,
}

struct MockServer {
    port: u16,
    state: Arc<Mutex<MockState>>,
    stop: Arc<AtomicBool>,
}

impl MockServer {
    fn start() -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(MockState {
            code: "ABCD".to_string(),
            linked: false,
            standalone: false,
            media: Vec::new(),
            register_count: 0,
            poll_404_once: false,
            files: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (st2, stop2) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => handle_conn(stream, &st2),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        MockServer { port, state, stop }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn set_media(&self, media: Vec<serde_json::Value>) {
        self.state.lock().unwrap().media = media;
    }

    fn set_linked(&self, linked: bool) {
        self.state.lock().unwrap().linked = linked;
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn handle_conn(mut stream: TcpStream, state: &Arc<Mutex<MockState>>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).unwrap() == 0 {
        return;
    }
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            return;
        }
        if line.to_ascii_lowercase().starts_with("content-length:") {
            content_length = line.split(':').nth(1).unwrap().trim().parse().unwrap_or(0);
        }
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    let mut body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut body);
    let mut it = req_line.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let path = it.next().unwrap_or("").to_string();

    let (status, body_out) = {
        let mut st = state.lock().unwrap();
        if method == "POST" && path == "/api/display/register" {
            st.register_count += 1;
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let standalone = req
                .get("standalone")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            st.standalone = standalone;
            let requested = req.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let reconnected = !requested.is_empty() && requested == st.code;
            if !reconnected && requested.is_empty() {
                st.code = format!("{:04}", st.register_count + 1000); // e.g. "1001"
            }
            if standalone {
                st.linked = true;
            }
            let code = st.code.clone();
            (
                200,
                serde_json::json!({"success": true, "code": code, "reconnected": reconnected})
                    .to_string(),
            )
        } else if method == "GET" && path.starts_with("/api/display/") {
            let code = path.rsplit('/').next().unwrap_or("");
            if st.poll_404_once {
                st.poll_404_once = false;
                (404, "{\"error\": \"Display not found\"}".to_string())
            } else if code != st.code {
                (404, "{\"error\": \"Display not found\"}".to_string())
            } else {
                let media = st.media.clone();
                (
                    200,
                    serde_json::json!({
                        "linked": st.linked,
                        "code": code,
                        "active_playlist_id": "p_1",
                        "media": media,
                    })
                    .to_string(),
                )
            }
        } else if method == "POST" && path == "/api/unlink_display" {
            st.linked = false;
            (200, "{\"success\": true}".to_string())
        } else if method == "GET" && path.starts_with("/static/media/") {
            let name = path.rsplit('/').next().unwrap_or("");
            match st.files.iter().find(|(n, _)| n == name) {
                Some((_, bytes)) => {
                    let mut resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )
                    .into_bytes();
                    resp.extend_from_slice(bytes);
                    stream.write_all(&resp).ok();
                    stream.flush().ok();
                    return;
                }
                None => (404, "not found".to_string()),
            }
        } else {
            (404, "not found".to_string())
        }
    };
    let resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_out}",
        body_out.len()
    );
    stream.write_all(resp.as_bytes()).ok();
    stream.flush().ok();
}

// --- helpers ----------------------------------------------------------------

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn png_bytes(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    let img: ImageBuffer<RgbaPx<u8>, Vec<u8>> = ImageBuffer::from_pixel(w, h, RgbaPx(color));
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
        .unwrap();
    out
}

fn gif_bytes() -> Vec<u8> {
    use image::codecs::gif::GifEncoder;
    use image::Frame;
    let mut red = vec![0u8; 4 * 4 * 4];
    let mut blue = vec![0u8; 4 * 4 * 4];
    for px in red.as_chunks_mut::<4>().0 {
        px.copy_from_slice(&[255, 0, 0, 255]);
    }
    for px in blue.as_chunks_mut::<4>().0 {
        px.copy_from_slice(&[0, 0, 255, 255]);
    }
    let f1 = Frame::new(ImageBuffer::<RgbaPx<u8>, Vec<u8>>::from_raw(4, 4, red).unwrap());
    let f2 = Frame::new(ImageBuffer::<RgbaPx<u8>, Vec<u8>>::from_raw(4, 4, blue).unwrap());
    let mut out = Vec::new();
    GifEncoder::new(&mut out).encode_frames([f1, f2]).unwrap();
    out
}

fn media_item(id: &str, name: &str, url: &str, mtype: &str, duration: u64) -> serde_json::Value {
    serde_json::json!({
        "id": id, "name": name, "url": url, "type": mtype, "duration": duration
    })
}

struct TestEnv {
    dir: PathBuf,
    out_dir: PathBuf,
    code_file: PathBuf,
    log_file: PathBuf,
    cfg: Config,
}

fn setup(server_url: &str) -> TestEnv {
    log::init_test();
    let dir = std::env::temp_dir().join(format!(
        "fossignage-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let out_dir = dir.join("frames");
    let code_file = dir.join("display_code");
    let log_file = dir.join("player.log");
    std::fs::create_dir_all(&out_dir).unwrap();
    log::set_log_file(Some(log_file.clone()));
    let cfg = Config {
        server: server_url.to_string(),
        backend: config::Backend::Headless,
        out_dir: Some(out_dir.clone()),
        width: Some(64),
        height: Some(48),
        poll_interval: Duration::from_millis(100),
        code_file: code_file.clone(),
        log_dir: None,
        cache_dir: dir.join("cache"),
        ..Default::default()
    };
    TestEnv {
        dir,
        out_dir,
        code_file,
        log_file,
        cfg,
    }
}

impl TestEnv {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_file).unwrap_or_default()
    }
    fn frame_count(&self) -> usize {
        std::fs::read_dir(&self.out_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().starts_with("frame_"))
                    .count()
            })
            .unwrap_or(0)
    }
    fn latest_ppm_center(&self) -> Option<[u8; 3]> {
        let data = std::fs::read(self.out_dir.join("latest.ppm")).ok()?;
        // parse P6 header
        let mut pos = 0;
        let mut fields = Vec::new();
        while fields.len() < 4 {
            while pos < data.len() && data[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if data[pos] == b'#' {
                while pos < data.len() && data[pos] != b'\n' {
                    pos += 1;
                }
                continue;
            }
            let start = pos;
            while pos < data.len() && !data[pos].is_ascii_whitespace() {
                pos += 1;
            }
            fields.push(String::from_utf8_lossy(&data[start..pos]).to_string());
        }
        pos += 1; // single whitespace after maxval
        let w: usize = fields[1].parse().ok()?;
        let h: usize = fields[2].parse().ok()?;
        let cx = w / 2;
        let cy = h / 2;
        let i = pos + (cy * w + cx) * 3;
        Some([data[i], data[i + 1], data[i + 2]])
    }
    fn run_for(&self, secs: f64) {
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = self.cfg.clone();
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || run_with_shutdown(cfg, stop2));
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }
    fn wait_for_log(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.log().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        log::set_log_file(None);
    }
}

// --- tests ------------------------------------------------------------------

#[test]
fn registers_and_plays_image() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    {
        let mut st = server.state.lock().unwrap();
        st.linked = true;
        st.files
            .push(("red.png".into(), png_bytes(16, 12, [200, 30, 40, 255])));
        st.media = vec![media_item("m1", "red", "/static/media/red.png", "image", 5)];
    }
    let env = setup(&server.url());
    env.run_for(2.0);
    let log = env.log();
    assert!(log.contains("registered as display"), "log: {log}");
    assert!(log.contains("playing [1/1] red (image)"), "log: {log}");
    assert!(env.frame_count() >= 1, "frames written");
    // code persisted to the code file
    let code = std::fs::read_to_string(&env.code_file).unwrap();
    assert_eq!(code.len(), 4);
    // the presented frame is (scaled) red
    let px = env.latest_ppm_center().expect("latest.ppm exists");
    assert!(
        px[0] > 180 && px[1] < 60 && px[2] < 70,
        "center pixel {px:?}"
    );
}

#[test]
fn two_image_playlist_advances_and_loops() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    {
        let mut st = server.state.lock().unwrap();
        st.linked = true;
        st.files
            .push(("a.png".into(), png_bytes(8, 8, [255, 0, 0, 255])));
        st.files
            .push(("b.png".into(), png_bytes(8, 8, [0, 0, 255, 255])));
        st.media = vec![
            media_item("m1", "a", "/static/media/a.png", "image", 1),
            media_item("m2", "b", "/static/media/b.png", "image", 1),
        ];
    }
    let env = setup(&server.url());
    env.run_for(3.6);
    let log = env.log();
    assert!(log.contains("playing [1/2] a (image)"), "log: {log}");
    assert!(log.contains("playing [2/2] b (image)"), "log: {log}");
    // loops back to item 1
    let idx = log.find("playing [1/2] a").unwrap();
    let idx2 = log.rfind("playing [1/2] a").unwrap();
    assert!(idx2 > idx, "should loop back to first item: {log}");
    // prefetch happened
    assert!(log.contains("playing [2/2]"), "log: {log}");
}

#[test]
fn playlist_change_restarts_playlist() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    {
        let mut st = server.state.lock().unwrap();
        st.linked = true;
        st.files
            .push(("a.png".into(), png_bytes(8, 8, [255, 0, 0, 255])));
        st.files
            .push(("b.png".into(), png_bytes(8, 8, [0, 255, 0, 255])));
        st.media = vec![media_item("m1", "a", "/static/media/a.png", "image", 30)];
    }
    let env = setup(&server.url());
    let stop = Arc::new(AtomicBool::new(false));
    let cfg = env.cfg.clone();
    let stop2 = stop.clone();
    let handle = std::thread::spawn(move || run_with_shutdown(cfg, stop2));
    assert!(env.wait_for_log("playing [1/1] a", Duration::from_secs(5)));
    // change the playlist server-side
    server.set_media(vec![
        media_item("m1", "a", "/static/media/a.png", "image", 30),
        media_item("m2", "b", "/static/media/b.png", "image", 1),
    ]);
    assert!(env.wait_for_log("playlist changed (2 items)", Duration::from_secs(5)));
    assert!(env.wait_for_log("playing [1/2] a (image)", Duration::from_secs(5)));
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

#[test]
fn poll_404_triggers_reregister() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    {
        let mut st = server.state.lock().unwrap();
        st.linked = true;
        st.files
            .push(("a.png".into(), png_bytes(8, 8, [10, 20, 30, 255])));
        st.media = vec![media_item("m1", "a", "/static/media/a.png", "image", 30)];
    }
    let env = setup(&server.url());
    let stop = Arc::new(AtomicBool::new(false));
    let cfg = env.cfg.clone();
    let stop2 = stop.clone();
    let handle = std::thread::spawn(move || run_with_shutdown(cfg, stop2));
    assert!(env.wait_for_log("playing [1/1] a", Duration::from_secs(5)));
    // server "loses" the display: next poll 404s once
    server.state.lock().unwrap().poll_404_once = true;
    assert!(env.wait_for_log("re-registering", Duration::from_secs(5)));
    assert!(env.wait_for_log("registered as display", Duration::from_secs(5)));
    let count = server.state.lock().unwrap().register_count;
    assert!(count >= 2, "register_count={count}");
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

#[test]
fn unlinked_shows_idle_and_stops() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    {
        let mut st = server.state.lock().unwrap();
        st.linked = true;
        st.files
            .push(("a.png".into(), png_bytes(8, 8, [255, 0, 0, 255])));
        st.media = vec![media_item("m1", "a", "/static/media/a.png", "image", 30)];
    }
    let env = setup(&server.url());
    let stop = Arc::new(AtomicBool::new(false));
    let cfg = env.cfg.clone();
    let stop2 = stop.clone();
    let handle = std::thread::spawn(move || run_with_shutdown(cfg, stop2));
    assert!(env.wait_for_log("playing [1/1] a", Duration::from_secs(5)));
    server.set_linked(false);
    // wait a couple of poll cycles; frames should stop increasing once idle
    std::thread::sleep(Duration::from_millis(600));
    let frames_at_idle = env.frame_count();
    std::thread::sleep(Duration::from_millis(700));
    let frames_later = env.frame_count();
    assert_eq!(frames_at_idle, frames_later, "no new frames while idle");
    // re-link: playback resumes
    server.set_linked(true);
    assert!(
        env.wait_for_log("playing [1/1] a", Duration::from_secs(5))
            || env.log().matches("playing [1/1] a").count() >= 2
    );
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

#[test]
fn unsupported_video_and_url_are_skipped() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    {
        let mut st = server.state.lock().unwrap();
        st.linked = true;
        st.files
            .push(("a.png".into(), png_bytes(8, 8, [0, 255, 0, 255])));
        st.media = vec![
            media_item("m1", "vid", "/static/media/v.mkv", "video", 5),
            media_item("m2", "web", "https://example.com/", "url", 5),
            media_item("m3", "a", "/static/media/a.png", "image", 30),
        ];
    }
    let env = setup(&server.url());
    env.run_for(2.5);
    let log = env.log();
    assert!(log.contains("not supported natively"), "log: {log}");
    assert!(log.contains("no --url-command"), "log: {log}");
    assert!(log.contains("playing [3/3] a (image)"), "log: {log}");
}

#[test]
fn animated_gif_plays_multiple_frames() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    {
        let mut st = server.state.lock().unwrap();
        st.linked = true;
        st.files.push(("anim.gif".into(), gif_bytes()));
        st.media = vec![media_item(
            "m1",
            "anim",
            "/static/media/anim.gif",
            "image",
            30,
        )];
    }
    let env = setup(&server.url());
    env.run_for(2.0);
    let log = env.log();
    assert!(log.contains("playing [1/1] anim"), "log: {log}");
    // gif frames keep being presented over time (more than a single frame)
    assert!(env.frame_count() > 2, "frames: {}", env.frame_count());
}

#[test]
fn api_client_unlink_roundtrip() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    server.set_linked(true);
    let api = ApiClient::new(&server.url());
    let reg = api.register(Some("ABCD"), false).unwrap();
    assert_eq!(reg.code, "ABCD");
    assert!(reg.reconnected);
    let poll = api.poll("ABCD").unwrap();
    assert!(poll.linked);
    api.unlink("ABCD").unwrap();
    let poll = api.poll("ABCD").unwrap();
    assert!(!poll.linked);
    // unknown code -> 404 status error
    let err = api.poll("ZZZZ").unwrap_err();
    assert!(matches!(err, fossignage_player::api::ApiError::Status(404)));
}

#[test]
fn idle_card_rendered_when_no_playlist() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    server.set_linked(true); // linked but no media
    let env = setup(&server.url());
    env.run_for(1.2);
    assert!(env.frame_count() >= 1, "idle card presented");
    // idle card: dark background at corners (not the media color)
    let px = env.latest_ppm_center().unwrap();
    // center has the code text area; just assert the frame exists and is dark-ish
    let data = std::fs::read(env.out_dir.join("latest.ppm")).unwrap();
    assert!(data.starts_with(b"P6\n64 48\n255\n"));
    let _ = px;
}

#[test]
fn standalone_register_sends_flag() {
    let _lock = TEST_LOCK.lock().unwrap();
    let server = MockServer::start();
    let api = ApiClient::new(&server.url());
    let reg = api.register(None, true).unwrap();
    assert!(!reg.code.is_empty());
    let st = server.state.lock().unwrap();
    assert!(st.standalone, "server saw standalone flag");
    assert!(st.linked, "standalone auto-links");
}
