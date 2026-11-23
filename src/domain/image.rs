//! Shrinks uploaded images before they are stored, to keep object storage
//! small: decode, apply the EXIF orientation, downscale so the longest side
//! fits `max_dimension`, and re-encode as lossy WebP. Re-encoding also strips
//! metadata (EXIF GPS, camera info).
//!
//! GIFs are passed through untouched so animations survive. When the
//! re-encoded image would not be smaller than the upload, or the image cannot
//! be decoded, the original bytes are kept (the caller already sniffed the
//! magic bytes, which is all echobackend checks).
//!
//! CPU-bound: call from `spawn_blocking`.

use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use std::io::Cursor;

/// Lossy WebP quality (0-100): visually close to the source, ~3-10x smaller
/// than phone JPEGs.
const WEBP_QUALITY: f32 = 80.0;
/// Refuse to decode anything larger than this (decompression-bomb guard).
const MAX_DECODE_DIMENSION: u32 = 10_000;
const MAX_DECODE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

/// Result of [`optimize`]: the bytes to store and their type.
#[derive(Debug)]
pub struct OptimizedImage {
    pub data: Vec<u8>,
    pub content_type: &'static str,
    pub ext: &'static str,
}

/// Returns a re-encoded WebP when it beats the original, `None` to keep the
/// original (GIF, undecodable, or no size gain).
pub fn optimize(data: &[u8], content_type: &str, max_dimension: u32) -> Option<OptimizedImage> {
    if content_type == "image/gif" {
        return None;
    }

    let img = match decode_oriented(data) {
        Ok(img) => img,
        Err(err) => {
            tracing::warn!(error = %err, content_type, "image optimize: decode failed, keeping original");
            return None;
        }
    };

    let img = if img.width() > max_dimension || img.height() > max_dimension {
        img.resize(
            max_dimension,
            max_dimension,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };

    let encoded = encode_webp(&img);
    if encoded.len() >= data.len() {
        return None;
    }
    Some(OptimizedImage {
        data: encoded,
        content_type: "image/webp",
        ext: ".webp",
    })
}

fn decode_oriented(data: &[u8]) -> image::ImageResult<DynamicImage> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);

    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    // Phone photos are often stored sideways with an EXIF rotation tag; the
    // re-encoded file has no EXIF, so bake the rotation into the pixels.
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

fn encode_webp(img: &DynamicImage) -> Vec<u8> {
    let (w, h) = (img.width(), img.height());
    let memory = if img.color().has_alpha() {
        webp::Encoder::from_rgba(img.to_rgba8().as_raw(), w, h).encode(WEBP_QUALITY)
    } else {
        webp::Encoder::from_rgb(img.to_rgb8().as_raw(), w, h).encode(WEBP_QUALITY)
    };
    memory.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

    /// A noisy image so encoders cannot shrink it to nothing.
    fn noisy_rgb(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            let v = x.wrapping_mul(31) ^ y.wrapping_mul(17);
            Rgb([v as u8, (v >> 3) as u8, (x + y) as u8])
        })
    }

    fn encode(img: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn decode(data: &[u8]) -> DynamicImage {
        image::load_from_memory(data).unwrap()
    }

    #[test]
    fn large_png_is_downscaled_to_webp() {
        let png = encode(
            DynamicImage::ImageRgb8(noisy_rgb(2400, 1200)),
            ImageFormat::Png,
        );
        let out = optimize(&png, "image/png", 1600).expect("should optimize");
        assert_eq!(out.content_type, "image/webp");
        assert_eq!(out.ext, ".webp");
        assert!(out.data.len() < png.len());
        let img = decode(&out.data);
        assert_eq!((img.width(), img.height()), (1600, 800));
    }

    #[test]
    fn alpha_is_preserved() {
        let rgba = RgbaImage::from_fn(800, 800, |x, y| {
            Rgba([x as u8, y as u8, 0, (x % 256) as u8])
        });
        let png = encode(DynamicImage::ImageRgba8(rgba), ImageFormat::Png);
        let out = optimize(&png, "image/png", 1600).expect("should optimize");
        assert!(decode(&out.data).color().has_alpha());
    }

    #[test]
    fn gif_is_passed_through() {
        assert!(optimize(b"GIF89a\x01\x00", "image/gif", 1600).is_none());
    }

    #[test]
    fn garbage_keeps_original() {
        assert!(optimize(b"\x89PNG\r\n\x1a\nnot really", "image/png", 1600).is_none());
    }

    #[test]
    fn no_gain_keeps_original() {
        // A tiny flat image: the WebP container overhead beats any saving.
        let png = encode(
            DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, Rgb([0, 0, 0]))),
            ImageFormat::Png,
        );
        if let Some(out) = optimize(&png, "image/png", 1600) {
            assert!(out.data.len() < png.len());
        }
    }
}
