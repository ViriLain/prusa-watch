//! Image helpers: OpenCV-compatible resize, ROI crop, JPEG, and annotation.

use image::{Rgb, RgbImage};

/// Portable OpenCV INTER_LINEAR_EXACT for 8-bit RGB: 8-bit interpolation weights
/// and one rounded 16-bit weighted sum. Coefficients use exact rational coordinates,
/// avoiding the architecture-dependent SIMD rounding of ordinary INTER_LINEAR.
/// Reference: opencv/modules/imgproc/src/resize.cpp (resize_bitExact).
pub fn resize_linear(src: &RgbImage, dw: u32, dh: u32) -> RgbImage {
    let (sw, sh) = src.dimensions();
    assert!(
        sw > 0 && sh > 0 && dw > 0 && dh > 0,
        "resize dimensions must be positive"
    );
    let table = |source: u32, destination: u32| -> Vec<(u32, u32, u32)> {
        let denominator = 2 * i64::from(destination);
        (0..destination)
            .map(|position| {
                let numerator = (2 * i64::from(position) + 1) * i64::from(source) - i64::from(destination);
                let index = numerator.div_euclid(denominator);
                if index < 0 {
                    return (0, 0, 0);
                }
                if index >= i64::from(source) - 1 {
                    return (source - 1, source - 1, 0);
                }
                let scaled = numerator.rem_euclid(denominator) * 256;
                let quotient = scaled / denominator;
                let remainder = scaled % denominator;
                let round_up = remainder * 2 > denominator || (remainder * 2 == denominator && quotient % 2 != 0);
                (index as u32, index as u32 + 1, (quotient + i64::from(round_up)) as u32)
            })
            .collect()
    };
    let xt = table(sw, dw);
    let yt = table(sh, dh);
    RgbImage::from_fn(dw, dh, |x, y| {
        let (x0, x1, wx) = xt[x as usize];
        let (y0, y1, wy) = yt[y as usize];
        Rgb(std::array::from_fn(|channel| {
            let upper =
                u32::from(src.get_pixel(x0, y0)[channel]) * (256 - wx) + u32::from(src.get_pixel(x1, y0)[channel]) * wx;
            let lower =
                u32::from(src.get_pixel(x0, y1)[channel]) * (256 - wx) + u32::from(src.get_pixel(x1, y1)[channel]) * wx;
            ((upper * (256 - wy) + lower * wy + 32768) >> 16) as u8
        }))
    })
}

/// Crop to a normalized [x1, y1, x2, y2] region (truncating like Python's int()).
pub fn crop_roi(image: &RgbImage, roi: Option<&[f64]>) -> RgbImage {
    let Some(r) = roi.filter(|r| r.len() == 4) else {
        return image.clone();
    };
    let (w, h) = (image.width() as f64, image.height() as f64);
    let (x1, y1, x2, y2) = (
        (r[0] * w) as u32,
        (r[1] * h) as u32,
        (r[2] * w) as u32,
        (r[3] * h) as u32,
    );
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
        let glyph = font8x8::BASIC_FONTS
            .get(ch)
            .or_else(|| font8x8::BASIC_FONTS.get('?'))
            .unwrap_or([0; 8]);
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
