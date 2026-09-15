//! Document engine: TXT / Markdown ↔ PDF / HTML, PDF → TXT.
//!
//! - `txt|md → pdf`: typeset with `printpdf` (Helvetica, A4, word-wrapped,
//!   automatic pagination). Markdown syntax is rendered as plain text —
//!   full Markdown styling is out of scope, but headings get a size bump
//!   and `#` markers are stripped for readability.
//! - `txt|md → html`: minimal standalone HTML (escaped, headings mapped to
//!   `<h1>`–`<h6>`, paragraphs to `<p>`). No external dependencies.
//! - `pdf → txt`: text extraction via `pdf-extract`.
//! - `html → txt`: naive tag strip for round-tripping exported HTML.
//! - `txt ↔ md` / `md → txt`: plain copy with extension swap (formats are
//!   mutually readable); `txt → md` passes through unchanged.
//!
//! Scanned/image-only PDFs contain no embedded text; extraction then yields
//! an empty file and we surface a hint suggesting OCR instead of failing
//! silently.

use std::path::Path;

use super::{ConversionError, ConvertOptions, Converter, ensure_parent_dir, extension_of};

/// Document engine implementing [`Converter`].
pub struct DocumentConverter;

impl Converter for DocumentConverter {
    fn convert(
        &self,
        input: &Path,
        output: &Path,
        _opts: &ConvertOptions,
    ) -> Result<(), ConversionError> {
        ensure_parent_dir(output)?;
        convert_document(input, output)
    }
}

/// Returns `true` for document pairs handled by this engine.
pub fn supports(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("txt", "pdf")
            | ("md", "pdf")
            | ("markdown", "pdf")
            | ("txt", "html")
            | ("md", "html")
            | ("markdown", "html")
            | ("pdf", "txt")
            | ("pdf", "md")
            | ("html", "txt")
            | ("html", "md")
            | ("txt", "md")
            | ("md", "txt")
            | ("markdown", "txt")
            | ("txt", "markdown")
    )
}

/// UI-ordered targets for a document source.
pub fn targets_for(from: &str) -> Vec<String> {
    match from {
        "pdf" => vec!["txt".to_string(), "md".to_string()],
        "txt" => vec![
            "pdf".to_string(),
            "html".to_string(),
            "md".to_string(),
        ],
        "md" | "markdown" => vec![
            "pdf".to_string(),
            "html".to_string(),
            "txt".to_string(),
        ],
        "html" => vec!["txt".to_string(), "md".to_string()],
        _ => vec!["pdf".to_string(), "txt".to_string(), "html".to_string()],
    }
}

fn convert_document(input: &Path, output: &Path) -> Result<(), ConversionError> {
    let from = extension_of(input);
    let to = extension_of(output);
    match (from.as_str(), to.as_str()) {
        ("txt" | "md" | "markdown", "pdf") => text_to_pdf(input, output),
        ("txt" | "md" | "markdown", "html") => text_to_html(input, output),
        ("pdf", "txt" | "md" | "markdown") => pdf_to_text(input, output),
        ("html", "txt" | "md" | "markdown") => html_to_text(input, output),
        ("txt", "md") | ("txt", "markdown") | ("md", "txt") | ("markdown", "txt")
        | ("txt", "txt") | ("md", "md") => {
            // Plain-text family: byte copy is a faithful conversion.
            std::fs::copy(input, output).map_err(|e| ConversionError::UnwritableOutput {
                path: output.display().to_string(),
                reason: e.to_string(),
            })?;
            Ok(())
        }
        _ => Err(ConversionError::UnsupportedConversion { from, to }),
    }
}

// ---------------------------------------------------------------------------
// TXT/MD → PDF (printpdf 0.8)
// ---------------------------------------------------------------------------

/// A4 page geometry and margins (millimetres).
const PAGE_W_MM: f64 = 210.0;
const PAGE_H_MM: f64 = 297.0;
const MARGIN_MM: f64 = 20.0;
const LINE_HEIGHT_MM: f64 = 6.0;
const BASE_FONT_PT: f64 = 12.0;

fn text_to_pdf(input: &Path, output: &Path) -> Result<(), ConversionError> {
    use printpdf::{BuiltinFont, Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Pt, TextItem};

    let label = input.display().to_string();
    let out_label = output.display().to_string();

    let raw = std::fs::read(input).map_err(|e| ConversionError::UnreadableInput {
        path: label.clone(),
        reason: e.to_string(),
    })?;
    let text = String::from_utf8_lossy(&raw);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);

    let title = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Document")
        .to_string();

    // Very rough width model: Helvetica ≈ 0.5 × point size per char in mm
    // at 12pt → ~2.1mm/char. Usable width 170mm → ~80 chars per line.
    let usable_mm = PAGE_W_MM - 2.0 * MARGIN_MM;
    let chars_per_line = ((usable_mm / (BASE_FONT_PT * 0.3528 * 0.55)) as usize).max(20);

    // Logical lines after markdown-aware wrapping.
    let mut logical: Vec<(String, bool)> = Vec::new(); // (text, is_heading)
    for raw_line in text.lines() {
        let (level, stripped) = parse_markdown_heading(raw_line);
        if stripped.trim().is_empty() {
            logical.push((String::new(), false)); // blank = paragraph gap
            continue;
        }
        for wrapped in wrap_line(stripped, chars_per_line) {
            logical.push((wrapped, level > 0));
        }
    }
    if logical.is_empty() {
        logical.push((String::new(), false));
    }

    // Paginate: ~40 body lines per A4 page at 6mm line height.
    let lines_per_page = ((PAGE_H_MM - 2.0 * MARGIN_MM) / LINE_HEIGHT_MM) as usize;
    let mut doc = PdfDocument::new(&title);
    let mut pages: Vec<PdfPage> = Vec::new();
    let mut ops: Vec<Op> = Vec::new();
    let mut lines_on_page = 0usize;

    // printpdf builtin fonts only cover WinAnsi; replace anything else
    // with `?` instead of emitting unrepresentable glyphs.
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| {
                if (c as u32) < 32 && c != '\t' {
                    '?'
                } else if (c as u32) > 255 {
                    '?'
                } else {
                    c
                }
            })
            .collect::<String>()
    };

    let begin_page = |ops: &mut Vec<Op>| {
        ops.push(Op::StartTextSection);
        ops.push(Op::SetTextCursor {
            pos: printpdf::Point::new(Mm(MARGIN_MM as f32), Mm((PAGE_H_MM - MARGIN_MM) as f32)),
        });
        ops.push(Op::SetLineHeight { lh: Pt(14.0) });
    };

    begin_page(&mut ops);
    for (line, is_heading) in &logical {
        if lines_on_page >= lines_per_page {
            ops.push(Op::EndTextSection);
            pages.push(PdfPage::new(
                Mm(PAGE_W_MM as f32),
                Mm(PAGE_H_MM as f32),
                std::mem::take(&mut ops),
            ));
            begin_page(&mut ops);
            lines_on_page = 0;
        }
        let (font, size) = if *is_heading {
            (BuiltinFont::HelveticaBold, Pt(16.0))
        } else {
            (BuiltinFont::Helvetica, Pt(12.0))
        };
        ops.push(Op::SetFontSizeBuiltinFont { size, font });
        ops.push(Op::WriteTextBuiltinFont {
            items: vec![TextItem::Text(sanitize(line))],
            font,
        });
        ops.push(Op::AddLineBreak);
        // Headings and paragraph gaps consume an extra line for air.
        if *is_heading || line.is_empty() {
            ops.push(Op::AddLineBreak);
            lines_on_page += 1;
        }
        lines_on_page += 1;
    }
    ops.push(Op::EndTextSection);
    pages.push(PdfPage::new(
        Mm(PAGE_W_MM as f32),
        Mm(PAGE_H_MM as f32),
        ops,
    ));

    let bytes = doc
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut Vec::new());

    std::fs::write(output, bytes).map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    Ok(())
}

/// Split a `#`-style heading into `(level, text)`. Level 0 = body text.
/// ATX closing hashes (`## Title ##`) and leading whitespace are tolerated.
fn parse_markdown_heading(line: &str) -> (usize, &str) {
    let trimmed = line.trim_start();
    // Fenced code markers pass through as body text.
    let mut level = 0;
    for c in trimmed.chars() {
        if c == '#' {
            level += 1;
        } else {
            break;
        }
    }
    if level > 0 && level <= 6 && trimmed[level..].starts_with([' ', '\t']) {
        let mut text = trimmed[level..].trim();
        // Strip optional closing sequence `##`.
        if let Some(stripped) = text.strip_suffix('#') {
            text = stripped.trim_end().trim_end_matches('#').trim_end();
        }
        (level, text)
    } else {
        (0, line.trim_end())
    }
}

/// Greedy word wrap at `width` chars; overlong words are hard-split.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        // Hard-split words longer than the line itself.
        let mut rest = word;
        while rest.len() > width {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            out.push(rest[..width].to_string());
            rest = &rest[width..];
        }
        if current.is_empty() {
            current = rest.to_string();
        } else if current.len() + 1 + rest.len() <= width {
            current.push(' ');
            current.push_str(rest);
        } else {
            out.push(std::mem::take(&mut current));
            current = rest.to_string();
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

// ---------------------------------------------------------------------------
// TXT/MD → HTML (dependency-free, minimal styling)
// ---------------------------------------------------------------------------

/// Escape `&<>"` for embedding in HTML body text.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn text_to_html(input: &Path, output: &Path) -> Result<(), ConversionError> {
    let label = input.display().to_string();
    let out_label = output.display().to_string();

    let raw = std::fs::read(input).map_err(|e| ConversionError::UnreadableInput {
        path: label.clone(),
        reason: e.to_string(),
    })?;
    let text = String::from_utf8_lossy(&raw);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);

    let title = html_escape(
        input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Document"),
    );

    let mut body = String::new();
    let mut paragraph: Vec<String> = Vec::new();
    let flush_paragraph = |paragraph: &mut Vec<String>, body: &mut String| {
        if !paragraph.is_empty() {
            body.push_str("<p>");
            body.push_str(&paragraph.join(" "));
            body.push_str("</p>\n");
            paragraph.clear();
        }
    };

    for raw_line in text.lines() {
        let (level, stripped) = parse_markdown_heading(raw_line);
        if stripped.trim().is_empty() {
            flush_paragraph(&mut paragraph, &mut body);
            continue;
        }
        if level > 0 {
            flush_paragraph(&mut paragraph, &mut body);
            let tag = format!("h{}", level.min(6));
            body.push_str(&format!(
                "<{tag}>{}</{tag}>\n",
                html_escape(stripped.trim())
            ));
        } else {
            paragraph.push(html_escape(stripped.trim()));
        }
    }
    flush_paragraph(&mut paragraph, &mut body);
    if body.is_empty() {
        body.push_str("<p></p>\n");
    }

    let html = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <style>body{{max-width:45rem;margin:2rem auto;padding:0 1rem;\
         font-family:system-ui,sans-serif;line-height:1.6}}</style>\n\
         </head>\n<body>\n{body}</body>\n</html>\n"
    );

    std::fs::write(output, html).map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    Ok(())
}

/// Naive HTML → text: strip tags, unescape the 4 entities we emit.
fn html_to_text(input: &Path, output: &Path) -> Result<(), ConversionError> {
    let label = input.display().to_string();
    let out_label = output.display().to_string();

    let raw = std::fs::read(input).map_err(|e| ConversionError::UnreadableInput {
        path: label.clone(),
        reason: e.to_string(),
    })?;
    let html = String::from_utf8_lossy(&raw);

    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                if !text.ends_with('\n') {
                    text.push('\n');
                }
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&");
    // Collapse 3+ newlines to a paragraph break.
    let mut collapsed = String::with_capacity(text.len());
    let mut blanks = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 2 {
                collapsed.push('\n');
            }
        } else {
            blanks = 0;
            collapsed.push_str(line.trim_end());
            collapsed.push('\n');
        }
    }

    std::fs::write(output, collapsed.trim_start().to_string() + "\n").map_err(|e| {
        ConversionError::UnwritableOutput {
            path: out_label.clone(),
            reason: e.to_string(),
        }
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// PDF → TXT (pdf-extract)
// ---------------------------------------------------------------------------

fn pdf_to_text(input: &Path, output: &Path) -> Result<(), ConversionError> {
    let label = input.display().to_string();
    let out_label = output.display().to_string();

    let bytes = std::fs::read(input).map_err(|e| ConversionError::UnreadableInput {
        path: label.clone(),
        reason: e.to_string(),
    })?;
    let text =
        pdf_extract::extract_text_from_mem(&bytes).map_err(|e| ConversionError::MalformedData {
            path: label.clone(),
            format: "PDF".to_string(),
            reason: format!("cannot extract text: {e}"),
        })?;

    if text.trim().is_empty() {
        return Err(ConversionError::MalformedData {
            path: label.clone(),
            format: "PDF".to_string(),
            reason: "no embedded text found — this PDF may be scanned images; OCR is not supported yet".to_string(),
        });
    }

    std::fs::write(output, text).map_err(|e| ConversionError::UnwritableOutput {
        path: out_label.clone(),
        reason: e.to_string(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_and_heading() {
        assert_eq!(parse_markdown_heading("## Hello ##"), (2, "Hello"));
        assert_eq!(parse_markdown_heading("plain"), (0, "plain"));
        let lines = wrap_line("one two three four", 7);
        assert_eq!(lines, vec!["one two", "three", "four"]);
    }

    #[test]
    fn txt_to_pdf_smoke() {
        let dir = std::env::temp_dir().join(format!(
            "hawellha_doc_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("note.txt");
        let dst = dir.join("note.pdf");
        std::fs::write(&src, "# Title\n\nHello world, this is a test of the PDF writer.\n").unwrap();
        DocumentConverter
            .convert(&src, &dst, &ConvertOptions::default())
            .expect("txt->pdf");
        assert!(dst.is_file());
        assert!(std::fs::metadata(&dst).unwrap().len() > 500);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn md_to_html_smoke() {
        let dir = std::env::temp_dir().join(format!(
            "hawellha_html_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("note.md");
        let dst = dir.join("note.html");
        std::fs::write(&src, "# Title\n\nHello <world> & friends.\n").unwrap();
        assert!(supports("md", "html"));
        DocumentConverter
            .convert(&src, &dst, &ConvertOptions::default())
            .expect("md->html");
        let html = std::fs::read_to_string(&dst).unwrap();
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("&lt;world&gt;"));
        // Round-trip back to text.
        let back = dir.join("note.txt");
        DocumentConverter
            .convert(&dst, &back, &ConvertOptions::default())
            .expect("html->txt");
        let txt = std::fs::read_to_string(&back).unwrap();
        assert!(txt.contains("Title"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
