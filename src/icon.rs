//! The folder button drawn inside the terminal (kitty graphics protocol): a round badge with a
//! folder glyph, rendered here as RGBA so no image library is needed.

fn blend(layers: &[(bool, [u8; 3])]) -> [f32; 4] {
    // first matching layer wins (later entries are underneath)
    for (on, c) in layers {
        if *on {
            return [c[0] as f32, c[1] as f32, c[2] as f32, 1.0];
        }
    }
    [0.0; 4]
}

fn in_round_rect(u: f32, v: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if u < x0 || u > x1 || v < y0 || v > y1 {
        return false;
    }
    let cx = u.clamp(x0 + r, x1 - r);
    let cy = v.clamp(y0 + r, y1 - r);
    (u - cx).hypot(v - cy) <= r
}

/// `px` x `px` straight-alpha RGBA. `bg` fills the badge, `ring` outlines it, `fg` is the folder.
pub fn folder_icon(px: usize, bg: [u8; 3], fg: [u8; 3], ring: [u8; 3]) -> Vec<u8> {
    const SS: usize = 3;
    let mut out = Vec::with_capacity(px * px * 4);
    for y in 0..px {
        for x in 0..px {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / px as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / px as f32;
                    let d = (u - 0.5).hypot(v - 0.5);
                    let body = in_round_rect(u, v, 0.26, 0.40, 0.74, 0.70, 0.04);
                    let tab = in_round_rect(u, v, 0.26, 0.31, 0.47, 0.44, 0.03);
                    let s = blend(&[(body || tab, fg), (d <= 0.5 - 0.045, bg), (d <= 0.5, ring)]);
                    for i in 0..3 {
                        acc[i] += s[i] * s[3];
                    }
                    acc[3] += s[3];
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            for i in 0..3 {
                out.push(if acc[3] > 0.0 { (acc[i] / acc[3]).round() as u8 } else { 0 });
            }
            out.push((a * 255.0).round() as u8);
        }
    }
    out
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

pub fn parse_hex(h: &str) -> Option<[u8; 3]> {
    let h = h.trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([v(0)?, v(2)?, v(4)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn icon_has_round_transparent_corners_and_opaque_centre() {
        let px = 32;
        let img = folder_icon(px, [200, 200, 200], [30, 30, 30], [100, 100, 100]);
        assert_eq!(img.len(), px * px * 4);
        assert_eq!(img[3], 0, "corner pixel must be transparent");
        let mid = (px / 2 * px + px / 2) * 4;
        assert_eq!(img[mid + 3], 255, "centre pixel must be opaque");
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#BC1020"), Some([0xBC, 0x10, 0x20]));
        assert_eq!(parse_hex("xyz"), None);
    }
}
