//! Bitmap text rendering on RGBA frames using the embedded 8x8 font.

use crate::font8x8::FONT8X8;
use crate::frame::Rgba;

pub fn text_width(text: &str, scale: u32) -> u32 {
    if text.is_empty() {
        return 0;
    }
    (text.chars().count() as u32 * 8 + (text.chars().count() as u32 - 1)) * scale
}

pub fn draw_text(img: &mut Rgba, x: i32, y: i32, scale: u32, color: [u8; 4], text: &str) {
    let scale = scale.max(1);
    let mut cx = x;
    for ch in text.chars() {
        let idx = (ch as usize).min(127);
        let glyph = FONT8X8[idx];
        for (row, &bits) in glyph.iter().enumerate() {
            for col in 0..8 {
                if (bits >> col) & 1 == 1 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            img.set_pixel(
                                (cx + col * scale as i32 + sx as i32) as u32,
                                (y + row as i32 * scale as i32 + sy as i32) as u32,
                                color,
                            );
                        }
                    }
                }
            }
        }
        cx += 9 * scale as i32; // 8px glyph + 1px spacing
    }
}

/// Draws `text` centered horizontally at vertical position `y`.
pub fn draw_text_centered(img: &mut Rgba, y: i32, scale: u32, color: [u8; 4], text: &str) {
    let w = text_width(text, scale) as i32;
    let x = (img.w as i32 - w) / 2;
    draw_text(img, x, y, scale, color, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_scales() {
        assert_eq!(text_width("", 1), 0);
        assert_eq!(text_width("A", 1), 8);
        assert_eq!(text_width("AB", 1), 17);
        assert_eq!(text_width("AB", 3), 51);
    }

    #[test]
    fn draws_known_glyph() {
        // '!' = 0x18,0x3C,0x3C,0x18,0x18,0x00,0x18,0x00
        let mut img = Rgba::filled(32, 16, [0, 0, 0]);
        draw_text(&mut img, 0, 0, 1, [255, 255, 255, 255], "!");
        // row 0: 0x18 = 0b00011000 -> cols 3,4 set
        assert_eq!(img.pixel(3, 0), [255, 255, 255, 255]);
        assert_eq!(img.pixel(4, 0), [255, 255, 255, 255]);
        assert_eq!(img.pixel(2, 0), [0, 0, 0, 255]);
        // row 5 is 0x00 -> nothing
        assert_eq!(img.pixel(3, 5), [0, 0, 0, 255]);
        // row 6: 0x18 again
        assert_eq!(img.pixel(3, 6), [255, 255, 255, 255]);
    }

    #[test]
    fn scaled_glyph_fills_blocks() {
        let mut img = Rgba::filled(32, 32, [0, 0, 0]);
        draw_text(&mut img, 0, 0, 2, [255, 0, 0, 255], "!");
        assert_eq!(img.pixel(6, 0), [255, 0, 0, 255]);
        assert_eq!(img.pixel(7, 1), [255, 0, 0, 255]);
        assert_eq!(img.pixel(5, 0), [0, 0, 0, 255]);
    }

    #[test]
    fn centered_text() {
        let mut img = Rgba::filled(100, 20, [0, 0, 0]);
        draw_text_centered(&mut img, 0, 1, [255, 255, 255, 255], "AB");
        // width of "AB" at scale 1 = 17, x = (100-17)/2 = 41
        // 'A' row 0 = 0x0C -> bits 2,3 set -> pixels at x = 43,44
        assert_eq!(img.pixel(43, 0), [255, 255, 255, 255]);
        assert_eq!(img.pixel(44, 0), [255, 255, 255, 255]);
        assert_eq!(img.pixel(41, 0), [0, 0, 0, 255]);
        assert_eq!(img.pixel(42, 0), [0, 0, 0, 255]);
    }
}
