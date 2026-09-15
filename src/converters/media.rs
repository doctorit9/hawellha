//! Media engine: audio conversion, video→audio extraction, video remux.
//!
//! All work is delegated to an external `ffmpeg` binary executed as a
//! subprocess. Rationale: `ffmpeg-next` links against system FFmpeg at
//! build time (fragile vendoring, slow builds), while a subprocess keeps
//! Hawellha dependency-light and automatically supports whatever codecs the
//! user's FFmpeg was built with.
//!
//! Behaviour:
//! - Audio → audio: `ffmpeg -y -i in out` (re-encode by extension).
//! - Video → audio: same invocation; FFmpeg drops the video stream when the
//!   target is audio-only (plus `-vn` for explicitness).
//! - Video → video: stream-copy repackaging (`-c copy`) when possible,
//!   which is fast and lossless; falls back to re-encode on failure.
//! - Missing `ffmpeg` produces a clear [`ConversionError::ExternalTool`]
//!   telling the user how to install it.

use std::path::Path;
use std::process::Stdio;

use super::{ConversionError, ConvertOptions, Converter, ensure_parent_dir, extension_of};

/// Media engine implementing [`Converter`] (blocking subprocess).
pub struct MediaConverter;

impl Converter for MediaConverter {
    fn convert(
        &self,
        input: &Path,
        output: &Path,
        opts: &ConvertOptions,
    ) -> Result<(), ConversionError> {
        ensure_parent_dir(output)?;
        convert_media(input, output, opts.audio_bitrate_kbps)
    }
}

const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "ogg", "oga", "flac", "aac", "m4a", "opus", "wma",
];
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "avi", "mov", "m4v", "flv", "ogv",
];

/// Audio → audio pairs.
pub fn supports_audio(from: &str, to: &str) -> bool {
    let (f, t) = (from.to_ascii_lowercase(), to.to_ascii_lowercase());
    AUDIO_EXTS.contains(&f.as_str()) && AUDIO_EXTS.contains(&t.as_str()) && f != t
}

/// Video → audio extraction (e.g. `mp4 → mp3`).
pub fn supports_video_to_audio(from: &str, to: &str) -> bool {
    let (f, t) = (from.to_ascii_lowercase(), to.to_ascii_lowercase());
    VIDEO_EXTS.contains(&f.as_str()) && AUDIO_EXTS.contains(&t.as_str())
}

/// Video container repackaging (`mp4 → mkv`, …).
pub fn supports_video_remux(from: &str, to: &str) -> bool {
    let (f, t) = (from.to_ascii_lowercase(), to.to_ascii_lowercase());
    VIDEO_EXTS.contains(&f.as_str()) && VIDEO_EXTS.contains(&t.as_str()) && f != t
}

/// UI targets for an audio source.
pub fn audio_targets_for(from: &str) -> Vec<String> {
    let from = from.to_ascii_lowercase();
    AUDIO_EXTS
        .iter()
        .map(|s| s.to_string())
        .filter(|t| *t != from)
        .collect()
}

/// UI targets for a video source: audio extraction + other containers.
pub fn video_targets_for(from: &str) -> Vec<String> {
    let from = from.to_ascii_lowercase();
    let mut out: Vec<String> = vec!["mp3".to_string(), "wav".to_string(), "ogg".to_string()];
    for v in VIDEO_EXTS {
        if *v != from {
            out.push(v.to_string());
        }
    }
    out
}

/// Locate the `ffmpeg` binary or explain how to install it.
pub fn ffmpeg_status() -> Result<String, ConversionError> {
    match std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => {
            let line = String::from_utf8_lossy(&output.stdout);
            Ok(line.lines().next().unwrap_or("ffmpeg").to_string())
        }
        _ => Err(ConversionError::ExternalTool(
            "ffmpeg not found. Install it to enable audio/video conversion \
             (e.g. `sudo dnf install ffmpeg` on Fedora, `sudo apt install ffmpeg` \
             on Ubuntu, or via Flathub)."
                .to_string(),
        )),
    }
}

fn convert_media(input: &Path, output: &Path, bitrate_kbps: u32) -> Result<(), ConversionError> {
    let from = extension_of(input);
    let to = extension_of(output);

    // Fail fast with install instructions when ffmpeg is absent.
    ffmpeg_status()?;

    let is_video_in = VIDEO_EXTS.contains(&from.as_str());
    let is_audio_out = AUDIO_EXTS.contains(&to.as_str());
    let is_video_out = VIDEO_EXTS.contains(&to.as_str());

    // Strategy:
    // 1. video→video: try `-c copy` repackaging first (instant, lossless).
    // 2. everything else (or copy failure): full re-encode.
    if is_video_in && is_video_out {
        if let Ok(()) = run_ffmpeg(input, output, &["-c", "copy"]) {
            return Ok(());
        }
        // Fall through to re-encode.
    }

    let mut extra: Vec<String> = Vec::new();
    if is_video_in && is_audio_out {
        extra.push("-vn".to_string()); // drop video stream explicitly
    }
    // Configurable audio bitrate so `wav → mp3` can trade size vs quality.
    if is_audio_out && (to == "mp3" || to == "aac" || to == "m4a" || to == "ogg" || to == "opus" || to == "wma") {
        let kbps = bitrate_kbps.clamp(64, 320);
        extra.push("-b:a".to_string());
        extra.push(format!("{kbps}k"));
    }

    let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    run_ffmpeg(input, output, &extra_refs)
}

/// Run `ffmpeg -y -v error -i input [args…] output`, mapping failures.
fn run_ffmpeg(input: &Path, output: &Path, extra_args: &[&str]) -> Result<(), ConversionError> {
    let out_label = output.display().to_string();
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(input);
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.arg(output);
    cmd.stdin(Stdio::null());

    let output_result = cmd.output().map_err(|e| {
        ConversionError::ExternalTool(format!("failed to launch ffmpeg: {e}"))
    })?;

    if output_result.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output_result.stderr);
        let detail = stderr.lines().last().unwrap_or("unknown ffmpeg error");
        Err(ConversionError::ExternalTool(format!(
            "ffmpeg could not convert '{}' ({}). {detail}",
            input
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("input"),
            out_label,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix() {
        assert!(supports_audio("mp3", "wav"));
        assert!(supports_video_to_audio("mp4", "mp3"));
        assert!(supports_video_remux("mp4", "mkv"));
        assert!(!supports_audio("mp3", "mp3"));
        assert!(!supports_audio("mp3", "xlsx"));
    }
}
