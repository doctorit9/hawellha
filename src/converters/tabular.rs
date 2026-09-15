//! Data & tabular conversion engine (primary focus).
//!
//! Supported pairs:
//!
//! | from ↓ / to → | `csv` | `tsv` | `xlsx` | `json` |
//! |---|---|---|---|---|
//! | `csv`/`tsv` | (normalise) | ✓ | ✓ styled | ✓ |
//! | `xlsx`/`xls`/… | ✓ | ✓ | — | ✓ |
//! | `json` | ✓ (flattened) | ✓ | ✓ (flattened) | — |
//!
//! Design notes:
//! - CSV delimiter is auto-detected from `, ; \t |` unless the caller
//!   overrides it via [`ConvertOptions::csv_delimiter`][super::ConvertOptions].
//! - XLSX output is styled: bold header row, autofilter, frozen top row,
//!   and content-aware column widths.
//! - JSON tables are flattened with dot-notation (`address.city`) so nested
//!   objects survive the round-trip to a flat grid. Arrays of primitives
//!   become `"; "`-joined strings; anything else becomes compact JSON.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

use calamine::{Data, Reader, open_workbook_auto};
use rust_xlsxwriter::{Color, Format, FormatAlign, FormatBorder, Workbook};

use super::{ConversionError, ConvertOptions, Converter, ensure_parent_dir, extension_of};

// Re-exported so `super` and tests can use an ordered map without adding a
// dependency: `IndexMap` preserves first-seen header order. We implement a
// tiny insertion-ordered map on top of `Vec` + `BTreeMap` to avoid pulling in
// `indexmap` for one use-site. (Kept private; public API uses Vecs.)

/// Tabular engine implementing [`Converter`].
pub struct TabularConverter;

impl Converter for TabularConverter {
    fn convert(
        &self,
        input: &Path,
        output: &Path,
        opts: &ConvertOptions,
    ) -> Result<(), ConversionError> {
        ensure_parent_dir(output)?;
        let from = extension_of(input);
        let to = extension_of(output);
        convert_tabular(input, &from, output, &to, opts)
    }
}

/// Returns `true` for tabular pairs handled by this engine.
pub fn supports(from: &str, to: &str) -> bool {
    matches!(from, "csv" | "tsv" | "xlsx" | "xls" | "xlsm" | "xlsb" | "ods" | "json")
        && matches!(to, "csv" | "tsv" | "xlsx" | "json")
        && from != to
}

/// UI-ordered target list for a tabular source extension.
pub fn targets_for(from: &str) -> Vec<String> {
    match from {
        "csv" | "tsv" => vec!["xlsx".into(), "json".into(), "csv".into(), "tsv".into()],
        "xlsx" | "xls" | "xlsm" | "xlsb" | "ods" => {
            vec!["csv".into(), "tsv".into(), "json".into()]
        }
        "json" => vec!["csv".into(), "tsv".into(), "xlsx".into()],
        _ => vec!["csv".into(), "xlsx".into(), "json".into()],
    }
    .into_iter()
    .filter(|t| t != from)
    .collect()
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

fn convert_tabular(
    input: &Path,
    from: &str,
    output: &Path,
    to: &str,
    opts: &ConvertOptions,
) -> Result<(), ConversionError> {
    let input_label = input.display().to_string();
    match (from, to) {
        ("csv" | "tsv", "xlsx") => {
            let (headers, rows) = read_csv(input, opts)?;
            write_xlsx(&headers, &rows, output, sheet_name_for(input))
        }
        ("csv", "json") | ("tsv", "json") | ("csv", "csv") | ("csv", "tsv") | ("tsv", "csv")
        | ("tsv", "tsv") => {
            let (headers, rows) = read_csv(input, opts)?;
            if to == "json" {
                write_json_rows(&headers, &rows, output)
            } else {
                write_csv_rows(&headers, &rows, output, delimiter_for_ext(to))
            }
        }
        ("xlsx" | "xls" | "xlsm" | "xlsb" | "ods", "csv" | "tsv") => {
            let (headers, rows) = read_workbook(input)?;
            write_csv_rows(&headers, &rows, output, delimiter_for_ext(to))
        }
        ("xlsx" | "xls" | "xlsm" | "xlsb" | "ods", "json") => {
            let (headers, rows) = read_workbook(input)?;
            write_json_rows(&headers, &rows, output)
        }
        ("json", "csv" | "tsv") => {
            let (headers, rows) = read_json_table(input)?;
            write_csv_rows(&headers, &rows, output, delimiter_for_ext(to))
        }
        ("json", "xlsx") => {
            let (headers, rows) = read_json_table(input)?;
            write_xlsx(&headers, &rows, output, sheet_name_for(input))
        }
        _ => Err(ConversionError::UnsupportedConversion {
            from: from.to_string(),
            to: to.to_string(),
        }),
    }
    .map_err(|e| match e {
        // Preserve already-contextualised errors untouched.
        ConversionError::Io(_)
        | ConversionError::UnsupportedConversion { .. }
        | ConversionError::MalformedData { .. }
        | ConversionError::UnreadableInput { .. }
        | ConversionError::UnwritableOutput { .. }
        | ConversionError::ExternalTool(_) => e,
    })?;
    let _ = input_label;
    Ok(())
}

// ---------------------------------------------------------------------------
// CSV reading (delimiter detection)
// ---------------------------------------------------------------------------

/// Candidate delimiters scored during detection, in priority order for ties.
const CANDIDATE_DELIMITERS: [u8; 4] = [b',', b';', b'\t', b'|'];

/// Detect the delimiter by scoring the first few non-empty lines.
///
/// The winner is the delimiter producing the most *consistent* column count
/// > 1 across sampled lines; ties prefer `,` → `;` → tab → `|`. A file with
/// no clear delimiter falls back to `,`.
pub fn detect_delimiter(sample: &str) -> u8 {
    let lines: Vec<&str> = sample
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(8)
        .collect();
    if lines.is_empty() {
        return b',';
    }

    let mut best = b',';
    let mut best_score: i64 = -1;

    for &delim in &CANDIDATE_DELIMITERS {
        let ch = delim as char;
        // Count occurrences per line (naïve split is fine for detection;
        // the real parse uses the `csv` crate with quoting support).
        let counts: Vec<usize> = lines.iter().map(|l| l.split(ch).count()).collect();
        let max = counts.iter().copied().max().unwrap_or(1);
        if max <= 1 {
            continue;
        }
        // Consistency: how many lines agree with the modal column count?
        let consistent = counts.iter().filter(|&&c| c == max).count() as i64;
        // Weight by column count so `a,b,c` beats `a;b` noise.
        let score = consistent * 10 + max as i64;
        if score > best_score {
            best_score = score;
            best = delim;
        }
    }
    best
}

fn delimiter_for_ext(ext: &str) -> u8 {
    match ext {
        "tsv" => b'\t',
        _ => b',',
    }
}

/// Read a CSV/TSV file into `(headers, rows)`.
///
/// * Empty files yield zero headers/rows (callers write an empty table).
/// * Ragged rows are padded/truncated to the header width — never an error.
/// * `opts.csv_delimiter` overrides auto-detection.
fn read_csv(
    input: &Path,
    opts: &ConvertOptions,
) -> Result<(Vec<String>, Vec<Vec<String>>), ConversionError> {
    let label = input.display().to_string();
    let raw = std::fs::read(input).map_err(|e| ConversionError::UnreadableInput {
        path: label.clone(),
        reason: e.to_string(),
    })?;
    // Lenient UTF-8: strip BOM, replace invalid sequences instead of failing.
    let text = String::from_utf8_lossy(&raw);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);

    let delimiter = opts.csv_delimiter.unwrap_or_else(|| {
        let sample: String = text.lines().take(8).collect::<Vec<_>>().join("\n");
        // A `.tsv` file that contains no tabs still parses as single-column
        // CSV; prefer tab when the extension says so and tabs are present.
        let ext = extension_of(input);
        let detected = detect_delimiter(&sample);
        if ext == "tsv" && sample.contains('\t') {
            b'\t'
        } else {
            detected
        }
    });

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .trim(csv::Trim::None)
        .delimiter(delimiter)
        .from_reader(text.as_bytes());

    let mut records: Vec<Vec<String>> = Vec::new();
    for result in reader.records() {
        let record = result.map_err(|e| ConversionError::MalformedData {
            path: label.clone(),
            format: "CSV".to_string(),
            reason: e.to_string(),
        })?;
        // Skip fully-blank lines (common trailing newline).
        if record.iter().all(|f| f.trim().is_empty()) {
            continue;
        }
        records.push(record.iter().map(|f| f.to_string()).collect());
    }

    if records.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let width = records.iter().map(Vec::len).max().unwrap_or(0);
    let mut headers = records.remove(0);
    normalise_width(&mut headers, width);
    // De-duplicate blank headers (`""` → `column_2`, …) so JSON keys are unique.
    dedupe_headers(&mut headers);
    for row in &mut records {
        normalise_width(row, width);
    }
    Ok((headers, records))
}

/// Pad with `""` or truncate a row to `width`.
fn normalise_width(row: &mut Vec<String>, width: usize) {
    if row.len() < width {
        row.resize(width, String::new());
    } else {
        row.truncate(width);
    }
}

/// Rename empty/duplicate headers to `column_N` / `name (2)` style.
fn dedupe_headers(headers: &mut [String]) {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (i, h) in headers.iter_mut().enumerate() {
        if h.trim().is_empty() {
            *h = format!("column_{}", i + 1);
        }
        let base = h.clone();
        let count = seen.entry(base.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            *h = format!("{base} ({count})");
        }
    }
}

// ---------------------------------------------------------------------------
// Spreadsheet reading (calamine)
// ---------------------------------------------------------------------------

/// Read the first non-empty sheet of a workbook into `(headers, rows)`.
///
/// Empty leading rows/columns are trimmed; ragged rows are normalised to the
/// widest row; the first row becomes the header (deduplicated).
fn read_workbook(input: &Path) -> Result<(Vec<String>, Vec<Vec<String>>), ConversionError> {
    let label = input.display().to_string();
    if !input.is_file() {
        return Err(ConversionError::UnreadableInput {
            path: label.clone(),
            reason: "file does not exist or is not a regular file".to_string(),
        });
    }
    let mut workbook = open_workbook_auto(input).map_err(|e| {
        ConversionError::MalformedData {
            path: label.clone(),
            format: "spreadsheet".to_string(),
            reason: format!("cannot parse workbook: {e}"),
        }
    })?;

    let sheet_names = workbook.sheet_names();
    if sheet_names.is_empty() {
        return Err(ConversionError::MalformedData {
            path: label.clone(),
            format: "spreadsheet".to_string(),
            reason: "workbook contains no sheets".to_string(),
        });
    }

    // Pick the first sheet that actually contains data.
    let mut chosen: Option<(String, Vec<Vec<String>>)> = None;
    for name in &sheet_names {
        let range = workbook.worksheet_range(name).map_err(|e| {
            ConversionError::MalformedData {
                path: label.clone(),
                format: "spreadsheet".to_string(),
                reason: format!("cannot read sheet '{name}': {e}"),
            }
        })?;
        if range.is_empty() {
            continue;
        }
        let rows: Vec<Vec<String>> = range
            .rows()
            .map(|row| row.iter().map(cell_to_string).collect())
            .collect();
        // Skip sheets that are entirely blank.
        if rows
            .iter()
            .all(|r| r.iter().all(|c| c.trim().is_empty()))
        {
            continue;
        }
        chosen = Some((name.clone(), rows));
        break;
    }

    let (_, mut rows) = chosen.unwrap_or_else(|| (sheet_names[0].clone(), Vec::new()));
    if rows.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    // Trim fully-blank trailing rows (LibreOffice often writes them).
    while rows
        .last()
        .is_some_and(|r| r.iter().all(|c| c.trim().is_empty()))
    {
        rows.pop();
    }
    if rows.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    for row in &mut rows {
        normalise_width(row, width);
    }
    let mut headers = rows.remove(0);
    normalise_width(&mut headers, width);
    dedupe_headers(&mut headers);
    Ok((headers, rows))
}

/// Render a calamine cell as plain text.
///
/// Numbers that are integral print without a trailing `.0`; floats use the
/// shortest round-trip representation; booleans become `TRUE`/`FALSE` to
/// match spreadsheet conventions.
fn cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                // `ryu`-style shortest repr via `{}` on f64 is fine here.
                format!("{f}")
            }
        }
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => {
            if *b {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#ERR:{e:?}"),
    }
}

// ---------------------------------------------------------------------------
// JSON reading (flattened)
// ---------------------------------------------------------------------------

/// Read a JSON document into `(headers, rows)`.
///
/// Accepted shapes:
/// - array of objects → one row per object (flattened with dot-notation);
/// - single object → one row;
/// - array of primitives → single `value` column;
/// - primitive → single cell.
///
/// The header is the union of all flattened keys in first-seen order.
fn read_json_table(input: &Path) -> Result<(Vec<String>, Vec<Vec<String>>), ConversionError> {
    let label = input.display().to_string();
    let raw = std::fs::read(input).map_err(|e| ConversionError::UnreadableInput {
        path: label.clone(),
        reason: e.to_string(),
    })?;
    let text = String::from_utf8_lossy(&raw);
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| ConversionError::MalformedData {
            path: label.clone(),
            format: "JSON".to_string(),
            reason: e.to_string(),
        })?;

    let objects: Vec<BTreeMap<String, String>> = match &value {
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                return Ok((Vec::new(), Vec::new()));
            }
            items
                .iter()
                .map(|item| match item {
                    serde_json::Value::Object(_) => flatten_to_strings(item, "", BTreeMap::new()),
                    other => {
                        let mut m = BTreeMap::new();
                        m.insert("value".to_string(), json_scalar_to_string(other));
                        m
                    }
                })
                .collect()
        }
        serde_json::Value::Object(_) => {
            vec![flatten_to_strings(&value, "", BTreeMap::new())]
        }
        other => {
            let mut m = BTreeMap::new();
            m.insert("value".to_string(), json_scalar_to_string(other));
            vec![m]
        }
    };

    // Union headers in first-seen order.
    let mut headers: Vec<String> = Vec::new();
    for obj in &objects {
        for key in obj.keys() {
            if !headers.contains(key) {
                headers.push(key.clone());
            }
        }
    }
    let rows: Vec<Vec<String>> = objects
        .iter()
        .map(|obj| {
            headers
                .iter()
                .map(|h| obj.get(h).cloned().unwrap_or_default())
                .collect()
        })
        .collect();
    Ok((headers, rows))
}

/// Flatten a JSON value into dotted `key → string` pairs.
///
/// Nested objects recurse (`{"a":{"b":1}}` → `a.b = "1"`); arrays of
/// primitives join with `"; "`; arrays of objects/arrays become compact JSON
/// so no data is silently dropped.
fn flatten_to_strings(
    value: &serde_json::Value,
    prefix: &str,
    mut out: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    match value {
        serde_json::Value::Object(map) => {
            if map.is_empty() && !prefix.is_empty() {
                out.insert(prefix.to_string(), String::new());
            }
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                out = flatten_to_strings(v, &key, out);
            }
        }
        serde_json::Value::Array(items) => {
            if items.iter().all(|i| i.is_null()) {
                out.insert(prefix.to_string(), String::new());
            } else if items.iter().all(|i| {
                i.is_string() || i.is_number() || i.is_boolean() || i.is_null()
            }) {
                let joined = items
                    .iter()
                    .filter(|i| !i.is_null())
                    .map(json_scalar_to_string)
                    .collect::<Vec<_>>()
                    .join("; ");
                out.insert(prefix.to_string(), joined);
            } else {
                // Mixed/complex arrays: preserve as compact JSON.
                out.insert(
                    prefix.to_string(),
                    serde_json::to_string(value).unwrap_or_default(),
                );
            }
        }
        other => {
            out.insert(prefix.to_string(), json_scalar_to_string(other));
        }
    }
    out
}

fn json_scalar_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_default()
        }
    }
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// Write `(headers, rows)` as CSV/TSV.
fn write_csv_rows(
    headers: &[String],
    rows: &[Vec<String>],
    output: &Path,
    delimiter: u8,
) -> Result<(), ConversionError> {
    let label = output.display().to_string();
    let file = File::create(output).map_err(|e| ConversionError::UnwritableOutput {
        path: label.clone(),
        reason: e.to_string(),
    })?;
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .from_writer(file);
    if !headers.is_empty() {
        writer
            .write_record(headers)
            .map_err(|e| ConversionError::UnwritableOutput {
                path: label.clone(),
                reason: e.to_string(),
            })?;
    }
    for row in rows {
        writer
            .write_record(row)
            .map_err(|e| ConversionError::UnwritableOutput {
                path: label.clone(),
                reason: e.to_string(),
            })?;
    }
    writer
        .flush()
        .map_err(|e| ConversionError::UnwritableOutput {
            path: label.clone(),
            reason: e.to_string(),
        })?;
    Ok(())
}

/// Write `(headers, rows)` as a JSON array of objects.
///
/// Cells that look numeric/boolean stay strings — CSV has no types, so we
/// preserve fidelity rather than guessing. (A typed inference pass would
/// corrupt IDs like `"00123"`.)
fn write_json_rows(
    headers: &[String],
    rows: &[Vec<String>],
    output: &Path,
) -> Result<(), ConversionError> {
    let label = output.display().to_string();
    let array: Vec<serde_json::Map<String, serde_json::Value>> = rows
        .iter()
        .map(|row| {
            headers
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    let key = if h.is_empty() {
                        format!("column_{}", i + 1)
                    } else {
                        h.clone()
                    };
                    (key, serde_json::Value::String(row.get(i).cloned().unwrap_or_default()))
                })
                .collect()
        })
        .collect();
    let file = File::create(output).map_err(|e| ConversionError::UnwritableOutput {
        path: label.clone(),
        reason: e.to_string(),
    })?;
    serde_json::to_writer_pretty(file, &array).map_err(|e| {
        ConversionError::UnwritableOutput {
            path: label.clone(),
            reason: e.to_string(),
        }
    })?;
    Ok(())
}

/// Write `(headers, rows)` as a styled XLSX workbook.
///
/// Styling: bold white-on-navy header, thin borders, autofilter, frozen top
/// row, and content-aware column widths (10–50 chars). Numbers that parse as
/// `f64` are written as numbers so Excel can sum them; everything else is a
/// shared string.
fn write_xlsx(
    headers: &[String],
    rows: &[Vec<String>],
    output: &Path,
    sheet_name: String,
) -> Result<(), ConversionError> {
    let label = output.display().to_string();
    let map_err = |e: rust_xlsxwriter::XlsxError| ConversionError::UnwritableOutput {
        path: label.clone(),
        reason: e.to_string(),
    };

    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    // Sheet names must be ≤ 31 chars and avoid `[]:*?/\`.
    let _ = worksheet.set_name(sanitize_sheet_name(&sheet_name));

    let header_format = Format::new()
        .set_bold()
        .set_font_color(Color::White)
        .set_background_color(Color::RGB(0x2B_579A))
        .set_align(FormatAlign::Center)
        .set_border(FormatBorder::Thin);
    let cell_format = Format::new().set_border(FormatBorder::Thin);
    let number_format = Format::new()
        .set_border(FormatBorder::Thin)
        .set_num_format("0.00");

    // Track max display width per column for auto-sizing.
    let width = headers.len().max(rows.first().map(Vec::len).unwrap_or(0));
    let mut col_widths = vec![10usize; width];

    for (col, header) in headers.iter().enumerate() {
        worksheet
            .write_string_with_format(0, col as u16, header.as_str(), &header_format)
            .map_err(map_err)?;
        col_widths[col] = col_widths[col].max(display_width(header).min(50).max(10));
    }

    for (r, row) in rows.iter().enumerate() {
        let excel_row = (r + 1) as u32;
        for (c, cell) in row.iter().enumerate().take(width) {
            col_widths[c] = col_widths[c].max(display_width(cell).min(50).max(10));
            // Numeric fast-path: integers and decimals become real numbers.
            if let Ok(number) = cell.replace(',', ".").parse::<f64>() {
                // Guard against IDs with leading zeros ("00123") and empty
                // strings: keep those as text to preserve fidelity.
                let keep_text = cell.starts_with('0') && cell.len() > 1 && !cell.contains(['.', 'e', 'E'])
                    || cell.trim().is_empty();
                if !keep_text && !cell.trim().is_empty() {
                    // Integers use the plain bordered format; decimals get 2dp.
                    if cell.contains(['.', 'e', 'E']) {
                        worksheet
                            .write_number_with_format(excel_row, c as u16, number, &number_format)
                            .map_err(map_err)?;
                    } else {
                        worksheet
                            .write_number_with_format(excel_row, c as u16, number, &cell_format)
                            .map_err(map_err)?;
                    }
                    continue;
                }
            }
            worksheet
                .write_string_with_format(excel_row, c as u16, cell.as_str(), &cell_format)
                .map_err(map_err)?;
        }
    }

    if width > 0 {
        let last_row = rows.len() as u32; // includes header row 0
        let last_col = (width - 1) as u16;
        // Best-effort niceties: ignore errors on empty tables.
        let _ = worksheet.autofilter(0, 0, last_row, last_col);
        worksheet.set_freeze_panes(1, 0).map_err(map_err)?;
        for (c, w) in col_widths.iter().enumerate() {
            let _ = worksheet.set_column_width(c as u16, *w as f64);
        }
    }

    workbook.save(output).map_err(map_err)?;
    return Ok(());

    // Local helper keeps the error closure simple.
    #[allow(dead_code)]
    fn unreachable() {}
}

/// Approximate display width: CJK/emoji count double, else char count.
fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if c as u32 >= 0x1100
                && (c as u32 <= 0x115F
                    || (0x2E80..=0xA4CF).contains(&(c as u32))
                    || (0xAC00..=0xD7A3).contains(&(c as u32))
                    || (0xF900..=0xFAFF).contains(&(c as u32))
                    || (0xFE30..=0xFE4F).contains(&(c as u32))
                    || (0xFF00..=0xFF60).contains(&(c as u32))
                    || (0xFFE0..=0xFFE6).contains(&(c as u32)))
            {
                2
            } else {
                1
            }
        })
        .sum::<usize>()
        + 2 // padding so text doesn't touch cell borders
}

/// Sheet name from the input file stem, sanitised for Excel rules.
fn sheet_name_for(input: &Path) -> String {
    input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Sheet1")
        .to_string()
}

fn sanitize_sheet_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '[' | ']' | ':' | '*' | '?' | '/' | '\\' => '_',
            c => c,
        })
        .collect();
    let mut short: String = cleaned.chars().take(31).collect();
    if short.trim().is_empty() {
        short = "Sheet1".to_string();
    }
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(path: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hawellha_test_{path}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        p
    }

    #[test]
    fn delimiter_detection() {
        assert_eq!(detect_delimiter("a,b,c\n1,2,3\n"), b',');
        assert_eq!(detect_delimiter("a;b;c\n1;2;3\n"), b';');
        assert_eq!(detect_delimiter("a\tb\tc\n1\t2\t3\n"), b'\t');
        assert_eq!(detect_delimiter("a|b|c\n1|2|3\n"), b'|');
    }

    #[test]
    fn csv_xlsx_roundtrip() {
        let dir = tmp("tabular");
        std::fs::create_dir_all(&dir).unwrap();
        let csv_path = dir.join("in.csv");
        let xlsx_path = dir.join("out.xlsx");
        let back_path = dir.join("back.csv");
        std::fs::write(&csv_path, "name,age,city\nAda,36,London\nLinus,55,Helsinki\n").unwrap();

        let opts = ConvertOptions::default();
        TabularConverter
            .convert(&csv_path, &xlsx_path, &opts)
            .expect("csv->xlsx");
        assert!(xlsx_path.is_file());
        TabularConverter
            .convert(&xlsx_path, &back_path, &opts)
            .expect("xlsx->csv");
        let back = std::fs::read_to_string(&back_path).unwrap();
        assert!(back.contains("Ada") && back.contains("Linus"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_csv_flatten() {
        let dir = tmp("json");
        std::fs::create_dir_all(&dir).unwrap();
        let json_path = dir.join("in.json");
        let csv_path = dir.join("out.csv");
        std::fs::write(
            &json_path,
            r#"[{"name":"Ada","address":{"city":"London"}},{"name":"Linus","address":{"city":"Helsinki"}}]"#,
        )
        .unwrap();
        TabularConverter
            .convert(&json_path, &csv_path, &ConvertOptions::default())
            .expect("json->csv");
        let out = std::fs::read_to_string(&csv_path).unwrap();
        assert!(out.contains("address.city"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn semicolon_csv_to_json() {
        let dir = tmp("semi");
        std::fs::create_dir_all(&dir).unwrap();
        let csv_path = dir.join("in.csv");
        let json_path = dir.join("out.json");
        let mut f = File::create(&csv_path).unwrap();
        writeln!(f, "a;b;c").unwrap();
        writeln!(f, "1;2;3").unwrap();
        TabularConverter
            .convert(&csv_path, &json_path, &ConvertOptions::default())
            .expect("csv->json");
        let out = std::fs::read_to_string(&json_path).unwrap();
        assert!(out.contains("\"a\""), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
