//! Reusable COSMIC widgets for Hawellha.
//!
//! Pure presentation helpers shared by [`crate::app`]: file-type icon names,
//! human-readable sizes, status labels, and small composite widgets (drop
//! zone, queue cards, status badges). Business logic (conversion dispatch,
//! validation) lives in [`crate::converters`]; this module only renders.

use cosmic::Element;
use cosmic::iced::Length;
use cosmic::widget::{self, icon};

use crate::converters::FileKind;

// ---------------------------------------------------------------------------
// Pure helpers (no widgets)
// ---------------------------------------------------------------------------

/// Symbolic icon name for a [`FileKind`], following freedesktop naming.
pub fn icon_name_for_kind(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Tabular => "x-office-spreadsheet-symbolic",
        FileKind::Image => "image-x-generic-symbolic",
        FileKind::Audio => "audio-x-generic-symbolic",
        FileKind::Video => "video-x-generic-symbolic",
        FileKind::Document => "x-office-document-symbolic",
        FileKind::Archive => "package-x-generic-symbolic",
        FileKind::Unknown => "text-x-generic-symbolic",
    }
}

/// Pretty-print a byte count (`1536` → `"1.5 KiB"`).
pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Short status label for a queue entry.
pub fn status_label(status: &crate::app::FileStatus) -> &'static str {
    match status {
        crate::app::FileStatus::Pending => "Ready",
        crate::app::FileStatus::Converting => "Converting…",
        crate::app::FileStatus::Done => "Done",
        crate::app::FileStatus::Failed => "Failed",
    }
}

// ---------------------------------------------------------------------------
// Composite widgets
// ---------------------------------------------------------------------------

/// File-type icon widget, sized for queue cards.
pub fn file_icon<'a, Message: 'static>(kind: FileKind) -> Element<'a, Message> {
    icon::from_name(icon_name_for_kind(kind)).size(32).into()
}

/// Uppercase source-format badge, e.g. `CSV`.
pub fn source_badge<'a, Message: 'static>(ext: &str) -> Element<'a, Message> {
    widget::container(widget::text::caption(ext.to_ascii_uppercase()))
        .padding([2, 8])
        .class(cosmic::theme::Container::List)
        .into()
}

/// Spacious drop-zone body: big icon + headline + hint.
///
/// The caller wraps this in a container/card and adds the "Add files"
/// buttons; keeping content separate makes the hovered vs idle states easy
/// to restyle without duplicating layout.
pub fn drop_zone_body<'a, Message: 'static>(is_hovered: bool) -> Element<'a, Message> {
    let headline = if is_hovered {
        "Drop files to add them"
    } else {
        "Drag & drop files here"
    };
    widget::column::with_capacity(3)
        .push(icon::from_name("document-open-symbolic").size(48))
        .push(widget::text::title2(headline))
        .push(widget::text::body(
            "…or use the buttons below. You can also drop files onto this window.",
        ))
        .spacing(8)
        .align_x(cosmic::iced::Alignment::Center)
        .width(Length::Fill)
        .into()
}
