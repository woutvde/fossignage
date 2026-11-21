//! Idle screen: a status card with the pairing code (classic mode) or the
//! server address (standalone mode), rendered natively with the bitmap font
//! instead of ImageMagick + feh.

use crate::frame::Rgba;
use crate::text::{draw_text_centered, text_width};

const BG: [u8; 3] = [0x0f, 0x17, 0x2a]; // #0f172a
const HEADING: [u8; 4] = [0x94, 0xa3, 0xb8, 255]; // #94a3b8
const ACCENT: [u8; 4] = [0x38, 0xbd, 0xf8, 255]; // #38bdf8

/// Renders the idle card at the display resolution.
/// `code` = Some(4-char code) in classic mode; standalone passes the server
/// address via `display_addr`.
pub fn render_idle(w: u32, h: u32, code: Option<&str>, display_addr: Option<&str>) -> Rgba {
    let mut img = Rgba::filled(w, h, BG);
    let (heading, big_text): (String, String) = match (code, display_addr) {
        (Some(c), _) => (
            "Fossignage Display - enter this code on the Operator Console:".to_string(),
            c.to_string(),
        ),
        (None, Some(addr)) => (
            "Fossignage Standalone Display - upload content at:".to_string(),
            addr.to_string(),
        ),
        (None, None) => ("Fossignage Display".to_string(), "waiting...".to_string()),
    };
    let mid_y = h as i32 / 2;
    // heading: fixed scale 5 (40px), shrink if too wide
    let mut hs = 5u32;
    while hs > 1 && text_width(&heading, hs) > w.saturating_sub(40) {
        hs -= 1;
    }
    draw_text_centered(
        &mut img,
        mid_y - (hs * 8) as i32 * 2 - 20,
        hs,
        HEADING,
        &heading,
    );
    // big text: as large as fits (cap 27 = 220px like player.py)
    let mut bs = 27u32.min(h / 8);
    while bs > 1 && text_width(&big_text, bs) > w.saturating_sub(60) {
        bs -= 1;
    }
    draw_text_centered(&mut img, mid_y + 10, bs, ACCENT, &big_text);
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_card_has_background_and_text() {
        let img = render_idle(320, 240, Some("ABCD"), None);
        assert_eq!(img.pixel(0, 0), [BG[0], BG[1], BG[2], 255]);
        // some accent-colored pixels must exist (the code)
        let accent = [ACCENT[0], ACCENT[1], ACCENT[2], 255];
        let mut found = false;
        for y in 0..img.h {
            for x in 0..img.w {
                if img.pixel(x, y) == accent {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "no accent pixels rendered");
    }

    #[test]
    fn long_address_shrinks_to_fit() {
        let addr = "http://192.168.123.234:5000/operator";
        let img = render_idle(320, 240, None, Some(addr));
        let accent = [ACCENT[0], ACCENT[1], ACCENT[2], 255];
        let mut max_x = 0;
        for y in 0..img.h {
            for x in 0..img.w {
                if img.pixel(x, y) == accent {
                    max_x = max_x.max(x);
                }
            }
        }
        assert!(max_x < 320, "text overflowed: max_x={max_x}");
    }
}
