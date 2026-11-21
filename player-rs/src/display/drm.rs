//! DRM/KMS display backend (Linux): opens /dev/dri/card*, becomes DRM master,
//! modesets the preferred connector mode, and presents frames through two
//! dumb buffers with vsync'd page flips. No X server, no libdrm — just the
//! kernel UAPI via ioctls. This is the primary backend on Raspberry Pi
//! (vc4/v3d KMS drivers) and works with zero userspace graphics stack.

use crate::frame::Rgba;
use drm_sys::*;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

// --- ioctl request numbers (kernel UAPI, stable; values verified against
// --- /usr/include/drm/drm.h with a C program — see tests/ffi_layout.rs)
const fn ioc(dir: u32, ty: u8, nr: u8, size: u32) -> libc::c_ulong {
    ((dir << 30) | (size << 16) | ((ty as u32) << 8) | nr as u32) as libc::c_ulong
}
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOCTL_SET_MASTER: libc::c_ulong = ioc(0, b'd', 0x1e, 0);
const IOCTL_DROP_MASTER: libc::c_ulong = ioc(0, b'd', 0x1f, 0);
const IOCTL_GET_CAP: libc::c_ulong = ioc(IOC_READ | IOC_WRITE, b'd', 0x0c, 16);
const IOCTL_SET_CLIENT_CAP: libc::c_ulong = ioc(IOC_WRITE, b'd', 0x0d, 16);
const IOCTL_MODE_GETRESOURCES: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    b'd',
    0xa0,
    size_of::<drm_mode_card_res>() as u32,
);
const IOCTL_MODE_GETCONNECTOR: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    b'd',
    0xa7,
    size_of::<drm_mode_get_connector>() as u32,
);
const IOCTL_MODE_GETENCODER: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    b'd',
    0xa6,
    size_of::<drm_mode_get_encoder>() as u32,
);
const IOCTL_MODE_GETCRTC: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    b'd',
    0xa1,
    size_of::<drm_mode_crtc>() as u32,
);
const IOCTL_MODE_SETCRTC: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    b'd',
    0xa2,
    size_of::<drm_mode_crtc>() as u32,
);
const IOCTL_MODE_ADDFB: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    b'd',
    0xae,
    size_of::<drm_mode_fb_cmd>() as u32,
);
const IOCTL_MODE_RMFB: libc::c_ulong = ioc(IOC_READ | IOC_WRITE, b'd', 0xaf, 4);
const IOCTL_MODE_CREATE_DUMB: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    b'd',
    0xb2,
    size_of::<drm_mode_create_dumb>() as u32,
);
const IOCTL_MODE_MAP_DUMB: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    b'd',
    0xb3,
    size_of::<drm_mode_map_dumb>() as u32,
);
const IOCTL_MODE_DESTROY_DUMB: libc::c_ulong = ioc(IOC_READ | IOC_WRITE, b'd', 0xb4, 4);
const IOCTL_MODE_PAGE_FLIP: libc::c_ulong = ioc(
    IOC_READ | IOC_WRITE,
    b'd',
    0xb0,
    size_of::<drm_mode_crtc_page_flip>() as u32,
);

const DRM_MODE_CONNECTED: u32 = 1;
const DRM_MODE_TYPE_PREFERRED: u32 = 8;
const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
#[allow(dead_code)]
const DRM_FORMAT_XRGB8888: u32 = 0x34325258; // 'XR24'

fn ioctl<T>(fd: RawFd, req: libc::c_ulong, arg: *mut T) -> io::Result<()> {
    let r = unsafe { libc::ioctl(fd, req, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct DumbBuffer {
    handle: u32,
    pitch: u32,
    size: u64,
    map: *mut u8,
}

impl DumbBuffer {
    fn create(fd: RawFd, w: u32, h: u32) -> io::Result<DumbBuffer> {
        let mut c = drm_mode_create_dumb {
            width: w,
            height: h,
            bpp: 32,
            ..Default::default()
        };
        ioctl(fd, IOCTL_MODE_CREATE_DUMB, &mut c)?;
        let mut m = drm_mode_map_dumb {
            handle: c.handle,
            ..Default::default()
        };
        ioctl(fd, IOCTL_MODE_MAP_DUMB, &mut m)?;
        let size = (c.pitch as u64) * (h as u64);
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                m.offset as i64,
            )
        };
        if map == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            let mut d = drm_mode_destroy_dumb { handle: c.handle };
            let _ = ioctl(fd, IOCTL_MODE_DESTROY_DUMB, &mut d);
            return Err(e);
        }
        Ok(DumbBuffer {
            handle: c.handle,
            pitch: c.pitch,
            size,
            map: map as *mut u8,
        })
    }

    fn addfb(&self, fd: RawFd, w: u32, h: u32) -> io::Result<u32> {
        let mut f = drm_mode_fb_cmd {
            width: w,
            height: h,
            pitch: self.pitch,
            bpp: 32,
            depth: 24,
            handle: self.handle,
            ..Default::default()
        };
        ioctl(fd, IOCTL_MODE_ADDFB, &mut f)?;
        Ok(f.fb_id)
    }

    fn rmfb(&self, fd: RawFd, fb_id: u32) {
        let mut id = fb_id;
        let _ = ioctl(fd, IOCTL_MODE_RMFB, &mut id);
    }
}

impl Drop for DumbBuffer {
    fn drop(&mut self) {
        unsafe {
            if !self.map.is_null() {
                libc::munmap(self.map as *mut libc::c_void, self.size as usize);
            }
        }
    }
}

pub struct DrmDisplay {
    _file: File,
    fd: RawFd,
    crtc_id: u32,
    saved_crtc: drm_mode_crtc,
    bufs: [Option<DumbBuffer>; 2],
    fbs: [u32; 2],
    cur: usize,
    w: u32,
    h: u32,
}

impl DrmDisplay {
    pub fn open() -> Result<DrmDisplay, String> {
        let mut last_err = String::new();
        for i in 0..4 {
            let path = format!("/dev/dri/card{i}");
            match Self::open_card(&path) {
                Ok(d) => return Ok(d),
                Err(e) => last_err = format!("{path}: {e}"),
            }
        }
        Err(format!("no usable DRM device ({last_err})"))
    }

    fn open_card(path: &str) -> Result<DrmDisplay, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        let fd = file.as_raw_fd();

        // become DRM master (fails with EBUSY/EPERM if another master exists)
        let _ = ioctl(fd, IOCTL_SET_MASTER, std::ptr::null_mut::<libc::c_void>());
        // dumb buffer support is mandatory for this backend
        let mut cap = drm_get_cap {
            capability: DRM_CAP_DUMB_BUFFER as u64,
            value: 0,
        };
        ioctl(fd, IOCTL_GET_CAP, &mut cap).map_err(|e| format!("GET_CAP: {e}"))?;
        if cap.value == 0 {
            return Err("driver has no dumb buffer support".into());
        }
        // best effort: universal planes (needed for some drivers' primary plane)
        let mut sc = drm_set_client_cap {
            capability: DRM_CLIENT_CAP_UNIVERSAL_PLANES as u64,
            value: 1,
        };
        let _ = ioctl(fd, IOCTL_SET_CLIENT_CAP, &mut sc);

        // resources
        let mut res = drm_mode_card_res::default();
        ioctl(fd, IOCTL_MODE_GETRESOURCES, &mut res).map_err(|e| format!("GETRESOURCES: {e}"))?;
        if res.count_connectors == 0 {
            return Err("no connectors".into());
        }
        let connectors = read_u32_array(fd, res.connector_id_ptr, res.count_connectors)?;
        let crtcs = read_u32_array(fd, res.crtc_id_ptr, res.count_crtcs)?;

        // find a connected connector with modes
        let mut chosen: Option<(u32, drm_mode_modeinfo, u32)> = None; // (connector_id, mode, crtc_id)
        for &conn in &connectors {
            let mut c = drm_mode_get_connector {
                connector_id: conn,
                ..Default::default()
            };
            if ioctl(fd, IOCTL_MODE_GETCONNECTOR, &mut c).is_err() {
                continue;
            }
            if c.connection != DRM_MODE_CONNECTED || c.count_modes == 0 {
                continue;
            }
            let modes = read_mode_array(fd, c.modes_ptr, c.count_modes)?;
            let encoders = read_u32_array(fd, c.encoders_ptr, c.count_encoders)?;
            let mode = modes
                .iter()
                .find(|m| m.type_ & DRM_MODE_TYPE_PREFERRED != 0)
                .or_else(|| modes.first())
                .copied()
                .ok_or("no modes")?;
            // resolve crtc: from the connector's current encoder, else first possible
            let mut crtc_id = 0u32;
            for &enc in &encoders {
                let mut e = drm_mode_get_encoder {
                    encoder_id: enc,
                    ..Default::default()
                };
                if ioctl(fd, IOCTL_MODE_GETENCODER, &mut e).is_ok() && e.crtc_id != 0 {
                    crtc_id = e.crtc_id;
                    break;
                }
            }
            if crtc_id == 0 {
                for (i, &c) in crtcs.iter().enumerate() {
                    // any encoder of this connector allowing this crtc?
                    let mut ok = false;
                    for &enc in &encoders {
                        let mut e = drm_mode_get_encoder {
                            encoder_id: enc,
                            ..Default::default()
                        };
                        if ioctl(fd, IOCTL_MODE_GETENCODER, &mut e).is_ok()
                            && e.possible_crtcs & (1 << i) != 0
                        {
                            ok = true;
                            break;
                        }
                    }
                    if ok || encoders.is_empty() {
                        crtc_id = c;
                        break;
                    }
                }
            }
            if crtc_id == 0 {
                crtc_id = *crtcs.first().ok_or("no crtcs")?;
            }
            chosen = Some((conn, mode, crtc_id));
            break;
        }
        let (connector_id, mode, crtc_id) = chosen.ok_or("no connected connector with modes")?;
        let w = mode.hdisplay as u32;
        let h = mode.vdisplay as u32;
        if w == 0 || h == 0 {
            return Err("zero-size mode".into());
        }

        // save current crtc so we can restore it on exit
        let mut saved_crtc = drm_mode_crtc {
            crtc_id,
            ..Default::default()
        };
        let _ = ioctl(fd, IOCTL_MODE_GETCRTC, &mut saved_crtc);

        // two dumb buffers for flip presentation
        let mut bufs: [Option<DumbBuffer>; 2] = [None, None];
        let mut fbs = [0u32; 2];
        for i in 0..2 {
            let b = DumbBuffer::create(fd, w, h).map_err(|e| format!("CREATE_DUMB: {e}"))?;
            fbs[i] = b.addfb(fd, w, h).map_err(|e| format!("ADDFB: {e}"))?;
            bufs[i] = Some(b);
        }

        // modeset
        let mut set = drm_mode_crtc {
            set_connectors_ptr: &connector_id as *const u32 as u64,
            count_connectors: 1,
            crtc_id,
            fb_id: fbs[0],
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode,
        };
        ioctl(fd, IOCTL_MODE_SETCRTC, &mut set).map_err(|e| format!("SETCRTC: {e}"))?;

        Ok(DrmDisplay {
            _file: file,
            fd,
            crtc_id,
            saved_crtc,
            bufs,
            fbs,
            cur: 0,
            w,
            h,
        })
    }

    fn wait_flip_event(&self, timeout_ms: i32) {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r <= 0 {
            return;
        }
        // drain pending flip events (header: u32 length + u32 type)
        let mut buf = [0u8; 4096];
        unsafe {
            let n = libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            if n > 8 {
                let len = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                let _ = len;
            }
        }
    }
}

fn read_u32_array(fd: RawFd, ptr: u64, count: u32) -> Result<Vec<u32>, String> {
    if count == 0 || ptr == 0 {
        return Ok(Vec::new());
    }
    let mut v = vec![0u32; count as usize];
    let r = unsafe {
        libc::pread(
            fd,
            v.as_mut_ptr() as *mut libc::c_void,
            (count as usize) * 4,
            ptr as i64,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(v)
}

fn read_mode_array(fd: RawFd, ptr: u64, count: u32) -> Result<Vec<drm_mode_modeinfo>, String> {
    if count == 0 || ptr == 0 {
        return Ok(Vec::new());
    }
    let mut v = vec![drm_mode_modeinfo::default(); count as usize];
    let r = unsafe {
        libc::pread(
            fd,
            v.as_mut_ptr() as *mut libc::c_void,
            (count as usize) * size_of::<drm_mode_modeinfo>(),
            ptr as i64,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(v)
}

impl super::Display for DrmDisplay {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    fn name(&self) -> &'static str {
        "drm"
    }

    fn present(&mut self, frame: &Rgba) -> Result<(), String> {
        let back = 1 - self.cur;
        let buf = self.bufs[back].as_ref().ok_or("no back buffer")?;
        let (w, h) = (self.w as usize, self.h as usize);
        if frame.w as usize != w || frame.h as usize != h {
            return Err(format!(
                "frame {}x{} != display {}x{}",
                frame.w, frame.h, w, h
            ));
        }
        // RGBA -> XRGB8888 (little-endian: bytes B,G,R,X)
        unsafe {
            let dst = buf.map;
            let pitch = buf.pitch as usize;
            for y in 0..h {
                let row = dst.add(y * pitch);
                let src = &frame.data[y * w * 4..(y + 1) * w * 4];
                for x in 0..w {
                    let s = &src[x * 4..x * 4 + 4];
                    let px = (s[2] as u32) | ((s[1] as u32) << 8) | ((s[0] as u32) << 16);
                    (row.add(x * 4) as *mut u32).write_unaligned(px);
                }
            }
        }
        // page flip to the back buffer; on failure write directly to front
        let mut flip = drm_mode_crtc_page_flip {
            crtc_id: self.crtc_id,
            fb_id: self.fbs[back],
            flags: DRM_MODE_PAGE_FLIP_EVENT,
            ..Default::default()
        };
        if ioctl(self.fd, IOCTL_MODE_PAGE_FLIP, &mut flip).is_ok() {
            self.wait_flip_event(1000);
            self.cur = back;
        } else {
            // direct write into the currently displayed buffer (may tear)
            let buf = self.bufs[self.cur].as_ref().ok_or("no front buffer")?;
            unsafe {
                let dst = buf.map;
                let pitch = buf.pitch as usize;
                for y in 0..h {
                    let row = dst.add(y * pitch);
                    let src = &frame.data[y * w * 4..(y + 1) * w * 4];
                    for x in 0..w {
                        let s = &src[x * 4..x * 4 + 4];
                        let px = (s[2] as u32) | ((s[1] as u32) << 8) | ((s[0] as u32) << 16);
                        (row.add(x * 4) as *mut u32).write_unaligned(px);
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for DrmDisplay {
    fn drop(&mut self) {
        // restore the previous modeset, remove our framebuffers, drop master
        let mut restore = self.saved_crtc;
        let _ = ioctl(self.fd, IOCTL_MODE_SETCRTC, &mut restore);
        for i in 0..2 {
            if let Some(b) = &self.bufs[i] {
                b.rmfb(self.fd, self.fbs[i]);
                let mut d = drm_mode_destroy_dumb { handle: b.handle };
                let _ = ioctl(self.fd, IOCTL_MODE_DESTROY_DUMB, &mut d);
            }
        }
        let _ = ioctl(
            self.fd,
            IOCTL_DROP_MASTER,
            std::ptr::null_mut::<libc::c_void>(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Values verified against /usr/include/drm/drm.h with gcc (see
    // tests/ffi_layout.rs, which checks the struct layouts the same way).
    #[test]
    fn ioctl_numbers_match_kernel_uapi() {
        assert_eq!(IOCTL_MODE_GETRESOURCES, 0xc04064a0);
        assert_eq!(IOCTL_MODE_GETCONNECTOR, 0xc05064a7);
        assert_eq!(IOCTL_MODE_GETENCODER, 0xc01464a6);
        assert_eq!(IOCTL_MODE_GETCRTC, 0xc06864a1);
        assert_eq!(IOCTL_MODE_SETCRTC, 0xc06864a2);
        assert_eq!(IOCTL_MODE_ADDFB, 0xc01c64ae);
        assert_eq!(IOCTL_MODE_RMFB, 0xc00464af);
        assert_eq!(IOCTL_MODE_CREATE_DUMB, 0xc02064b2);
        assert_eq!(IOCTL_MODE_MAP_DUMB, 0xc01064b3);
        assert_eq!(IOCTL_MODE_DESTROY_DUMB, 0xc00464b4);
        assert_eq!(IOCTL_MODE_PAGE_FLIP, 0xc01864b0);
        assert_eq!(IOCTL_SET_MASTER, 0x641e);
        assert_eq!(IOCTL_DROP_MASTER, 0x641f);
        assert_eq!(IOCTL_GET_CAP, 0xc010640c);
        assert_eq!(IOCTL_SET_CLIENT_CAP, 0x4010640d);
    }

    #[test]
    fn drm_sys_struct_layouts_match_c() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<drm_mode_modeinfo>(), 68);
        assert_eq!(size_of::<drm_mode_card_res>(), 64);
        assert_eq!(size_of::<drm_mode_get_connector>(), 80);
        assert_eq!(size_of::<drm_mode_get_encoder>(), 20);
        assert_eq!(size_of::<drm_mode_crtc>(), 104);
        assert_eq!(size_of::<drm_mode_fb_cmd>(), 28);
        assert_eq!(size_of::<drm_mode_create_dumb>(), 32);
        assert_eq!(size_of::<drm_mode_map_dumb>(), 16);
        assert_eq!(size_of::<drm_mode_destroy_dumb>(), 4);
        assert_eq!(size_of::<drm_mode_crtc_page_flip>(), 24);
        assert_eq!(size_of::<drm_event>(), 8);
        assert_eq!(size_of::<drm_event_vblank>(), 32);
        assert_eq!(offset_of!(drm_mode_get_connector, modes_ptr), 8);
        assert_eq!(offset_of!(drm_mode_get_connector, connection), 60);
        assert_eq!(offset_of!(drm_mode_crtc, fb_id), 16);
        assert_eq!(offset_of!(drm_mode_crtc, mode), 36);
        assert_eq!(offset_of!(drm_mode_fb_cmd, handle), 24);
        assert_eq!(offset_of!(drm_mode_create_dumb, handle), 16);
        assert_eq!(offset_of!(drm_mode_map_dumb, offset), 8);
        assert_eq!(offset_of!(drm_mode_crtc_page_flip, user_data), 16);
        assert_eq!(DRM_FORMAT_XRGB8888, 0x34325258);
    }
}
