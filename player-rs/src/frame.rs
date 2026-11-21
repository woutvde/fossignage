//! RGBA frame buffers, scaling, and YUV (NV12/I420) to RGB conversion.
//! Everything is integer math; the video path fuses colorspace conversion and
//! scaling in a single pass so a hardware-decoded 1080p NV12 frame goes
//! straight into the display buffer.

/// 8-bit RGBA image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

impl Rgba {
    pub fn new(w: u32, h: u32, color: [u8; 4]) -> Rgba {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&color);
        }
        Rgba { w, h, data }
    }

    pub fn filled(w: u32, h: u32, rgb: [u8; 3]) -> Rgba {
        Rgba::new(w, h, [rgb[0], rgb[1], rgb[2], 255])
    }

    #[inline]
    pub fn set_pixel(&mut self, x: u32, y: u32, c: [u8; 4]) {
        if x < self.w && y < self.h {
            let i = ((y * self.w + x) * 4) as usize;
            self.data[i..i + 4].copy_from_slice(&c);
        }
    }

    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.w + x) * 4) as usize;
        [
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ]
    }
}

/// Scales an RGBA image to (dw, dh). Downscaling uses area averaging (box
/// filter, integer fixed point); upscaling uses bilinear interpolation.
pub fn scale_rgba(src: &Rgba, dw: u32, dh: u32) -> Rgba {
    if dw == 0 || dh == 0 {
        return Rgba {
            w: dw,
            h: dh,
            data: Vec::new(),
        };
    }
    if src.w == dw && src.h == dh {
        return src.clone();
    }
    let mut out = Rgba {
        w: dw,
        h: dh,
        data: vec![0u8; (dw * dh * 4) as usize],
    };
    let (sw, sh) = (src.w as i64, src.h as i64);
    let (dw, dh) = (dw as i64, dh as i64);
    if sw >= dw && sh >= dh {
        // area average: each dst pixel covers src rect [x0,x1) x [y0,y1)
        for dy in 0..dh {
            let y0 = dy * sh / dh;
            let y1 = ((dy + 1) * sh / dh).max(y0 + 1);
            for dx in 0..dw {
                let x0 = dx * sw / dw;
                let x1 = ((dx + 1) * sw / dw).max(x0 + 1);
                let mut r = 0u64;
                let mut g = 0u64;
                let mut b = 0u64;
                let mut a = 0u64;
                let count = ((x1 - x0) * (y1 - y0)) as u64;
                for sy in y0..y1.min(sh) {
                    for sx in x0..x1.min(sw) {
                        let i = ((sy * sw + sx) * 4) as usize;
                        r += src.data[i] as u64;
                        g += src.data[i + 1] as u64;
                        b += src.data[i + 2] as u64;
                        a += src.data[i + 3] as u64;
                    }
                }
                let i = ((dy * dw + dx) * 4) as usize;
                out.data[i] = (r / count) as u8;
                out.data[i + 1] = (g / count) as u8;
                out.data[i + 2] = (b / count) as u8;
                out.data[i + 3] = (a / count) as u8;
            }
        }
    } else {
        // bilinear upscale (or mixed): map dst center into src coords
        for dy in 0..dh {
            let fy = (dy as f64 + 0.5) * sh as f64 / dh as f64 - 0.5;
            let y0 = fy.floor().max(0.0) as i64;
            let y1 = (y0 + 1).min(sh - 1);
            let wy = fy - y0 as f64;
            for dx in 0..dw {
                let fx = (dx as f64 + 0.5) * sw as f64 / dw as f64 - 0.5;
                let x0 = fx.floor().max(0.0) as i64;
                let x1 = (x0 + 1).min(sw - 1);
                let wx = fx - x0 as f64;
                let i00 = ((y0 * sw + x0) * 4) as usize;
                let i10 = ((y0 * sw + x1) * 4) as usize;
                let i01 = ((y1 * sw + x0) * 4) as usize;
                let i11 = ((y1 * sw + x1) * 4) as usize;
                let i = ((dy * dw + dx) * 4) as usize;
                for c in 0..4 {
                    let top = src.data[i00 + c] as f64 * (1.0 - wx) + src.data[i10 + c] as f64 * wx;
                    let bot = src.data[i01 + c] as f64 * (1.0 - wx) + src.data[i11 + c] as f64 * wx;
                    out.data[i + c] = (top * (1.0 - wy) + bot * wy).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
    out
}

/// A decoded video frame in a planar/semi-planar YUV format (what V4L2
/// hardware decoders produce).
#[derive(Debug, Clone)]
pub struct YuvFrame {
    pub w: u32,
    pub h: u32,
    /// Luma plane, row stride = w.
    pub y: Vec<u8>,
    /// Chroma: NV12 (semi-planar, UV interleaved) or I420 (planar U then V).
    pub chroma: Chroma,
}

#[derive(Debug, Clone)]
pub enum Chroma {
    /// UV interleaved, one byte U then one byte V per 2x2 block.
    Nv12(Vec<u8>),
    /// Planar: U plane (w/2 * h/2) then V plane.
    I420 { u: Vec<u8>, v: Vec<u8> },
}

/// BT.601 limited-range YUV -> RGB, fused with scaling to (dw, dh).
/// Uses fixed-point integer math with precomputed chroma tables.
pub fn yuv_to_rgba_scaled(f: &YuvFrame, dw: u32, dh: u32) -> Rgba {
    let (w, h) = (f.w as usize, f.h as usize);
    let cw = w / 2;
    let ch = h / 2;
    // precompute per-Cr/Cb contributions (limited range BT.601):
    // R = 1.164*(Y-16) + 1.596*(Cr-128)
    // G = 1.164*(Y-16) - 0.813*(Cr-128) - 0.391*(Cb-128)
    // B = 1.164*(Y-16) + 2.018*(Cb-128)
    // scaled by 256, clamped to [0, 255*256]
    let mut cr_r = [0i32; 256];
    let mut cb_g = [0i32; 256];
    let mut cr_g = [0i32; 256];
    let mut cb_b = [0i32; 256];
    for i in 0..256 {
        let c = i as i32 - 128;
        cr_r[i] = 409 * c; // 1.596*256
        cr_g[i] = -208 * c; // -0.813*256
        cb_g[i] = -100 * c; // -0.391*256
        cb_b[i] = 517 * c; // 2.018*256
    }
    let sample_chroma = |u: u8, v: u8| {
        (
            cr_r[v as usize],
            cr_g[v as usize],
            cb_g[u as usize],
            cb_b[u as usize],
        )
    };

    let mut out = Rgba {
        w: dw,
        h: dh,
        data: vec![0u8; (dw * dh * 4) as usize],
    };
    let get_chroma = |cx: usize, cy: usize| -> (u8, u8) {
        match &f.chroma {
            Chroma::Nv12(uv) => {
                let i = (cy * cw + cx) * 2;
                (uv[i], uv[i + 1]) // NV12: U first (Cb), then V (Cr)
            }
            Chroma::I420 { u, v } => {
                let i = cy * cw + cx;
                (u[i], v[i])
            }
        }
    };
    for dy in 0..dh as usize {
        // bilinear-ish sampling: nearest for luma is fine at these ratios,
        // but use mapped coords with linear interp on luma for quality
        let fy = (dy as f64 + 0.5) * h as f64 / dh as f64 - 0.5;
        let y0 = fy.clamp(0.0, (h - 1) as f64) as usize;
        let y1 = (y0 + 1).min(h - 1);
        let wy = if h > 1 {
            fy.clamp(0.0, (h - 1) as f64) - y0 as f64
        } else {
            0.0
        };
        for dx in 0..dw as usize {
            let fx = (dx as f64 + 0.5) * w as f64 / dw as f64 - 0.5;
            let x0 = fx.clamp(0.0, (w - 1) as f64) as usize;
            let x1 = (x0 + 1).min(w - 1);
            let wx = if w > 1 {
                fx.clamp(0.0, (w - 1) as f64) - x0 as f64
            } else {
                0.0
            };
            let l00 = f.y[y0 * w + x0] as f64;
            let l10 = f.y[y0 * w + x1] as f64;
            let l01 = f.y[y1 * w + x0] as f64;
            let l11 = f.y[y1 * w + x1] as f64;
            let lum =
                (l00 * (1.0 - wx) + l10 * wx) * (1.0 - wy) + (l01 * (1.0 - wx) + l11 * wx) * wy;
            let (cx, cy) = (x0 / 2, y0 / 2);
            let (u, v) = get_chroma(cx.min(cw.saturating_sub(1)), cy.min(ch.saturating_sub(1)));
            let (rr, rg, gb, bb) = sample_chroma(u, v);
            let yy = ((lum - 16.0) * 298.0) as i32; // 1.164*256
            let i = (dy * dw as usize + dx) * 4;
            out.data[i] = (((yy + rr).clamp(0, 255 * 256) + 128) / 256) as u8;
            out.data[i + 1] = (((yy + rg + gb).clamp(0, 255 * 256) + 128) / 256) as u8;
            out.data[i + 2] = (((yy + bb).clamp(0, 255 * 256) + 128) / 256) as u8;
            out.data[i + 3] = 255;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_identity_is_clone() {
        let src = Rgba::filled(4, 4, [10, 20, 30]);
        let out = scale_rgba(&src, 4, 4);
        assert_eq!(src, out);
    }

    #[test]
    fn scale_down_2x2_to_1x1_averages() {
        let mut src = Rgba::new(2, 2, [0, 0, 0, 0]);
        src.set_pixel(0, 0, [255, 0, 0, 255]);
        src.set_pixel(1, 0, [0, 255, 0, 255]);
        src.set_pixel(0, 1, [0, 0, 255, 255]);
        src.set_pixel(1, 1, [255, 255, 255, 255]);
        let out = scale_rgba(&src, 1, 1);
        assert_eq!(out.pixel(0, 0), [127, 127, 127, 255]);
    }

    #[test]
    fn scale_down_solid_color_is_preserved() {
        let src = Rgba::filled(8, 8, [12, 34, 56]);
        let out = scale_rgba(&src, 3, 5);
        for y in 0..5 {
            for x in 0..3 {
                assert_eq!(out.pixel(x, y), [12, 34, 56, 255]);
            }
        }
    }

    #[test]
    fn scale_up_1x1_fills() {
        let src = Rgba::filled(1, 1, [200, 100, 50]);
        let out = scale_rgba(&src, 4, 4);
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(out.pixel(x, y), [200, 100, 50, 255]);
            }
        }
    }

    #[test]
    fn scale_up_bilinear_midpoint() {
        // 2x1 black/white -> 4x1: mid pixels blend
        let mut src = Rgba::new(2, 1, [0, 0, 0, 255]);
        src.set_pixel(1, 0, [255, 255, 255, 255]);
        let out = scale_rgba(&src, 4, 1);
        // dst x=0 maps to src -0.25 -> clamped black; x=1 -> 0.25 -> 25% white
        let p = out.pixel(1, 0);
        assert!((p[0] as i32 - 64).abs() <= 1, "got {p:?}");
    }

    #[test]
    fn nv12_black_white_red() {
        // BT.601 limited range reference values
        let mut f = YuvFrame {
            w: 2,
            h: 2,
            y: vec![16, 16, 16, 16],
            chroma: Chroma::Nv12(vec![128, 128, 128, 128]),
        };
        let out = yuv_to_rgba_scaled(&f, 2, 2);
        assert_eq!(out.pixel(0, 0), [0, 0, 0, 255]);

        f.y = vec![235, 235, 235, 235];
        let out = yuv_to_rgba_scaled(&f, 2, 2);
        assert_eq!(out.pixel(0, 0), [255, 255, 255, 255]);

        // pure red: Y=81, Cb=90, V=240
        f.y = vec![81, 81, 81, 81];
        f.chroma = Chroma::Nv12(vec![90, 240, 90, 240]);
        let out = yuv_to_rgba_scaled(&f, 2, 2);
        let p = out.pixel(0, 0);
        assert!(p[0] > 240, "red channel {p:?}");
        assert!(p[1] < 16, "green channel {p:?}");
        assert!(p[2] < 16, "blue channel {p:?}");
    }

    #[test]
    fn nv12_scaling_to_display_size() {
        let mut f = YuvFrame {
            w: 4,
            h: 4,
            y: vec![235; 16],
            chroma: Chroma::Nv12(vec![128u8; 16]),
        };
        let out = yuv_to_rgba_scaled(&f, 2, 2);
        assert_eq!(out.w, 2);
        assert_eq!(out.h, 2);
        assert_eq!(out.pixel(1, 1), [255, 255, 255, 255]);
        // upscale path
        f.y = vec![16; 16];
        let out = yuv_to_rgba_scaled(&f, 8, 8);
        assert_eq!(out.pixel(7, 7), [0, 0, 0, 255]);
    }

    #[test]
    fn i420_chroma_layout() {
        let f = YuvFrame {
            w: 2,
            h: 2,
            y: vec![81, 81, 81, 81],
            chroma: Chroma::I420 {
                u: vec![90],
                v: vec![240],
            },
        };
        let out = yuv_to_rgba_scaled(&f, 2, 2);
        let p = out.pixel(0, 0);
        assert!(p[0] > 240 && p[1] < 16 && p[2] < 16, "{p:?}");
    }
}
