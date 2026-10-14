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

/// `w` x `h` straight-alpha RGBA. `bg` fills the badge, `ring` outlines it, `fg` is the folder.
/// The badge is a circle as long as the image is displayed at its own aspect ratio (the caller
/// picks `w`:`h` to match the block of terminal cells it will cover).
/// `top` transparent pixels are left above the circle (it is as wide as the image).
pub fn folder_icon(w: usize, h: usize, top: usize, bg: [u8; 3], fg: [u8; 3], ring: [u8; 3]) -> Vec<u8> {
    const SS: usize = 3;
    let s = w as f32; // the circle's diameter in image pixels
    let cy = top as f32 + s / 2.0; // its centre row
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    // unit coordinates centred on the image, scaled by the shorter side
                    let u = ((x as f32 + (sx as f32 + 0.5) / SS as f32) - w as f32 / 2.0) / s + 0.5;
                    let v = ((y as f32 + (sy as f32 + 0.5) / SS as f32) - cy) / s + 0.5;
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

/// The badge fill with the blue channel nudged by 3: invisible to the eye, but an exact colour
/// that a screenshot can be searched for to learn where the badge is on screen.
pub fn marker_fill(c: [u8; 3]) -> [u8; 3] {
    [c[0], c[1], if c[2] >= 252 { c[2] - 3 } else { c[2] + 3 }]
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
        let (w, h) = (32, 48);
        let img = folder_icon(w, h, 6, [200, 200, 200], [30, 30, 30], [100, 100, 100]);
        assert_eq!(img.len(), w * h * 4);
        assert_eq!(img[3], 0, "corner pixel must be transparent");
        let mid = ((6 + w / 2) * w + w / 2) * 4;
        assert_eq!(img[mid + 3], 255, "centre pixel must be opaque");
        assert_eq!(img[(3 * w + w / 2) * 4 + 3], 0, "the margin above the circle is transparent");
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#BC1020"), Some([0xBC, 0x10, 0x20]));
        assert_eq!(parse_hex("xyz"), None);
    }
}
