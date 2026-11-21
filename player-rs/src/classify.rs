//! Media classification, mirroring player.py's rules.

pub const IMAGE_EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
pub const ANIMATED_EXTS: [&str; 1] = ["gif"];
pub const VIDEO_EXTS: [&str; 6] = ["mp4", "webm", "ogg", "mov", "mkv", "avi"];
/// Containers the native demuxer understands (ISO base media file format).
pub const NATIVE_VIDEO_EXTS: [&str; 2] = ["mp4", "mov"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Animated GIF: played frame by frame natively.
    Gif,
    /// H.264 in an ISO-BMFF container: native demux + V4L2 hw decode.
    VideoH264,
    /// Video in a container we cannot demux natively (webm/mkv/avi/ogg...).
    VideoUnsupported,
    Image,
    Url,
}

pub fn extension_of(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/')
        .next()
        .unwrap_or("")
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn classify(item_type: &str, url: &str) -> Kind {
    let ext = extension_of(url);
    let is_video_ext = VIDEO_EXTS.contains(&ext.as_str()) || ANIMATED_EXTS.contains(&ext.as_str());
    if item_type == "video" || is_video_ext {
        if ANIMATED_EXTS.contains(&ext.as_str()) {
            return Kind::Gif;
        }
        if NATIVE_VIDEO_EXTS.contains(&ext.as_str()) || ext.is_empty() {
            // mp4/mov (or an extension-less URL marked as video): try the
            // native H.264 path; demux failure degrades to a skip at runtime.
            return Kind::VideoH264;
        }
        return Kind::VideoUnsupported;
    }
    if IMAGE_EXTS.contains(&ext.as_str()) {
        return Kind::Image;
    }
    Kind::Url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_parsing() {
        assert_eq!(extension_of("/static/media/a.PNG"), "png");
        assert_eq!(extension_of("http://x/y.MP4?dl=1"), "mp4");
        assert_eq!(extension_of("http://x/noext"), "");
        assert_eq!(extension_of("http://x/"), "");
    }

    #[test]
    fn classification_matches_player_py() {
        // player.py: type=="video" or video/animated ext -> video path
        assert_eq!(classify("video", "/static/media/a.mp4"), Kind::VideoH264);
        assert_eq!(
            classify("video", "/static/media/a.webm"),
            Kind::VideoUnsupported
        );
        assert_eq!(
            classify("video", "/static/media/a.mkv"),
            Kind::VideoUnsupported
        );
        assert_eq!(classify("image", "/static/media/a.gif"), Kind::Gif);
        assert_eq!(classify("video", "/static/media/a.gif"), Kind::Gif);
        assert_eq!(classify("image", "/static/media/a.png"), Kind::Image);
        assert_eq!(classify("image", "/static/media/a.webp"), Kind::Image);
        assert_eq!(classify("url", "https://example.com/"), Kind::Url);
        assert_eq!(
            classify("video", "https://example.com/stream"),
            Kind::VideoH264
        );
        assert_eq!(classify("", "/static/media/x.jpeg"), Kind::Image);
    }
}
