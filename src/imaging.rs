//! Image helpers: OpenCV-compatible resize, ROI crop, JPEG, and annotation.

use image::{Rgb, RgbImage};

/// `cv2.resize(..., interpolation=INTER_LINEAR)` for 8-bit 3-channel images,
/// bit-exact with OpenCV's SIMD path: 11-bit fixed-point coefficients, an
/// integer horizontal pass, then `(((b0*(r0>>4))>>16) + ((b1*(r1>>4))>>16) + 2) >> 2`.
/// Matching OpenCV exactly keeps model confidences identical to the Python
/// implementation (and to what Obico's thresholds were tuned on).
pub fn resize_linear(src: &RgbImage, dw: u32, dh: u32) -> RgbImage {
    const SCALE: f32 = 2048.0;
    let (sw, sh) = src.dimensions();
    let tab = |dsize: u32, ssize: u32| -> Vec<(usize, i32, i32)> {
        let scale = 1.0f64 / (dsize as f64 / ssize as f64);
        (0..dsize)
            .map(|d| {
                let mut f = ((d as f64 + 0.5) * scale - 0.5) as f32;
                let mut i = f.floor() as i64;
                f -= i as f32;
                if i < 0 {
                    i = 0;
                    f = 0.0;
                }
                if i >= ssize as i64 - 1 {
                    i = ssize as i64 - 1;
                    f = 0.0;
                }
                let a0 = ((1.0 - f) * SCALE).round_ties_even() as i32;
                let a1 = (f * SCALE).round_ties_even() as i32;
                (i as usize, a0, a1)
            })
            .collect()
    };
    let xt = tab(dw, sw);
    // Vertically OpenCV clamps the source *rows* but not the weights: at the top edge
    // (sy = -1 when upscaling) it blends row 0 with itself using the unclamped fractional
    // weights, which rounds differently in fixed point. Replicate that exactly.
    let yt: Vec<(usize, usize, i32, i32)> = {
        let scale = 1.0f64 / (dh as f64 / sh as f64);
        let last = sh as i64 - 1;
        (0..dh)
            .map(|d| {
                let mut f = ((d as f64 + 0.5) * scale - 0.5) as f32;
                let i = f.floor() as i64;
                f -= i as f32;
                let b0 = ((1.0 - f) * SCALE).round_ties_even() as i32;
                let b1 = (f * SCALE).round_ties_even() as i32;
                (i.clamp(0, last) as usize, (i + 1).clamp(0, last) as usize, b0, b1)
            })
            .collect()
    };
    let raw = src.as_raw();
    let w = sw as usize;
    let hrow = |y: usize| -> Vec<i32> {
        let row = &raw[y * w * 3..(y + 1) * w * 3];
        let mut out = vec![0i32; dw as usize * 3];
        for (dx, &(sx, a0, a1)) in xt.iter().enumerate() {
            let sx1 = (sx + 1).min(w - 1);
            for c in 0..3 {
                out[dx * 3 + c] = row[sx * 3 + c] as i32 * a0 + row[sx1 * 3 + c] as i32 * a1;
            }
        }
        out
    };
    let mulhi = |a: i32, b: i32| -> i32 { ((a as i16 as i32) * (b as i16 as i32)) >> 16 };
    let mut out = RgbImage::new(dw, dh);
    let stride = dw as usize * 3;
    let o: &mut [u8] = &mut out;
    let mut cache: Option<(usize, Vec<i32>)> = None;
    for (dy, &(sy, sy1, b0, b1)) in yt.iter().enumerate() {
        let r0 = match &cache {
            Some((y, r)) if *y == sy => r.clone(),
            _ => hrow(sy),
        };
        let r1 = if sy1 == sy { r0.clone() } else { hrow(sy1) };
        for i in 0..stride {
            let v = mulhi(r0[i] >> 4, b0) + mulhi(r1[i] >> 4, b1);
            o[dy * stride + i] = ((v + 2) >> 2).clamp(0, 255) as u8;
        }
        cache = Some((sy1, r1));
    }
    out
}

/// Crop to a normalized [x1, y1, x2, y2] region (truncating like Python's int()).
pub fn crop_roi(image: &RgbImage, roi: Option<&[f64]>) -> RgbImage {
    let Some(r) = roi.filter(|r| r.len() == 4) else { return image.clone() };
    let (w, h) = (image.width() as f64, image.height() as f64);
    let (x1, y1, x2, y2) = ((r[0] * w) as u32, (r[1] * h) as u32, (r[2] * w) as u32, (r[3] * h) as u32);
    let (x2, y2) = (x2.min(image.width()), y2.min(image.height()));
    if x2 <= x1 || y2 <= y1 {
        return image.clone();
    }
    image::imageops::crop_imm(image, x1, y1, x2 - x1, y2 - y1).to_image()
}

pub fn encode_jpeg(img: &RgbImage, quality: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    if img.write_with_encoder(enc).is_err() {
        return Vec::new();
    }
    buf
}

pub fn fill_rect(img: &mut RgbImage, x0: i64, y0: i64, x1: i64, y1: i64, color: Rgb<u8>) {
    let (w, h) = (img.width() as i64, img.height() as i64);
    for y in y0.max(0)..=y1.min(h - 1) {
        for x in x0.max(0)..=x1.min(w - 1) {
            img.put_pixel(x as u32, y as u32, color);
        }
    }
}

/// Rectangle outline, `thickness` px, clipped to the image (cv2.rectangle).
pub fn draw_rect(img: &mut RgbImage, p1: (i64, i64), p2: (i64, i64), color: Rgb<u8>, thickness: i64) {
    let (x0, x1) = (p1.0.min(p2.0), p1.0.max(p2.0));
    let (y0, y1) = (p1.1.min(p2.1), p1.1.max(p2.1));
    let t = thickness.max(1);
    let lo = (t - 1) / 2;
    let hi = t / 2;
    fill_rect(img, x0 - lo, y0 - lo, x1 + hi, y0 + hi, color);
    fill_rect(img, x0 - lo, y1 - lo, x1 + hi, y1 + hi, color);
    fill_rect(img, x0 - lo, y0 - lo, x0 + hi, y1 + hi, color);
    fill_rect(img, x1 - lo, y0 - lo, x1 + hi, y1 + hi, color);
}

/// 8x8 bitmap text with its baseline at `y` (like cv2.putText's origin).
pub fn draw_text(img: &mut RgbImage, x: i64, y: i64, text: &str, color: Rgb<u8>) {
    use font8x8::UnicodeFonts;
    let top = y - 8;
    let (w, h) = (img.width() as i64, img.height() as i64);
    for (i, ch) in text.chars().enumerate() {
        let glyph = font8x8::BASIC_FONTS.get(ch).or_else(|| font8x8::BASIC_FONTS.get('?')).unwrap_or([0; 8]);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8 {
                if bits & (1 << col) != 0 {
                    let px = x + i as i64 * 8 + col;
                    let py = top + row as i64;
                    if px >= 0 && py >= 0 && px < w && py < h {
                        img.put_pixel(px as u32, py as u32, color);
                    }
                }
            }
        }
    }
}
