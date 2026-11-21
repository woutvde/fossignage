//! Native image decoding (png/jpg/jpeg/webp) and animated GIF decoding.
//! Pure-Rust decoders via the `image` crate — no feh/fbi/ImageMagick.

use crate::frame::Rgba;
use image::AnimationDecoder;
use std::path::Path;
use std::time::Duration;

/// Decodes a static image file into RGBA8.
pub fn decode_image(path: &Path) -> Result<Rgba, String> {
    let img = image::ImageReader::open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?
        .with_guessed_format()
        .map_err(|e| format!("format {}: {e}", path.display()))?
        .decode()
        .map_err(|e| format!("decode {}: {e}", path.display()))?;
    let rgba = img.to_rgba8();
    Ok(Rgba {
        w: rgba.width(),
        h: rgba.height(),
        data: rgba.into_raw(),
    })
}

pub struct GifSequence {
    pub frames: Vec<Rgba>,
    /// Per-frame display delay.
    pub delays: Vec<Duration>,
}

/// Decodes an animated GIF into full-canvas RGBA frames with delays.
/// The `image` crate's frame iterator already applies disposal compositing.
pub fn decode_gif(path: &Path) -> Result<GifSequence, String> {
    use image::codecs::gif::GifDecoder;
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let decoder = GifDecoder::new(std::io::BufReader::new(file))
        .map_err(|e| format!("gif init {}: {e}", path.display()))?;
    let frames: Vec<_> = decoder
        .into_frames()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("gif frames {}: {e}", path.display()))?;
    if frames.is_empty() {
        return Err(format!("gif {} has no frames", path.display()));
    }
    let mut out_frames = Vec::with_capacity(frames.len());
    let mut delays = Vec::with_capacity(frames.len());
    for f in frames {
        let buf = f.buffer();
        let (numer, denom) = f.delay().numer_denom_ms();
        // GIF delay of 0 means "as fast as possible"; browsers clamp to 100ms
        let ms = if numer == 0 {
            100
        } else {
            numer as u64 * 1000 / denom.max(1) as u64
        };
        let ms = ms.clamp(20, 60_000);
        out_frames.push(Rgba {
            w: buf.width(),
            h: buf.height(),
            data: buf.as_raw().clone(),
        });
        delays.push(Duration::from_millis(ms));
    }
    Ok(GifSequence {
        frames: out_frames,
        delays,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba as RgbaPx};

    fn write_png(path: &Path, w: u32, h: u32, color: [u8; 4]) {
        let img: ImageBuffer<RgbaPx<u8>, Vec<u8>> = ImageBuffer::from_pixel(w, h, RgbaPx(color));
        img.save(path).unwrap();
    }

    #[test]
    fn decodes_png() {
        let dir =
            std::env::temp_dir().join(format!("fossignage-decode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("red.png");
        write_png(&p, 8, 6, [200, 10, 20, 255]);
        let img = decode_image(&p).unwrap();
        assert_eq!((img.w, img.h), (8, 6));
        assert_eq!(img.pixel(4, 3), [200, 10, 20, 255]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn decodes_jpeg() {
        let dir =
            std::env::temp_dir().join(format!("fossignage-decode-test-jpg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("g.jpg");
        let img: ImageBuffer<RgbaPx<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(16, 16, RgbaPx([0, 128, 255, 255]));
        // jpeg encoder wants RGB8, not RGBA8
        image::DynamicImage::ImageRgba8(img)
            .to_rgb8()
            .save(&p)
            .unwrap();
        let dec = decode_image(&p).unwrap();
        assert_eq!((dec.w, dec.h), (16, 16));
        // jpeg is lossy; allow slack
        let px = dec.pixel(8, 8);
        assert!((px[1] as i32 - 128).abs() < 12 && px[2] > 240, "{px:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn decodes_animated_gif_with_delays() {
        use image::codecs::gif::GifEncoder;
        use image::{Frame, ImageBuffer, Rgba as RgbaPx};
        use std::time::Duration as D;
        let dir =
            std::env::temp_dir().join(format!("fossignage-decode-test-gif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("anim.gif");
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
        {
            let mut enc = GifEncoder::new(std::fs::File::create(&p).unwrap());
            enc.encode_frames([f1, f2]).unwrap();
        }
        let seq = decode_gif(&p).unwrap();
        assert_eq!(seq.frames.len(), 2);
        assert_eq!(seq.delays.len(), 2);
        assert!(seq.delays.iter().all(|d| *d >= D::from_millis(20)));
        let p0 = seq.frames[0].pixel(0, 0);
        assert!(p0[0] > 200 && p0[2] < 60, "{p0:?}");
        let p1 = seq.frames[1].pixel(0, 0);
        assert!(p1[2] > 200 && p1[0] < 60, "{p1:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_error_not_panic() {
        assert!(decode_image(Path::new("/nonexistent/nope.png")).is_err());
        assert!(decode_gif(Path::new("/nonexistent/nope.gif")).is_err());
    }
}
