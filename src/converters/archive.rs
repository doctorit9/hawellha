//! Archive engine: packaging (`→ zip|tar.gz`) and extraction.
//!
//! Two directions:
//! - **Pack**: any regular file (or, in the future, a directory) → `.zip`
//!   or `.tar.gz`. The GUI always passes a single input file; batch jobs
//!   produce one archive per input.
//! - **Unpack**: `.zip` / `.tar.gz` / `.tgz` / `.tar` → destination folder.
//!   Zip-slip paths (`../evil`) are rejected, and extraction never writes
//!   outside the destination directory.
//!
//! The GUI represents extraction targets with the pseudo-extension
//! `extract` (resolved to a folder by
//! [`resolve_output_path`][super::resolve_output_path]); this engine also
//! accepts literal `zip → <dir>` calls where `output` is a directory.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::{ConversionError, ConvertOptions, Converter, ensure_parent_dir};

/// Archive engine implementing [`Converter`].
pub struct ArchiveConverter;

impl Converter for ArchiveConverter {
    fn convert(
        &self,
        input: &Path,
        output: &Path,
        _opts: &ConvertOptions,
    ) -> Result<(), ConversionError> {
        convert_archive(input, output)
    }
}

/// Pack targets offered for any packable input.
pub fn supports_pack(to: &str) -> bool {
    matches!(to, "zip" | "tar.gz" | "tgz" | "tar" | "gz")
}

/// Whether `from` is an archive we can unpack.
pub fn supports_unpack(from: &str) -> bool {
    matches!(from, "zip" | "tar.gz" | "tgz" | "tar" | "gz")
}

/// Pseudo-targets shown for an archive source (extraction only).
pub fn unpack_targets_for(_from: &str) -> Vec<String> {
    vec!["extract".to_string()]
}

fn convert_archive(input: &Path, output: &Path) -> Result<(), ConversionError> {
    let in_label = input.display().to_string();
    if !input.exists() {
        return Err(ConversionError::UnreadableInput {
            path: in_label,
            reason: "file does not exist".to_string(),
        });
    }

    let from = super::extension_of(input);
    let from_compound = compound_ext(input);

    // Direction 1: input is an archive → extract to `output` dir.
    if supports_unpack(&from) || supports_unpack(&from_compound) {
        let dest_dir = if output.extension().is_some() && looks_like_file_target(output) {
            // Caller gave a file path for extraction: use its parent + stem.
            output
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(
                    output
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("extracted"),
                )
        } else {
            output.to_path_buf()
        };
        return extract_archive(input, &dest_dir);
    }

    // Direction 2: pack a single file into an archive file.
    let to = compound_ext(output);
    ensure_parent_dir(output)?;
    match to.as_str() {
        "zip" => pack_zip(input, output),
        "tar.gz" | "tgz" => pack_tar_gz(input, output),
        "tar" => pack_tar(input, output),
        _ => Err(ConversionError::UnsupportedConversion {
            from: from_compound,
            to,
        }),
    }
}

/// Extension aware of `.tar.gz` / `.tgz` compound names.
fn compound_ext(path: &Path) -> String {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".tar.gz") {
            return "tar.gz".to_string();
        }
        if lower.ends_with(".tgz") {
            return "tar.gz".to_string();
        }
    }
    super::extension_of(path)
}

/// Heuristic: `output` with a known archive/file extension is a file target;
/// extensionless or `extract`-style paths are directories.
fn looks_like_file_target(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => !matches!(ext.to_ascii_lowercase().as_str(), "zip" | "gz" | "tar"),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Packing
// ---------------------------------------------------------------------------

fn pack_zip(input: &Path, output: &Path) -> Result<(), ConversionError> {
    let out_label = output.display().to_string();
    let in_label = input.display().to_string();

    let file = File::create(output).map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    if input.is_dir() {
        pack_dir_as_zip(&mut zip, input, input)?;
    } else {
        let name = input
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        zip.start_file(name, options)
            .map_err(|e| ConversionError::UnwritableOutput {
                path: out_label.clone(),
                reason: e.to_string(),
            })?;
        let mut src = File::open(input).map_err(|e| ConversionError::UnreadableInput {
            path: in_label.clone(),
            reason: e.to_string(),
        })?;
        std::io::copy(&mut src, &mut zip).map_err(|e| ConversionError::UnreadableInput {
            path: in_label.clone(),
            reason: e.to_string(),
        })?;
    }

    zip.finish().map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    Ok(())
}

fn pack_dir_as_zip<W: Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    base: &Path,
    dir: &Path,
) -> Result<(), ConversionError> {
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let entries = std::fs::read_dir(dir).map_err(ConversionError::Io)?;
    for entry in entries {
        let entry = entry.map_err(ConversionError::Io)?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .map_err(|e| ConversionError::UnreadableInput {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        if path.is_dir() {
            zip.add_directory(rel.to_string_lossy().into_owned(), options)
                .map_err(|e| ConversionError::UnwritableOutput {
                    path: base.display().to_string(),
                    reason: e.to_string(),
                })?;
            pack_dir_as_zip(zip, base, &path)?;
        } else {
            zip.start_file(rel.to_string_lossy().into_owned(), options)
                .map_err(|e| ConversionError::UnwritableOutput {
                    path: base.display().to_string(),
                    reason: e.to_string(),
                })?;
            let mut src =
                File::open(&path).map_err(|e| ConversionError::UnreadableInput {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?;
            std::io::copy(&mut src, zip).map_err(|e| ConversionError::UnreadableInput {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        }
    }
    Ok(())
}

fn pack_tar_gz(input: &Path, output: &Path) -> Result<(), ConversionError> {
    let out_label = output.display().to_string();
    let file = File::create(output).map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);
    append_input_to_tar(&mut tar, input)?;
    let encoder = tar.into_inner().map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    encoder.finish().map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    Ok(())
}

fn pack_tar(input: &Path, output: &Path) -> Result<(), ConversionError> {
    let out_label = output.display().to_string();
    let file = File::create(output).map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    let mut tar = tar::Builder::new(file);
    append_input_to_tar(&mut tar, input)?;
    tar.into_inner().map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    Ok(())
}

fn append_input_to_tar<W: Write>(
    tar: &mut tar::Builder<W>,
    input: &Path,
) -> Result<(), ConversionError> {
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    if input.is_dir() {
        tar.append_dir_all(name, input)
            .map_err(|e| ConversionError::UnreadableInput {
                path: input.display().to_string(),
                reason: e.to_string(),
            })?;
    } else {
        let mut src = File::open(input).map_err(|e| ConversionError::UnreadableInput {
            path: input.display().to_string(),
            reason: e.to_string(),
        })?;
        tar.append_file(name, &mut src)
            .map_err(|e| ConversionError::UnreadableInput {
                path: input.display().to_string(),
                reason: e.to_string(),
            })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Extraction (zip-slip safe)
// ---------------------------------------------------------------------------

fn extract_archive(input: &Path, dest: &Path) -> Result<(), ConversionError> {
    std::fs::create_dir_all(dest).map_err(|e| ConversionError::UnwritableOutput {
        path: dest.display().to_string(),
        reason: e.to_string(),
    })?;
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.ends_with(".zip") {
        extract_zip(input, dest)
    } else {
        extract_tar(input, dest)
    }
}

fn extract_zip(input: &Path, dest: &Path) -> Result<(), ConversionError> {
    let in_label = input.display().to_string();
    let file = File::open(input).map_err(|e| ConversionError::UnreadableInput {
        path: in_label.clone(),
        reason: e.to_string(),
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| ConversionError::MalformedData {
        path: in_label.clone(),
        format: "ZIP".to_string(),
        reason: e.to_string(),
    })?;

    for i in 0..archive.len() {
        let mut entry =
            archive
                .by_index(i)
                .map_err(|e| ConversionError::MalformedData {
                    path: in_label.clone(),
                    format: "ZIP".to_string(),
                    reason: e.to_string(),
                })?;
        let entry_name = entry.name().to_string();
        let safe_path = safe_join(dest, &entry_name).ok_or_else(|| {
            ConversionError::MalformedData {
                path: in_label.clone(),
                format: "ZIP".to_string(),
                reason: format!("unsafe entry path rejected: '{entry_name}'"),
            }
        })?;
        if entry.is_dir() {
            std::fs::create_dir_all(&safe_path).map_err(|e| {
                ConversionError::UnwritableOutput {
                    path: safe_path.display().to_string(),
                    reason: e.to_string(),
                }
            })?;
        } else {
            if let Some(parent) = safe_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ConversionError::UnwritableOutput {
                        path: parent.display().to_string(),
                        reason: e.to_string(),
                    }
                })?;
            }
            let mut out =
                File::create(&safe_path).map_err(|e| ConversionError::UnwritableOutput {
                    path: safe_path.display().to_string(),
                    reason: e.to_string(),
                })?;
            std::io::copy(&mut entry, &mut out).map_err(|e| {
                ConversionError::UnwritableOutput {
                    path: safe_path.display().to_string(),
                    reason: e.to_string(),
                }
            })?;
        }
    }
    Ok(())
}

fn extract_tar(input: &Path, dest: &Path) -> Result<(), ConversionError> {
    let in_label = input.display().to_string();
    let file = File::open(input).map_err(|e| ConversionError::UnreadableInput {
        path: in_label.clone(),
        reason: e.to_string(),
    })?;
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_gz = name.ends_with(".gz") || name.ends_with(".tgz");

    if is_gz {
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        unpack_tar_entries(&mut archive, input, dest)
    } else {
        let mut archive = tar::Archive::new(file);
        unpack_tar_entries(&mut archive, input, dest)
    }
    .map_err(|e| match e {
        ConversionError::Io(io) => ConversionError::MalformedData {
            path: in_label.clone(),
            format: "TAR".to_string(),
            reason: io.to_string(),
        },
        other => other,
    })
}

fn unpack_tar_entries<R: Read>(
    archive: &mut tar::Archive<R>,
    input: &Path,
    dest: &Path,
) -> Result<(), ConversionError> {
    let in_label = input.display().to_string();
    let entries = archive
        .entries()
        .map_err(|e| ConversionError::MalformedData {
            path: in_label.clone(),
            format: "TAR".to_string(),
            reason: e.to_string(),
        })?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| ConversionError::MalformedData {
                path: in_label.clone(),
                format: "TAR".to_string(),
                reason: e.to_string(),
            })?;
        let entry_path: PathBuf = entry
            .path()
            .map_err(|e| ConversionError::MalformedData {
                path: in_label.clone(),
                format: "TAR".to_string(),
                reason: e.to_string(),
            })?
            .into_owned();
        let safe = safe_join(dest, &entry_path.to_string_lossy()).ok_or_else(|| {
            ConversionError::MalformedData {
                path: in_label.clone(),
                format: "TAR".to_string(),
                reason: format!("unsafe entry path rejected: '{}'", entry_path.display()),
            }
        })?;
        entry
            .unpack(&safe)
            .map_err(|e| ConversionError::UnwritableOutput {
                path: safe.display().to_string(),
                reason: e.to_string(),
            })?;
    }
    Ok(())
}

/// Join `entry` onto `dest`, returning `None` when the result would escape
/// `dest` (zip-slip / tar-slip protection).
fn safe_join(dest: &Path, entry: &str) -> Option<PathBuf> {
    let entry_path = Path::new(entry);
    // Reject absolute paths outright.
    if entry_path.is_absolute() {
        return None;
    }
    let mut joined = dest.to_path_buf();
    for component in entry_path.components() {
        match component {
            std::path::Component::Normal(part) => joined.push(part),
            // Skip `.`; reject `..` and prefixes (`C:\`, `~/`).
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_pack_unpack_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "hawellha_zip_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("hello.txt");
        let archive = dir.join("hello.zip");
        let out_dir = dir.join("out");
        std::fs::write(&src, "hello archive").unwrap();
        ArchiveConverter
            .convert(&src, &archive, &ConvertOptions::default())
            .expect("pack");
        assert!(archive.is_file());
        ArchiveConverter
            .convert(&archive, &out_dir, &ConvertOptions::default())
            .expect("unpack");
        assert_eq!(
            std::fs::read_to_string(out_dir.join("hello.txt")).unwrap(),
            "hello archive"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_slip_rejected() {
        let dest = Path::new("/tmp/hawellha_dest");
        assert!(safe_join(dest, "../evil.sh").is_none());
        assert!(safe_join(dest, "/absolute").is_none());
        assert!(safe_join(dest, "ok/nested.txt").is_some());
    }
}
