//! Command-line / environment configuration. Mirrors player.py's flags and
//! adds backend selection for the native renderer.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Drm,
    Fb,
    Headless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    Auto,
    None,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server: String,
    pub code_file: PathBuf,
    pub poll_interval: Duration,
    pub standalone: bool,
    pub unlink: bool,
    pub debug: bool,
    pub log_dir: Option<PathBuf>,
    pub backend: Backend,
    /// Force a display size (mainly for the headless backend / tests).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Headless backend: directory to write presented frames into.
    pub out_dir: Option<PathBuf>,
    pub video: VideoMode,
    /// Optional external command for `url` items; `{url}` is replaced by the
    /// item URL. Native rendering of web pages is out of scope.
    pub url_command: Option<String>,
    pub prefetch: bool,
    pub cache_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let server = env::var("SIGNAGE_SERVER").unwrap_or_else(|_| "http://127.0.0.1:5000".into());
        let code_file = env::var("SIGNAGE_CODE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                env::var("HOME")
                    .map(|h| PathBuf::from(h).join(".fossignage_display_code"))
                    .unwrap_or_else(|_| PathBuf::from("/tmp/fossignage_display_code"))
            });
        let log_dir = env::var("SIGNAGE_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/var/log/fossignage"));
        Config {
            server: server.trim_end_matches('/').to_string(),
            code_file,
            poll_interval: Duration::from_secs_f64(2.0),
            standalone: env::var("SIGNAGE_STANDALONE")
                .map(|v| v == "1")
                .unwrap_or(false),
            unlink: false,
            debug: false,
            log_dir: Some(log_dir),
            backend: Backend::Auto,
            width: None,
            height: None,
            out_dir: None,
            video: VideoMode::Auto,
            url_command: None,
            prefetch: true,
            cache_dir: env::temp_dir().join("fossignage-player-cache"),
        }
    }
}

fn parse_backend(s: &str) -> Result<Backend, String> {
    match s.to_ascii_lowercase().as_str() {
        "auto" => Ok(Backend::Auto),
        "drm" => Ok(Backend::Drm),
        "fb" | "framebuffer" => Ok(Backend::Fb),
        "headless" => Ok(Backend::Headless),
        other => Err(format!("unknown backend '{other}' (auto|drm|fb|headless)")),
    }
}

fn parse_video(s: &str) -> Result<VideoMode, String> {
    match s.to_ascii_lowercase().as_str() {
        "auto" | "v4l2" => Ok(VideoMode::Auto),
        "none" | "off" => Ok(VideoMode::None),
        other => Err(format!("unknown video mode '{other}' (auto|none)")),
    }
}

/// Parses CLI args (without the program name) into `cfg`. Exits with a usage
/// message on error, like argparse.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut take = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--server" => cfg.server = take("--server")?.trim_end_matches('/').to_string(),
            "--code-file" => cfg.code_file = PathBuf::from(take("--code-file")?),
            "--poll-interval" => {
                cfg.poll_interval = Duration::from_secs_f64(
                    take("--poll-interval")?
                        .parse()
                        .map_err(|_| "--poll-interval must be a number of seconds")?,
                )
            }
            "--standalone" => cfg.standalone = true,
            "--unlink" => cfg.unlink = true,
            "--debug" => cfg.debug = true,
            "--log-dir" => cfg.log_dir = Some(PathBuf::from(take("--log-dir")?)),
            "--no-log-file" => cfg.log_dir = None,
            "--backend" => cfg.backend = parse_backend(&take("--backend")?)?,
            "--width" => {
                cfg.width = Some(
                    take("--width")?
                        .parse()
                        .map_err(|_| "--width must be an integer")?,
                )
            }
            "--height" => {
                cfg.height = Some(
                    take("--height")?
                        .parse()
                        .map_err(|_| "--height must be an integer")?,
                )
            }
            "--out-dir" => cfg.out_dir = Some(PathBuf::from(take("--out-dir")?)),
            "--video" => cfg.video = parse_video(&take("--video")?)?,
            "--url-command" => cfg.url_command = Some(take("--url-command")?),
            "--no-prefetch" => cfg.prefetch = false,
            "--cache-dir" => cfg.cache_dir = PathBuf::from(take("--cache-dir")?),
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown argument '{other}'\n\n{}", usage())),
        }
    }
    if cfg.backend == Backend::Headless && cfg.out_dir.is_none() {
        return Err("--backend headless requires --out-dir DIR".into());
    }
    if (cfg.width.is_some()) != (cfg.height.is_some()) {
        return Err("--width and --height must be given together".into());
    }
    Ok(cfg)
}

pub fn usage() -> String {
    "Usage: fossignage-player [OPTIONS]\n\
     \n\
     Native fossignage display player (no external player processes).\n\
     \n\
     Options:\n\
     \x20 --server URL          backend base URL (default http://127.0.0.1:5000)\n\
     \x20 --code-file PATH      pairing code persistence file\n\
     \x20 --poll-interval SEC   poll interval in seconds (default 2.0)\n\
     \x20 --standalone          standalone mode (idle screen shows server address)\n\
     \x20 --unlink              unlink this display from the server and exit\n\
     \x20 --debug               verbose logging\n\
     \x20 --log-dir DIR         log file directory (default /var/log/fossignage)\n\
     \x20 --no-log-file         do not write a log file\n\
     \x20 --backend B           auto|drm|fb|headless (default auto)\n\
     \x20 --width N --height N  force display size (headless/tests)\n\
     \x20 --out-dir DIR         headless backend: write frames here\n\
     \x20 --video MODE          auto (V4L2 hw decode) | none\n\
     \x20 --url-command CMD     render url items with CMD ({url} placeholder)\n\
     \x20 --no-prefetch         disable next-image prefetch\n\
     \x20 --cache-dir DIR       download cache directory\n\
     \x20 -h, --help            show this help\n"
        .to_string()
}
