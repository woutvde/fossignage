//! Local download cache for server-hosted media (mirrors player.py's
//! `_ensure_local`), with path-traversal-safe file naming.

use crate::api::ApiClient;
use crate::http;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

pub struct MediaCache {
    dir: PathBuf,
    map: Mutex<HashMap<String, PathBuf>>,
}

impl MediaCache {
    pub fn new(dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(dir);
        MediaCache {
            dir: dir.to_path_buf(),
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Sanitizes a URL into a safe local file name (last segment only).
    fn file_name(url: &str) -> String {
        // last path segment first, then strip any query/fragment
        let seg = url.rsplit('/').next().unwrap_or(url);
        let seg = seg.split(['?', '#']).next().unwrap_or(seg);
        let cleaned: String = seg
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let cleaned = cleaned.trim_start_matches('.').to_string();
        if cleaned.is_empty() {
            "media".to_string()
        } else {
            cleaned
        }
    }

    fn cache_path(&self, url: &str) -> PathBuf {
        // hash prefix avoids collisions between different query strings
        let mut h: u64 = 0xcbf29ce484222325;
        for b in url.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        self.dir.join(format!("{h:016x}_{}", Self::file_name(url)))
    }

    /// Returns a local path for `item_url`, downloading it first if needed.
    pub fn ensure_local(&self, api: &ApiClient, item_url: &str) -> Option<PathBuf> {
        {
            let map = self.map.lock().unwrap();
            if let Some(p) = map.get(item_url) {
                if p.exists() {
                    return Some(p.clone());
                }
            }
        }
        let full = api.media_url(item_url);
        let path = self.cache_path(item_url);
        match http::download(&full, &path, DOWNLOAD_TIMEOUT) {
            Ok(n) if n > 0 => {
                self.map
                    .lock()
                    .unwrap()
                    .insert(item_url.to_string(), path.clone());
                Some(path)
            }
            Ok(_) => {
                let _ = std::fs::remove_file(&path);
                crate::log!("download of {full} returned 0 bytes");
                None
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                crate::log!("failed to download {full}: {e}");
                None
            }
        }
    }

    pub fn invalidate(&self, item_url: &str) {
        let p = self.map.lock().unwrap().remove(item_url);
        if let Some(p) = p {
            let _ = std::fs::remove_file(p);
        }
    }

    pub fn clear(&self) {
        let map = std::mem::take(&mut *self.map.lock().unwrap());
        for (_, p) in map {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_is_safe() {
        assert_eq!(
            MediaCache::file_name("/static/media/abc_test.png"),
            "abc_test.png"
        );
        assert_eq!(MediaCache::file_name("../../etc/passwd"), "passwd");
        assert_eq!(MediaCache::file_name("a b/c.png?x=1#y"), "c.png");
        assert_eq!(MediaCache::file_name(""), "media");
        assert_eq!(MediaCache::file_name("..."), "media");
        // no path separators or leading dots may survive (traversal-safe)
        for url in ["..\\..\\evil.png", "../../etc/passwd", "a/../b.png", ".."] {
            let n = MediaCache::file_name(url);
            assert!(!n.contains('/'), "{url} -> {n}");
            assert!(!n.contains('\\'), "{url} -> {n}");
            assert!(!n.starts_with('.'), "{url} -> {n}");
        }
    }

    #[test]
    fn cache_path_stays_in_dir() {
        let c = MediaCache::new(Path::new("/tmp/fossignage-cache-test"));
        for url in [
            "/static/media/x.png",
            "../../etc/passwd",
            "http://h/a?b=../c",
        ] {
            let p = c.cache_path(url);
            assert_eq!(p.parent().unwrap(), Path::new("/tmp/fossignage-cache-test"));
            assert!(!p.to_string_lossy().contains(".."));
        }
    }
}
