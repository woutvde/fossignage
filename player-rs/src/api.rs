//! Client for the fossignage backend REST API (same endpoints as player.py).

use crate::http;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum ApiError {
    Http(String),
    Status(u16),
    Json(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Http(e) => write!(f, "http: {e}"),
            ApiError::Status(s) => write!(f, "HTTP {s}"),
            ApiError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Serialize)]
struct RegisterReq<'a> {
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    standalone: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct RegisterResp {
    pub code: String,
    #[serde(default)]
    pub reconnected: bool,
}

/// Result of a poll call, as forwarded to the player loop.
pub type PollResult = Result<PollResp, ApiError>;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MediaItem {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub url: String,
    #[serde(rename = "type")]
    pub mtype: String,
    #[serde(default = "default_duration")]
    pub duration: u64,
}

fn default_duration() -> u64 {
    8
}

#[derive(Debug, Clone, Deserialize)]
pub struct PollResp {
    pub linked: bool,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub active_playlist_id: Option<String>,
    #[serde(default)]
    pub media: Vec<MediaItem>,
}

pub struct ApiClient {
    pub server: String,
}

impl ApiClient {
    pub fn new(server: &str) -> Self {
        ApiClient {
            server: server.trim_end_matches('/').to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.server, path)
    }

    fn parse<T: for<'de> Deserialize<'de>>(resp: http::Response) -> Result<T, ApiError> {
        if resp.status == 404 {
            return Err(ApiError::Status(404));
        }
        if !(200..300).contains(&resp.status) {
            return Err(ApiError::Status(resp.status));
        }
        serde_json::from_slice(&resp.body).map_err(|e| ApiError::Json(e.to_string()))
    }

    pub fn register(&self, code: Option<&str>, standalone: bool) -> Result<RegisterResp, ApiError> {
        let req = RegisterReq {
            code: code.unwrap_or(""),
            standalone: standalone.then_some(true),
        };
        let body = serde_json::to_string(&req).map_err(|e| ApiError::Json(e.to_string()))?;
        let resp = http::post_json(&self.url("/api/display/register"), &body, TIMEOUT)
            .map_err(ApiError::Http)?;
        Self::parse(resp)
    }

    pub fn poll(&self, code: &str) -> Result<PollResp, ApiError> {
        let resp = http::get(&self.url(&format!("/api/display/{code}")), TIMEOUT)
            .map_err(ApiError::Http)?;
        Self::parse(resp)
    }

    pub fn unlink(&self, code: &str) -> Result<(), ApiError> {
        let body = serde_json::to_string(&serde_json::json!({"code": code}))
            .map_err(|e| ApiError::Json(e.to_string()))?;
        let resp = http::post_json(&self.url("/api/unlink_display"), &body, TIMEOUT)
            .map_err(ApiError::Http)?;
        if !(200..300).contains(&resp.status) {
            return Err(ApiError::Status(resp.status));
        }
        Ok(())
    }

    /// Absolute URL for a media item (server-relative or absolute).
    pub fn media_url(&self, item_url: &str) -> String {
        if item_url.starts_with("http://") || item_url.starts_with("https://") {
            item_url.to_string()
        } else {
            self.url(item_url)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_resp_parses_full_payload() {
        let raw = r#"{
            "linked": true,
            "code": "ABCD",
            "active_playlist_id": "p_1",
            "media": [
                {"id": "m_1", "name": "a", "url": "/static/media/a.png", "type": "image", "duration": 5},
                {"id": "m_2", "name": "b", "url": "http://x/v.mp4", "type": "video"}
            ]
        }"#;
        let resp: PollResp = serde_json::from_str(raw).unwrap();
        assert!(resp.linked);
        assert_eq!(resp.media.len(), 2);
        assert_eq!(resp.media[0].duration, 5);
        assert_eq!(resp.media[0].mtype, "image");
        // duration defaults to 8 when missing (player.py: `or 8`)
        assert_eq!(resp.media[1].duration, 8);
    }

    #[test]
    fn poll_resp_tolerates_missing_fields() {
        let resp: PollResp = serde_json::from_str(r#"{"linked": false}"#).unwrap();
        assert!(!resp.linked);
        assert!(resp.media.is_empty());
        assert!(resp.active_playlist_id.is_none());
    }

    #[test]
    fn register_req_omits_standalone_when_false() {
        let req = RegisterReq {
            code: "",
            standalone: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v, serde_json::json!({"code": ""}));
        let req = RegisterReq {
            code: "ABCD",
            standalone: Some(true),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v, serde_json::json!({"code": "ABCD", "standalone": true}));
    }

    #[test]
    fn media_url_joins_relative_paths() {
        let api = ApiClient::new("http://127.0.0.1:5000/");
        assert_eq!(
            api.media_url("/static/media/x.png"),
            "http://127.0.0.1:5000/static/media/x.png"
        );
        assert_eq!(
            api.media_url("https://cdn.example.com/a.jpg"),
            "https://cdn.example.com/a.jpg"
        );
    }
}
