//! Raster image conversion engine.
//!
//! Converts between PNG, JPEG, WebP, BMP, TIFF and GIF via the [`image`]
//! crate. Supports optional resizing (Lanczos3) and JPEG quality control.
//!
//! Notes:
//! - JPEG quality comes from [`ConvertOptions::jpeg_quality`][super::ConvertOptions].
//! - WebP output uses the crate's default lossless encoder; `webp_quality`
//!   is reserved for a future lossy path.
//! - Alpha channels are flattened onto white when the target (JPEG/BMP)
//!   cannot represent transparency — otherwise the encoder would fail.

use std::fs::File;
use std::path::Path;

use image::ImageFormat;
use image::imageops::FilterType;

use super::{ConversionError, ConvertOptions, Converter, ensure_parent_dir, extension_of};

/// Image engine implementing [`Converter`].
pub struct ImageConverter;

impl Converter for ImageConverter {
    fn convert(
        &self,
        input: &Path,
        output: &Path,
        opts: &ConvertOptions,
    ) -> Result<(), ConversionError> {
        ensure_parent_dir(output)?;
        convert_image(input, output, opts)
    }
}

/// Extensions handled by this engine (source or target).
const SUPPORTED: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "tiff", "tif", "gif"];

/// Returns `true` for image pairs handled by this engine.
pub fn supports(from: &str, to: &str) -> bool {
    let (from, to) = (normalise(from), normalise(to));
    SUPPORTED.contains(&from.as_str())
        && SUPPORTED.contains(&to.as_str())
        && from != to
}

/// UI-ordered target list for an image source extension.
pub fn targets_for(from: &str) -> Vec<String> {
    let from = normalise(from);
    ["png", "jpg", "webp", "bmp", "tiff", "gif"]
        .into_iter()
        .map(str::to_string)
        .filter(|t| *t != from)
        .collect()
}

fn normalise(ext: &str) -> String {
    match ext.to_ascii_lowercase().as_str() {
        "tif" => "tiff".to_string(),
        "jpeg" => "jpg".to_string(),
        other => other.to_string(),
    }
}

fn image_format_for(ext: &str) -> Option<ImageFormat> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "webp" => Some(ImageFormat::WebP),
        "bmp" => Some(ImageFormat::Bmp),
        "tiff" | "tif" => Some(ImageFormat::Tiff),
        "gif" => Some(ImageFormat::Gif),
        _ => None,
    }
}

fn convert_image(
    input: &Path,
    output: &Path,
    opts: &ConvertOptions,
) -> Result<(), ConversionError> {
    let in_label = input.display().to_string();
    let out_label = output.display().to_string();
    let from = extension_of(input);
    let to = extension_of(output);

    let out_format = image_format_for(&to).ok_or_else(|| ConversionError::UnsupportedConversion {
        from: from.clone(),
        to: to.clone(),
    })?;

    // Decode: `image::open` sniffs magic bytes, so a mislabelled extension
    // still decodes when the content is a supported format.
    let mut img =
        image::open(input).map_err(|e| ConversionError::MalformedData {
            path: in_label.clone(),
            format: format!("image ({from})"),
            reason: e.to_string(),
        })?;

    // Optional resize. When only one dimension is given, preserve aspect
    // ratio via the image crate's thumbnail-style scaling.
    match (opts.resize_width, opts.resize_height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            img = img.resize_exact(w, h, FilterType::Lanczos3);
        }
        (Some(w), _) if w > 0 => {
            img = img.resize(w, u32::MAX, FilterType::Lanczos3);
        }
        (_, Some(h)) if h > 0 => {
            img = img.resize(u32::MAX, h, FilterType::Lanczos3);
        }
        _ => {}
    }

    // JPEG/BMP have no alpha: flatten onto white instead of erroring.
    let needs_flatten = matches!(out_format, ImageFormat::Jpeg | ImageFormat::Bmp)
        && matches!(img.color(), image::ColorType::Rgba8 | image::ColorType::Rgba16 | image::ColorType::Rgba32F | image::ColorType::La8 | image::ColorType::La16);
    if needs_flatten {
        let rgb = img.to_rgb8();
        // Paint over a white canvas honouring… (to_rgb8 already blends?
        // No — it drops alpha. Composite manually for correctness.)
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        let mut canvas = image::RgbImage::from_pixel(w, h, image::Rgb([255, 255, 255]));
        for (x, y, px) in rgba.enumerate_pixels() {
            let a = px[3] as f32 / 255.0;
            let blended = image::Rgb([
                (px[0] as f32 * a + 255.0 * (1.0 - a)) as u8,
                (px[1] as f32 * a + 255.0 * (1.0 - a)) as u8,
                (px[2] as f32 * a + 255.0 * (1.0 - a)) as u8,
            ]);
            canvas.put_pixel(x, y, blended);
        }
        img = image::DynamicImage::ImageRgb8(canvas);
        let _ = rgb; // (kept for clarity of intent)
    }

    // Encode. JPEG gets explicit quality control; everything else uses the
    // crate's default encoder via `save_with_format` equivalent.
    if matches!(out_format, ImageFormat::Jpeg) {
        let quality = opts.jpeg_quality.clamp(1, 100);
        let file = File::create(output).map_err(|e| ConversionError::UnwritableOutput {
            path: out_label.clone(),
            reason: e.to_string(),
        })?;
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, quality);
        encoder.encode_image(&img).map_err(|e| {
            ConversionError::UnwritableOutput {
                path: out_label.clone(),
                reason: e.to_string(),
            }
        })?;
    } else {
        // `save_with_format` doesn't exist on DynamicImage in 0.25 under
        // no-default-features combos, so encode via explicit encoders where
        // cheap, else fall back to `save` (extension-sniffed).
        img.save_with_format(output, out_format).map_err(|e| {
            ConversionError::UnwritableOutput {
                path: out_label.clone(),
                reason: e.to_string(),
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix() {
        assert!(supports("png", "jpg"));
        assert!(supports("jpg", "webp"));
        assert!(!supports("png", "png"));
        assert!(!supports("png", "mp3"));
    }

    #[test]
    fn png_to_bmp_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "hawellha_img_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        let dst = dir.join("a.bmp");
        let img = image::DynamicImage::new_rgb8(8, 8);
        img.save_with_format(&src, ImageFormat::Png).unwrap();
        ImageConverter
            .convert(&src, &dst, &ConvertOptions::default())
            .expect("png->bmp");
        assert!(dst.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
