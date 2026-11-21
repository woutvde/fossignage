//! Headless backend for tests/CI: instead of pushing to hardware it writes
//! every presented frame to `<out_dir>/frame_NNNNNN.ppm` (P6) plus a
//! `latest.ppm` copy, and counts presents. Lets the whole pipeline (poll,
//! decode, scale, present) run end-to-end on machines without a display.

use crate::config::Config;
use crate::frame::Rgba;
use std::path::PathBuf;

pub struct HeadlessDisplay {
    w: u32,
    h: u32,
    out_dir: PathBuf,
    count: u64,
}

impl HeadlessDisplay {
    pub fn open(cfg: &Config) -> Result<HeadlessDisplay, String> {
        let (w, h) = match (cfg.width, cfg.height) {
            (Some(w), Some(h)) => (w, h),
            _ => (1920, 1080),
        };
        let out_dir = cfg
            .out_dir
            .clone()
            .ok_or("headless backend requires --out-dir")?;
        std::fs::create_dir_all(&out_dir)
            .map_err(|e| format!("create {}: {e}", out_dir.display()))?;
        Ok(HeadlessDisplay {
            w,
            h,
            out_dir,
            count: 0,
        })
    }

    pub fn frame_count(&self) -> u64 {
        self.count
    }
}

impl super::Display for HeadlessDisplay {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn name(&self) -> &'static str {
        "headless"
    }
    fn present(&mut self, frame: &Rgba) -> Result<(), String> {
        if frame.w != self.w || frame.h != self.h {
            return Err(format!(
                "frame {}x{} != headless {}x{}",
                frame.w, frame.h, self.w, self.h
            ));
        }
        let mut ppm = Vec::with_capacity((self.w * self.h * 3) as usize + 32);
        ppm.extend_from_slice(format!("P6\n{} {}\n255\n", self.w, self.h).as_bytes());
        for px in frame.data.as_chunks::<4>().0 {
            ppm.push(px[0]);
            ppm.push(px[1]);
            ppm.push(px[2]);
        }
        let path = self.out_dir.join(format!("frame_{:06}.ppm", self.count));
        std::fs::write(&path, &ppm).map_err(|e| format!("write {}: {e}", path.display()))?;
        let _ = std::fs::write(self.out_dir.join("latest.ppm"), &ppm);
        self.count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::display::Display as _;

    #[test]
    fn writes_ppm_frames() {
        let dir =
            std::env::temp_dir().join(format!("fossignage-headless-test-{}", std::process::id()));
        let cfg = Config {
            backend: crate::config::Backend::Headless,
            out_dir: Some(dir.clone()),
            width: Some(4),
            height: Some(2),
            ..Default::default()
        };
        let mut d = HeadlessDisplay::open(&cfg).unwrap();
        let frame = Rgba::filled(4, 2, [1, 2, 3]);
        d.present(&frame).unwrap();
        d.present(&frame).unwrap();
        assert_eq!(d.frame_count(), 2);
        let data = std::fs::read(dir.join("frame_000001.ppm")).unwrap();
        assert!(data.starts_with(b"P6\n4 2\n255\n"));
        assert_eq!(data.len(), "P6\n4 2\n255\n".len() + 4 * 2 * 3);
        // first pixel is [1,2,3]
        let off = "P6\n4 2\n255\n".len();
        assert_eq!(&data[off..off + 3], &[1, 2, 3]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_wrong_sized_frame() {
        let dir =
            std::env::temp_dir().join(format!("fossignage-headless-test2-{}", std::process::id()));
        let cfg = Config {
            backend: crate::config::Backend::Headless,
            out_dir: Some(dir.clone()),
            width: Some(4),
            height: Some(2),
            ..Default::default()
        };
        let mut d = HeadlessDisplay::open(&cfg).unwrap();
        assert!(d.present(&Rgba::filled(8, 8, [0, 0, 0])).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
