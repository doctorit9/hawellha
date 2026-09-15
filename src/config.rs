// SPDX-License-Identifier: MPL-2.0

use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};

/// Persisted user preferences for Hawellha.
///
/// Stored via `cosmic-config` under the application ID, so the output
/// directory, image quality and resize defaults survive restarts.
#[derive(Debug, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 2]
pub struct Config {
    /// Last-used output directory (empty = system default `~/Downloads`).
    pub output_dir: String,
    /// JPEG quality 1–100.
    pub jpeg_quality: u32,
    /// Default resize width in px (`0` = keep original).
    pub resize_width: u32,
    /// Default resize height in px (`0` = keep original).
    pub resize_height: u32,
    /// Overwrite existing output files instead of auto-renaming (`file_1.ext`).
    pub overwrite_existing: bool,
    /// Group outputs into subfolders per file kind (`images/`, `data/`, …).
    pub group_by_kind: bool,
    /// Automatically open the output folder when a batch finishes.
    pub auto_open_output: bool,
    /// Audio bitrate in kbps for lossy audio targets (mp3/aac/m4a/ogg).
    pub audio_bitrate_kbps: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output_dir: String::new(),
            jpeg_quality: 90,
            resize_width: 0,
            resize_height: 0,
            overwrite_existing: false,
            group_by_kind: false,
            auto_open_output: false,
            audio_bitrate_kbps: 192,
        }
    }
}
