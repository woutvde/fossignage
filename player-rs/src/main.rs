//! fossignage-player — native signage display player.
//! Drop-in replacement for player.py with no external player processes.

use fossignage_player::api::{ApiClient, ApiError};
use fossignage_player::{config, log, run_forever};
use std::path::Path;
use std::process::exit;

fn load_code(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim().to_uppercase();
    if s.len() == 4 {
        Some(s)
    } else {
        None
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match config::parse_args(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            exit(2);
        }
    };
    log::init(cfg.log_dir.as_deref(), true);

    let api = ApiClient::new(&cfg.server);

    if cfg.unlink {
        match load_code(&cfg.code_file) {
            Some(code) => match api.unlink(&code) {
                Ok(()) => log!("unlinked display {code}"),
                Err(ApiError::Status(s)) => log!("unlink failed: HTTP {s}"),
                Err(e) => log!("unlink failed: {e}"),
            },
            None => log!("no stored code; nothing to unlink"),
        }
        let _ = std::fs::remove_file(&cfg.code_file);
        return;
    }

    log!("starting native player -> {}", cfg.server);
    if cfg.standalone {
        log!("standalone mode (auto-linked; idle screen shows the server address)");
    }
    if let Err(e) = run_forever(cfg) {
        log!("fatal: {e}");
        exit(1);
    }
    log!("stopped");
}
