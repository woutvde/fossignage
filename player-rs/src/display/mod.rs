//! Display output backends. The player always presents full-screen RGBA
//! frames at the display's native resolution; each backend converts to its
//! native pixel format and pushes it to the hardware.

use crate::config::{Backend, Config};
use crate::frame::Rgba;

#[cfg(target_os = "linux")]
pub mod drm;
#[cfg(target_os = "linux")]
pub mod fb;
pub mod headless;

pub trait Display {
    /// Native output size in pixels.
    fn size(&self) -> (u32, u32);
    /// Present a full-screen RGBA frame (already scaled to `size()`).
    fn present(&mut self, frame: &Rgba) -> Result<(), String>;
    fn name(&self) -> &'static str;
}

pub fn open(cfg: &Config) -> Result<Box<dyn Display>, String> {
    match cfg.backend {
        Backend::Drm => drm::DrmDisplay::open().map(|d| Box::new(d) as Box<dyn Display>),
        Backend::Fb => fb::FbDisplay::open().map(|d| Box::new(d) as Box<dyn Display>),
        Backend::Headless => {
            headless::HeadlessDisplay::open(cfg).map(|d| Box::new(d) as Box<dyn Display>)
        }
        Backend::Auto => {
            #[cfg(target_os = "linux")]
            {
                if let Ok(d) = drm::DrmDisplay::open() {
                    crate::log!("display backend: drm/kms ({}x{})", d.size().0, d.size().1);
                    return Ok(Box::new(d));
                }
                if let Ok(d) = fb::FbDisplay::open() {
                    crate::log!(
                        "display backend: framebuffer ({}x{})",
                        d.size().0,
                        d.size().1
                    );
                    return Ok(Box::new(d));
                }
            }
            if cfg.out_dir.is_some() {
                crate::log!("no hardware display found; falling back to headless");
                return headless::HeadlessDisplay::open(cfg)
                    .map(|d| Box::new(d) as Box<dyn Display>);
            }
            Err(
                "no display backend available (tried DRM/KMS and /dev/fb0); \
                 use --backend headless --out-dir DIR for testing"
                    .into(),
            )
        }
    }
}
