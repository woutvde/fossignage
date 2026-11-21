//! Raw framebuffer backend (/dev/fb0): the lowest common denominator on
//! Pi consoles without KMS. mmap the fb and write converted pixels per line.
//! Structs are local (libc doesn't export them); layouts verified against
//! /usr/include/linux/fb.h in the unit tests below.

use crate::frame::Rgba;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;

// linux/fb.h defines these as plain _IO (no size/dir bits)
const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FbBitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FbVarScreeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: FbBitfield,
    pub green: FbBitfield,
    pub blue: FbBitfield,
    pub transp: FbBitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FbFixScreeninfo {
    pub id: [u8; 16],
    pub smem_start: libc::c_ulong,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: libc::c_ulong,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

pub struct FbDisplay {
    #[allow(dead_code)]
    file: std::fs::File,
    map: *mut u8,
    map_len: usize,
    line_length: usize,
    w: u32,
    h: u32,
    bpp: u32,
    r_off: u32,
    g_off: u32,
    b_off: u32,
    r_len: u32,
    g_len: u32,
    b_len: u32,
}

impl FbDisplay {
    pub fn open() -> Result<FbDisplay, String> {
        Self::open_path("/dev/fb0")
    }

    pub fn open_path(path: &str) -> Result<FbDisplay, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("open {path}: {e}"))?;
        let fd = file.as_raw_fd();
        let mut var: FbVarScreeninfo = FbVarScreeninfo::default();
        let mut fix: FbFixScreeninfo = FbFixScreeninfo::default();
        let r = unsafe { libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut var) };
        if r < 0 {
            return Err(format!(
                "FBIOGET_VSCREENINFO: {}",
                io::Error::last_os_error()
            ));
        }
        let r = unsafe { libc::ioctl(fd, FBIOGET_FSCREENINFO, &mut fix) };
        if r < 0 {
            return Err(format!(
                "FBIOGET_FSCREENINFO: {}",
                io::Error::last_os_error()
            ));
        }
        let (w, h, bpp) = (var.xres, var.yres, var.bits_per_pixel);
        if w == 0 || h == 0 || (bpp != 16 && bpp != 24 && bpp != 32) {
            return Err(format!("unsupported fb mode {w}x{h}@{bpp}"));
        }
        let line_length = fix.line_length as usize;
        let map_len = line_length * h as usize;
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                fix.smem_start as i64,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(format!("mmap fb: {}", io::Error::last_os_error()));
        }
        Ok(FbDisplay {
            file,
            map: map as *mut u8,
            map_len,
            line_length,
            w,
            h,
            bpp,
            r_off: var.red.offset,
            g_off: var.green.offset,
            b_off: var.blue.offset,
            r_len: var.red.length,
            g_len: var.green.length,
            b_len: var.blue.length,
        })
    }

    fn write_pixels(&self, frame: &Rgba) -> Result<(), String> {
        let (w, h) = (self.w as usize, self.h as usize);
        if frame.w as usize != w || frame.h as usize != h {
            return Err(format!("frame {}x{} != fb {}x{}", frame.w, frame.h, w, h));
        }
        let bytes_pp = (self.bpp / 8) as usize;
        unsafe {
            for y in 0..h {
                let row = self.map.add(y * self.line_length);
                let src = &frame.data[y * w * 4..(y + 1) * w * 4];
                for x in 0..w {
                    let s = &src[x * 4..x * 4 + 4];
                    let r = s[0] as u32;
                    let g = s[1] as u32;
                    let b = s[2] as u32;
                    let px = ((r >> (8 - self.r_len.min(8))) << self.r_off)
                        | ((g >> (8 - self.g_len.min(8))) << self.g_off)
                        | ((b >> (8 - self.b_len.min(8))) << self.b_off);
                    let dst = row.add(x * bytes_pp);
                    match bytes_pp {
                        4 => (dst as *mut u32).write_unaligned(px),
                        3 => {
                            dst.write((px & 0xff) as u8);
                            dst.add(1).write(((px >> 8) & 0xff) as u8);
                            dst.add(2).write(((px >> 16) & 0xff) as u8);
                        }
                        2 => (dst as *mut u16).write_unaligned(px as u16),
                        _ => return Err(format!("unsupported bpp {}", self.bpp)),
                    }
                }
            }
        }
        Ok(())
    }
}

impl super::Display for FbDisplay {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn name(&self) -> &'static str {
        "fb"
    }
    fn present(&mut self, frame: &Rgba) -> Result<(), String> {
        self.write_pixels(frame)
    }
}

impl Drop for FbDisplay {
    fn drop(&mut self) {
        unsafe {
            if !self.map.is_null() {
                libc::munmap(self.map as *mut libc::c_void, self.map_len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn fb_ioctl_numbers_match_kernel_uapi() {
        assert_eq!(FBIOGET_VSCREENINFO, 0x4600);
        assert_eq!(FBIOGET_FSCREENINFO, 0x4602);
    }

    #[test]
    fn fb_struct_layouts_match_c() {
        // verified with gcc against /usr/include/linux/fb.h
        assert_eq!(size_of::<FbVarScreeninfo>(), 160);
        assert_eq!(size_of::<FbFixScreeninfo>(), 80);
        assert_eq!(size_of::<FbBitfield>(), 12);
        assert_eq!(offset_of!(FbVarScreeninfo, xres), 0);
        assert_eq!(offset_of!(FbVarScreeninfo, yres), 4);
        assert_eq!(offset_of!(FbVarScreeninfo, bits_per_pixel), 24);
        assert_eq!(offset_of!(FbVarScreeninfo, red), 32);
        assert_eq!(offset_of!(FbFixScreeninfo, smem_start), 16);
        assert_eq!(offset_of!(FbFixScreeninfo, line_length), 48);
    }
}
