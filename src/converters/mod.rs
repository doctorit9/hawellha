//! Conversion engine registry for Hawellha.
//!
//! This module defines the shared [`Converter`] trait, the unified
//! [`ConversionError`] type, file-kind detection, the supported
//! conversion matrix, and the top-level [`convert_file`] dispatcher
//! used by the GUI worker tasks.
//!
//! Concrete engines live in sibling modules:
//! - [`crate::converters::tabular`] — CSV / XLSX / JSON
//! - [`crate::converters::image`] — PNG / JPEG / WebP / BMP / TIFF / GIF
//! - [`crate::converters::media`] — audio/video via `ffmpeg`
//! - [`crate::converters::document`] — TXT/MD ↔ PDF, PDF → TXT
//! - [`crate::converters::archive`] — ZIP / TAR.GZ packaging

pub mod archive;
pub mod document;
pub mod image;
pub mod media;
pub mod tabular;

use std::path::{Path, PathBuf};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Every failure a converter can report.
///
/// Variants are deliberately user-facing: their `Display` strings are shown
/// in the queue cards and toast notifications, so they avoid jargon and
/// always mention the file or the missing tool involved.
#[derive(Debug, Error)]
pub enum ConversionError {
    /// The input file could not be read (missing, permission denied, …).
    #[error("cannot read input file '{path}': {reason}")]
    UnreadableInput { path: String, reason: String },

    /// The output file could not be written.
    #[error("cannot write output file '{path}': {reason}")]
    UnwritableOutput { path: String, reason: String },

    /// The input content is malformed for its claimed format.
    #[error("malformed {format} data in '{path}': {reason}")]
    MalformedData {
        path: String,
        format: String,
        reason: String,
    },

    /// The requested conversion pair is not supported.
    #[error("unsupported conversion: {from} → {to}")]
    UnsupportedConversion { from: String, to: String },

    /// An external helper (currently only `ffmpeg`) is missing or failed.
    #[error("external tool failed: {0}")]
    ExternalTool(String),

    /// Underlying I/O error, wrapped with context by the caller.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// File kinds & format helpers
// ---------------------------------------------------------------------------

/// Broad domain a file extension belongs to. Used for tab filtering,
/// per-file target lists, and validation of conversion pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Tabular,
    Image,
    Audio,
    Video,
    Document,
    Archive,
    Unknown,
}

/// Global conversion options shared by all engines.
///
/// The GUI exposes the image-related fields in the settings / footer area;
/// the tabular delimiter is auto-detected but can be overridden here.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// JPEG quality 1–100 used when the target is `.jpg`/`.jpeg`.
    pub jpeg_quality: u8,
    /// Kept for API symmetry; the `image` crate's WebP encoder is
    /// lossless-by-default so this currently gates nothing. Reserved.
    #[allow(dead_code)]
    pub webp_quality: u8,
    /// Optional resize width (pixels). `None` keeps the source width.
    pub resize_width: Option<u32>,
    /// Optional resize height (pixels). `None` keeps the source height.
    pub resize_height: Option<u32>,
    /// CSV delimiter override. `None` = auto-detect (`,`, `;`, `\t`, `|`).
    pub csv_delimiter: Option<u8>,
    /// Audio bitrate in kbps for lossy audio targets (mp3/aac/m4a/ogg).
    /// Passed to ffmpeg as `-b:a <N>k`. Clamped to 64–320 at use time.
    pub audio_bitrate_kbps: u32,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            jpeg_quality: 90,
            webp_quality: 90,
            resize_width: None,
            resize_height: None,
            csv_delimiter: None,
            audio_bitrate_kbps: 192,
        }
    }
}

/// Lowercase extension without the leading dot, e.g. `"xlsx"`.
pub fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Classify an extension into a [`FileKind`].
pub fn kind_of_extension(ext: &str) -> FileKind {
    match ext {
        // Tabular / data
        "csv" | "tsv" | "xlsx" | "xls" | "xlsm" | "xlsb" | "ods" | "json" => FileKind::Tabular,
        // Raster images
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tiff" | "tif" | "gif" => FileKind::Image,
        // Audio
        "mp3" | "wav" | "ogg" | "oga" | "flac" | "aac" | "m4a" | "opus" | "wma" => {
            FileKind::Audio
        }
        // Video (also convertible *to* audio via extraction)
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "m4v" | "flv" | "ogv" => FileKind::Video,
        // Documents (HTML is output-only from txt/md; html→txt strips tags)
        "pdf" | "txt" | "md" | "markdown" | "html" => FileKind::Document,
        // Archives
        "zip" | "tar" | "tgz" | "tar.gz" | "gz" => FileKind::Archive,
        _ => FileKind::Unknown,
    }
}

/// Classify a full path via its extension.
pub fn kind_of_path(path: &Path) -> FileKind {
    // Handle compound `.tar.gz` explicitly: `Path::extension` sees only `gz`.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            return FileKind::Archive;
        }
    }
    kind_of_extension(&extension_of(path))
}

/// MIME guess for display purposes (icon choice, badges).
pub fn mime_of(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Supported conversion matrix
// ---------------------------------------------------------------------------

/// Returns `true` when `from_ext → to_ext` is a conversion we can perform.
///
/// The GUI calls this for instant validation: unsupported pairs (e.g.
/// audio → xlsx) are flagged inline instead of failing after "Convert".
pub fn is_supported(from_ext: &str, to_ext: &str) -> bool {
    let from = from_ext.to_ascii_lowercase();
    let to = to_ext.to_ascii_lowercase();
    if from == to {
        return false; // same-format "conversion" is a no-op; force a real choice
    }
    match (kind_of_extension(&from), kind_of_extension(&to)) {
        (FileKind::Tabular, FileKind::Tabular) => tabular::supports(&from, &to),
        (FileKind::Image, FileKind::Image) => image::supports(&from, &to),
        (FileKind::Audio, FileKind::Audio) => media::supports_audio(&from, &to),
        // Video → audio extraction (mp4 → mp3) plus audio → audio.
        (FileKind::Video, FileKind::Audio) => media::supports_video_to_audio(&from, &to),
        (FileKind::Video, FileKind::Video) => media::supports_video_remux(&from, &to),
        (FileKind::Document, FileKind::Document) => document::supports(&from, &to),
        // Anything → zip/tar.gz packaging, plus archive extraction targets.
        (_, FileKind::Archive) => archive::supports_pack(&to),
        (FileKind::Archive, _) => archive::supports_unpack(&from),
        _ => false,
    }
}

/// Target formats offered for a given source extension, in UI order.
pub fn targets_for(from_ext: &str) -> Vec<String> {
    let from = from_ext.to_ascii_lowercase();
    match kind_of_extension(&from) {
        FileKind::Tabular => tabular::targets_for(&from),
        FileKind::Image => image::targets_for(&from),
        FileKind::Audio => media::audio_targets_for(&from),
        FileKind::Video => media::video_targets_for(&from),
        FileKind::Document => document::targets_for(&from),
        FileKind::Archive => archive::unpack_targets_for(&from),
        FileKind::Unknown => {
            // Unknown inputs can still be packaged into an archive.
            vec!["zip".to_string(), "tar.gz".to_string()]
        }
    }
}

/// All target formats the global batch dropdown may offer for a mixed queue.
/// Only formats valid for *at least one* queued file are listed; per-file
/// validation still happens at conversion time.
pub fn batch_targets_for(exts: &[String]) -> Vec<String> {
    let mut ordered: Vec<String> = Vec::new();
    for ext in exts {
        for t in targets_for(ext) {
            if !ordered.contains(&t) {
                ordered.push(t);
            }
        }
    }
    ordered
}

// ---------------------------------------------------------------------------
// Converter trait
// ---------------------------------------------------------------------------

/// Synchronous conversion contract implemented by every engine.
///
/// Implementations must be blocking-safe (they run inside
/// `tokio::task::spawn_blocking` from the GUI) and must never panic on
/// malformed input — all failures come back as [`ConversionError`].
pub trait Converter: Send + Sync {
    /// Convert `input` into `output` using `opts`.
    fn convert(
        &self,
        input: &Path,
        output: &Path,
        opts: &ConvertOptions,
    ) -> Result<(), ConversionError>;
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Convert one file, routing to the correct engine by extension pair.
///
/// `output` must already be the final destination path (parent directory
/// created by the caller or by the engine). On success the output file
/// exists; on failure a [`ConversionError`] explains why.
pub fn convert_file(
    input: &Path,
    output: &Path,
    opts: &ConvertOptions,
) -> Result<(), ConversionError> {
    let from = extension_of(input);
    // `tar.gz` outputs: `Path::extension` yields `gz`, so recover compound ext.
    let to = compound_extension_of(output);

    if from.is_empty() {
        return Err(ConversionError::UnsupportedConversion {
            from: "(no extension)".to_string(),
            to: to.clone(),
        });
    }

    if !is_supported(&from, &to) {
        return Err(ConversionError::UnsupportedConversion {
            from: from.clone(),
            to: to.clone(),
        });
    }

    if !input.is_file() {
        return Err(ConversionError::UnreadableInput {
            path: input.display().to_string(),
            reason: "file does not exist or is not a regular file".to_string(),
        });
    }

    match (kind_of_extension(&from), kind_of_extension(&to)) {
        (FileKind::Tabular, FileKind::Tabular) => {
            tabular::TabularConverter.convert(input, output, opts)
        }
        (FileKind::Image, FileKind::Image) => image::ImageConverter.convert(input, output, opts),
        (FileKind::Audio, FileKind::Audio)
        | (FileKind::Video, FileKind::Audio)
        | (FileKind::Video, FileKind::Video) => media::MediaConverter.convert(input, output, opts),
        (FileKind::Document, FileKind::Document) => {
            document::DocumentConverter.convert(input, output, opts)
        }
        (_, FileKind::Archive) => archive::ArchiveConverter.convert(input, output, opts),
        // Archive → directory: `output` is the destination *folder*.
        (FileKind::Archive, _) => archive::ArchiveConverter.convert(input, output, opts),
        _ => Err(ConversionError::UnsupportedConversion {
            from: from.clone(),
            to: to.clone(),
        }),
    }
}

/// Output extension aware of compound `.tar.gz` names.
fn compound_extension_of(path: &Path) -> String {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".tar.gz") {
            return "tar.gz".to_string();
        }
        if lower.ends_with(".tgz") {
            return "tar.gz".to_string();
        }
    }
    extension_of(path)
}

/// Build the default output path for `input` + `target_ext` inside `dir`.
///
/// Example: `/a/report.csv` + `xlsx` → `<dir>/report.xlsx`.
/// For archive extraction (`zip → dir`) the "extension" is stripped instead:
/// `/a/files.zip` → `<dir>/files/`.
pub fn resolve_output_path(input: &Path, target_ext: &str, dir: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("converted");
    // `report.tar` stem is `report`; `archive.tar.gz` stem is `archive.tar`,
    // so strip a second time for the compound case.
    let stem = stem
        .strip_suffix(".tar")
        .map(|s| {
            // `Path::file_stem` on `archive.tar.gz` gives `archive.tar`.
            s.to_string()
        })
        .unwrap_or_else(|| stem.to_string());

    let target = target_ext.to_ascii_lowercase();
    if target.is_empty() || target == "unzip" || target == "extract" {
        // Extraction pseudo-targets produce a directory.
        return dir.join(stem);
    }
    // Normalise `tgz` → canonical `tar.gz` file name.
    if target == "tgz" {
        return dir.join(format!("{stem}.tar.gz"));
    }
    dir.join(format!("{stem}.{target}"))
}

/// Ensure the parent directory of `path` exists (created recursively).
pub fn ensure_parent_dir(path: &Path) -> Result<(), ConversionError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| ConversionError::UnwritableOutput {
                path: parent.display().to_string(),
                reason: e.to_string(),
            })?;
        }
    }
    Ok(())
}

/// Default output directory: `~/Downloads`, falling back to the OS temp dir.
pub fn default_output_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_spot_checks() {
        assert!(is_supported("csv", "xlsx"));
        assert!(is_supported("xlsx", "csv"));
        assert!(is_supported("json", "csv"));
        assert!(is_supported("png", "jpg"));
        assert!(is_supported("mp4", "mp3"));
        assert!(is_supported("txt", "pdf"));
        assert!(is_supported("md", "html"));
        assert!(is_supported("txt", "html"));
        assert!(is_supported("html", "txt"));
        assert!(!is_supported("mp3", "xlsx"));
        assert!(!is_supported("png", "mp3"));
        assert!(!is_supported("csv", "csv"));
    }

    #[test]
    fn output_paths() {
        let dir = Path::new("/tmp/out");
        assert_eq!(
            resolve_output_path(Path::new("/a/report.csv"), "xlsx", dir),
            PathBuf::from("/tmp/out/report.xlsx")
        );
        assert_eq!(
            resolve_output_path(Path::new("/a/photo.PNG"), "jpg", dir),
            PathBuf::from("/tmp/out/photo.jpg")
        );
        assert_eq!(
            resolve_output_path(Path::new("/a/files.zip"), "extract", dir),
            PathBuf::from("/tmp/out/files")
        );
    }
}
