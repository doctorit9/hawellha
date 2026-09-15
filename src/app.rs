// SPDX-License-Identifier: MPL-2.0
//! Hawellha (تحويلها) — universal file converter for COSMIC Desktop.
//!
//! Architecture:
//! - [`AppModel`] holds all GUI state (queue, targets, output dir, toasts).
//! - [`Message`] drives [`cosmic::Application::update`]; heavy conversions
//!   run in `tokio::task::spawn_blocking` workers via `cosmic::task::future`
//!   so the UI stays at 60 FPS.
//! - [`crate::converters`] owns validation + engines; this file only maps
//!   state to COSMIC widgets.

use crate::config::Config;
use crate::converters::{
    self, ConvertOptions, FileKind, batch_targets_for, default_output_dir, extension_of,
    is_supported, kind_of_path, mime_of, resolve_output_path, targets_for,
};
use crate::fl;
use cosmic::app::{context_drawer, Core};
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::{event, keyboard, window, Alignment, Length, Subscription};
use cosmic::prelude::*;
use cosmic::widget::menu::key_bind::{KeyBind, Modifier};
use cosmic::widget::{self, about::About, icon, menu, nav_bar};
use cosmic::Application;
use std::collections::HashMap;
use std::path::PathBuf;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const APP_ICON: &[u8] = include_bytes!("../resources/icons/hicolor/scalable/apps/icon.svg");

// ---------------------------------------------------------------------------
// Queue model
// ---------------------------------------------------------------------------

/// Per-file conversion status shown in queue cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileStatus {
    #[default]
    Pending,
    Converting,
    Done,
    Failed,
}

/// One file in the processing queue.
#[derive(Debug, Clone)]
pub struct QueuedFile {
    /// Stable row id (never reused within a session).
    pub id: u64,
    pub path: PathBuf,
    pub file_name: String,
    pub size: u64,
    pub source_ext: String,
    pub mime: String,
    pub kind: FileKind,
    /// Valid target extensions for this source, in UI order.
    pub targets: Vec<String>,
    /// Index into `targets`, if the user picked one.
    pub target_idx: Option<usize>,
    pub status: FileStatus,
    /// 0.0 pending · 0.5 converting · 1.0 done (engines are synchronous,
    /// so per-file progress is coarse by design; the footer shows batch
    /// progress and toasts confirm completion).
    pub progress: f32,
    pub error: Option<String>,
    pub output_path: Option<PathBuf>,
    /// Whether this file is selected for batch operations.
    pub selected: bool,
    /// Timestamp when conversion started (for duration calculation).
    pub start_time: Option<std::time::Instant>,
    /// Timestamp when conversion finished (for duration calculation).
    pub end_time: Option<std::time::Instant>,
    /// One-line preview: image dimensions or first text line. Empty when
    /// unavailable (binary, unreadable, …). Computed once at queue time so
    /// cards render without I/O.
    pub preview: String,
}

/// Cheap one-line preview for queue cards. Never panics, never blocks long:
/// images probe dimensions only, text reads a small prefix.
fn file_preview(path: &std::path::Path, kind: FileKind, ext: &str) -> String {
    match kind {
        FileKind::Image => match image::image_dimensions(path) {
            Ok((w, h)) => format!("{w} × {h} px"),
            Err(_) => String::new(),
        },
        FileKind::Document | FileKind::Tabular if matches!(ext, "txt" | "md" | "markdown" | "csv" | "tsv" | "json" | "html") => {
            const LIMIT: u64 = 4096;
            let bytes = std::fs::read(path).unwrap_or_default();
            if bytes.is_empty() {
                return String::new();
            }
            let head = &bytes[..bytes.len().min(LIMIT as usize)];
            let text = String::from_utf8_lossy(head);
            for line in text.lines() {
                let trimmed = line.trim().trim_start_matches('\u{FEFF}');
                if !trimmed.is_empty() {
                    let mut s: String = trimmed.chars().take(90).collect();
                    if trimmed.chars().count() > 90 {
                        s.push('…');
                    }
                    return s;
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

impl QueuedFile {
    /// Build a queue entry from a path, probing size asynchronously is
    /// unnecessary — metadata is a fast syscall; failures yield size 0.
    fn from_path(id: u64, path: PathBuf) -> Self {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let source_ext = extension_of(&path);
        let kind = kind_of_path(&path);
        let targets = targets_for(&source_ext);
        let target_idx = default_target_index(&source_ext, &targets);
        let preview = file_preview(&path, kind, &source_ext);
        Self {
            id,
            path,
            file_name,
            size,
            source_ext,
            mime: String::new(), // filled below to keep constructor total
            kind,
            targets,
            target_idx,
            status: FileStatus::Pending,
            progress: 0.0,
            error: None,
            output_path: None,
            selected: false,
            start_time: None,
            end_time: None,
            preview,
        }
        .with_mime()
    }

    fn with_mime(mut self) -> Self {
        self.mime = mime_of(&self.path);
        self
    }

    /// Currently selected target extension, if any.
    fn selected_target(&self) -> Option<&str> {
        self.target_idx
            .and_then(|i| self.targets.get(i))
            .map(String::as_str)
    }

    /// Index of the selected target for the dropdown widget.
    fn selected_index(&self) -> Option<usize> {
        self.target_idx
            .filter(|i| *i < self.targets.len())
    }

    /// Validation message when the current pair cannot convert, else `None`.
    fn validation_error(&self) -> Option<String> {
        let target = self.selected_target()?;
        if is_supported(&self.source_ext, target) {
            None
        } else {
            Some(format!(
                "Cannot convert .{} to .{target}",
                self.source_ext
            ))
        }
    }
}

/// Sensible default target per source extension (first match wins).
fn default_target_index(source_ext: &str, targets: &[String]) -> Option<usize> {
    let preferred: &[&str] = match source_ext {
        "csv" | "tsv" => &["xlsx"],
        "xlsx" | "xls" | "xlsm" | "xlsb" | "ods" => &["csv"],
        "json" => &["csv"],
        "png" | "bmp" | "tiff" | "tif" | "gif" => &["jpg"],
        "jpg" | "jpeg" => &["png"],
        "webp" => &["png"],
        "mp3" | "wav" | "ogg" | "flac" | "aac" => &["mp3", "wav"],
        "mp4" | "mkv" | "webm" | "avi" | "mov" => &["mp3"],
        "txt" | "md" | "markdown" => &["pdf", "html"],
        "html" => &["txt"],
        "pdf" => &["txt"],
        "zip" | "tar" | "gz" => &["extract"],
        _ => &["zip"],
    };
    preferred
        .iter()
        .find_map(|p| targets.iter().position(|t| t == p))
        .or(if targets.is_empty() { None } else { Some(0) })
}

/// One finished conversion, kept for the History tab.
///
/// Persisted as JSON in the user cache dir (not via cosmic-config, which is
/// for small scalar prefs). Best-effort: I/O failures are ignored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub input_name: String,
    pub source_ext: String,
    pub target: String,
    pub output_display: String,
    pub success: bool,
    /// Unix seconds, for display only.
    pub finished_at: u64,
    pub favorite: bool,
}

impl HistoryEntry {
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

fn history_file_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("hawellha").join("history.json"))
}

fn load_history() -> Vec<HistoryEntry> {
    let Some(path) = history_file_path() else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_history(history: &[HistoryEntry]) {
    let Some(path) = history_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Keep the file small: last 100 entries only.
    let trimmed: Vec<&HistoryEntry> = history.iter().rev().take(100).collect();
    let owned: Vec<HistoryEntry> = trimmed.into_iter().rev().cloned().collect();
    if let Ok(json) = serde_json::to_string_pretty(&owned) {
        let _ = std::fs::write(&path, json);
    }
}

/// Subfolder name for grouped outputs.
fn kind_dir_name(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Tabular => "data",
        FileKind::Image => "images",
        FileKind::Audio => "audio",
        FileKind::Video => "video",
        FileKind::Document => "documents",
        FileKind::Archive => "archives",
        FileKind::Unknown => "files",
    }
}

/// Make `base` unique by appending `_1`, `_2`, … before the extension.
/// Handles compound `.tar.gz` names. Returns `base` unchanged when free.
fn unique_output_path(base: PathBuf) -> PathBuf {
    if !base.exists() {
        return base;
    }
    let parent = base.parent().map(|p| p.to_path_buf());
    let file_name = base
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");
    let (stem, ext) = if file_name.to_ascii_lowercase().ends_with(".tar.gz") {
        (
            file_name[..file_name.len() - 7].to_string(),
            ".tar.gz".to_string(),
        )
    } else if let Some(dot) = file_name.rfind('.') {
        (
            file_name[..dot].to_string(),
            file_name[dot..].to_string(),
        )
    } else {
        (file_name.to_string(), String::new())
    };
    for n in 1..1000 {
        let candidate = format!("{stem}_{n}{ext}");
        let full = match &parent {
            Some(p) => p.join(&candidate),
            None => PathBuf::from(&candidate),
        };
        if !full.exists() {
            return full;
        }
    }
    base
}

// ---------------------------------------------------------------------------
// Application model
// ---------------------------------------------------------------------------

/// The application model stores app-specific state used to describe its
/// interface and drive its logic.
pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: Core,
    /// Display a context drawer with the designated page if defined.
    context_page: ContextPage,
    /// The about page for this app.
    about: About,
    /// Contains items assigned to the nav bar panel.
    nav: nav_bar::Model,
    /// Key bindings for the application's menu bar.
    key_binds: HashMap<menu::KeyBind, MenuAction>,
    /// Configuration data that persists between application runs.
    config: Config,

    // --- Hawellha state ---
    /// All queued files (the active tab only *filters* this list).
    files: Vec<QueuedFile>,
    /// Monotonic id source for queue rows.
    next_id: u64,
    /// Destination folder for converted files.
    output_dir: PathBuf,
    /// Batch target applied via the footer dropdown (`None` = no override).
    global_target_idx: Option<usize>,
    /// Whether a batch conversion is currently running.
    is_converting: bool,
    /// Toast notifications (completion / failure summaries).
    toasts: widget::toaster::Toasts<Message>,
    /// JPEG quality text field (parsed on convert; invalid → 90).
    quality_text: String,
    /// Resize width/height text fields (empty = keep original).
    resize_w_text: String,
    resize_h_text: String,
    /// Non-modal warning banner (dialog errors, validation summaries).
    warning: Option<String>,
    /// Drag-hover highlight for the drop zone.
    drag_hovered: bool,
    /// Cached `ffmpeg -version` first line, or the install hint on failure.
    pub ffmpeg_info: String,
    /// Finished conversions (newest last, capped at 100, persisted to cache).
    pub history: Vec<HistoryEntry>,
    /// Audio bitrate dropdown selection (`None` = use config value).
    pub audio_bitrate_idx: Option<usize>,
}

/// Messages emitted by the application and its widgets.
#[derive(Debug, Clone)]
pub enum Message {
    LaunchUrl(String),
    ToggleContextPage(ContextPage),
    UpdateConfig(Config),
    // Queue management
    OpenFiles,
    OpenFolderAsInput,
    FilesChosen(Vec<PathBuf>),
    ChooseOutputDir,
    OutputDirChosen(PathBuf),
    RemoveFile(u64),
    ClearQueue,
    ClearFinished,
    SetFileTarget(u64, usize),
    SetGlobalTarget(usize),
    ApplyGlobalTarget,
    ToggleFileSelection(u64),
    SelectAllFiles,
    DeselectAllFiles,
    RemoveSelectedFiles,
    RetryFile(u64),
    RetryFailed,
    MoveFileUp(u64),
    MoveFileDown(u64),
    // Conversion
    ConvertAll,
    ConvertSelected,
    StopConversion,
    ConversionFinished(u64, Result<PathBuf, String>),
    // History
    ClearHistory,
    ToggleHistoryFavorite(usize),
    OpenPath(PathBuf),
    // Settings fields
    QualityInput(String),
    ResizeWInput(String),
    ResizeHInput(String),
    ToggleOverwrite(bool),
    ToggleGroupByKind(bool),
    ToggleAutoOpen(bool),
    SetAudioBitrate(usize),
    // Drag & drop (native OS file drops onto the window)
    FilesHovered,
    FilesHoverLeft,
    FilesDropped(Vec<PathBuf>),
    // Misc UI
    ToastClose(widget::toaster::ToastId),
    DismissWarning,
    OpenOutputDir,
    DialogError(String),
}

/// Toast auto-dismiss mapping (`fn` pointer required by [`widget::toaster`]).
fn toast_close(id: widget::toaster::ToastId) -> Message {
    Message::ToastClose(id)
}

/// `event::listen_with` mapper (must be a free `fn`, not a closure).
fn event_mapper(
    event: cosmic::iced::Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        cosmic::iced::Event::Window(window::Event::FileDropped(paths)) => {
            Some(Message::FilesDropped(paths))
        }
        cosmic::iced::Event::Window(window::Event::FileHovered(_)) => {
            Some(Message::FilesHovered)
        }
        cosmic::iced::Event::Window(window::Event::FilesHoveredLeft) => {
            Some(Message::FilesHoverLeft)
        }
        _ => None,
    }
}

/// Keyboard shortcut mapper for queue management.
///
/// Uses the modern `iced` 0.14 API (`keyboard::Key` + `Modifiers`), not the
/// legacy `KeyCode` API. Ignores events already handled by focused widgets
/// (e.g. text inputs) so typing in settings fields is never hijacked.
fn shortcut_mapper(
    event: cosmic::iced::Event,
    status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    // Only handle shortcuts when no widget consumed the event. This keeps
    // text-input editing (quality, resize) working with Ctrl+A etc.
    if status != event::Status::Ignored {
        return None;
    }

    let cosmic::iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        modifiers,
        ..
    }) = event
    else {
        return None;
    };

    // Ctrl-based shortcuts: Ctrl+A select all, Ctrl+D deselect,
    // Ctrl+O add files, Ctrl+R retry failed.
    if modifiers.control() && !modifiers.logo() && !modifiers.alt() && !modifiers.shift() {
        if let keyboard::Key::Character(c) = &key {
            match c.as_str().to_ascii_lowercase().as_str() {
                "a" => return Some(Message::SelectAllFiles),
                "d" => return Some(Message::DeselectAllFiles),
                "o" => return Some(Message::OpenFiles),
                "r" => return Some(Message::RetryFailed),
                _ => {}
            }
        }
        return None;
    }

    // Shift must not be held for single-key shortcuts below.
    if !modifiers.is_empty() {
        return None;
    }

    match key {
        keyboard::Key::Named(keyboard::key::Named::Delete)
        | keyboard::Key::Named(keyboard::key::Named::Backspace) => {
            Some(Message::RemoveSelectedFiles)
        }
        keyboard::Key::Named(keyboard::key::Named::Escape) => Some(Message::DeselectAllFiles),
        _ => None,
    }
}

/// Canonical key bindings shown in the menu bar and settings.
/// Kept in one place so menu labels and `shortcut_mapper` stay in sync.
fn app_key_binds() -> HashMap<KeyBind, MenuAction> {
    use cosmic::iced::keyboard::Key;
    let mut binds = HashMap::new();
    macro_rules! bind {
        ([$($modifier:ident),*], $key:expr, $action:ident) => {{
            binds.insert(
                KeyBind {
                    modifiers: vec![$(Modifier::$modifier),*],
                    key: $key,
                },
                MenuAction::$action,
            );
        }};
    }
    bind!([Ctrl], Key::Character("o".into()), OpenFiles);
    bind!([Ctrl], Key::Character("a".into()), SelectAll);
    bind!([Ctrl], Key::Character("d".into()), DeselectAll);
    bind!([Ctrl], Key::Character("r".into()), RetryFailed);
    binds
}

// ---------------------------------------------------------------------------
// cosmic::Application implementation
// ---------------------------------------------------------------------------

/// Create a COSMIC application from the app model
impl cosmic::Application for AppModel {
    /// The async executor that will be used to run your application's commands.
    type Executor = cosmic::executor::Default;

    /// Data that your application receives to its init method.
    type Flags = ();

    /// Messages which the application and its widgets will emit.
    type Message = Message;

    /// Unique identifier in RDNN (reverse domain name notation) format.
    const APP_ID: &'static str = "io.github.doctorit.Hawellha";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    /// Initializes the application with any given flags and startup commands.
    fn init(
        core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        // Tabs: each filters the queue by conversion domain.
        let mut nav = nav_bar::Model::default();
        nav.insert()
            .text("All files")
            .data::<Page>(Page::All)
            .icon(icon::from_name("folder-open-symbolic"))
            .activate();
        nav.insert()
            .text("Data")
            .data::<Page>(Page::Data)
            .icon(icon::from_name("x-office-spreadsheet-symbolic"));
        nav.insert()
            .text("Images")
            .data::<Page>(Page::Images)
            .icon(icon::from_name("image-x-generic-symbolic"));
        nav.insert()
            .text("Media")
            .data::<Page>(Page::Media)
            .icon(icon::from_name("audio-x-generic-symbolic"));
        nav.insert()
            .text("Documents")
            .data::<Page>(Page::Documents)
            .icon(icon::from_name("x-office-document-symbolic"));
        nav.insert()
            .text("History")
            .data::<Page>(Page::History)
            .icon(icon::from_name("document-open-recent-symbolic"));

        // Create the about widget
        let about = About::default()
            .name(fl!("app-title"))
            .icon(widget::icon::from_svg_bytes(APP_ICON))
            .version(env!("CARGO_PKG_VERSION"))
            .links([(fl!("repository"), REPOSITORY)])
            .license(env!("CARGO_PKG_LICENSE"));

        // Optional configuration file for an application.
        let config: Config = cosmic_config::Config::new(Self::APP_ID, Config::VERSION)
            .map(|context| match Config::get_entry(&context) {
                Ok(config) => config,
                Err((_errors, config)) => config,
            })
            .unwrap_or_default();

        let output_dir = if config.output_dir.is_empty() {
            default_output_dir()
        } else {
            PathBuf::from(&config.output_dir)
        };
        let quality_text = if config.jpeg_quality == 0 {
            "90".to_string()
        } else {
            config.jpeg_quality.min(100).to_string()
        };
        let resize_w_text = if config.resize_width == 0 {
            String::new()
        } else {
            config.resize_width.to_string()
        };
        let resize_h_text = if config.resize_height == 0 {
            String::new()
        } else {
            config.resize_height.to_string()
        };

        let ffmpeg_info = match crate::converters::media::ffmpeg_status() {
            Ok(line) => line,
            Err(e) => e.to_string(),
        };

        let history = load_history();
        let audio_bitrate_idx = audio_bitrate_index(if config.audio_bitrate_kbps == 0 {
            192
        } else {
            config.audio_bitrate_kbps
        });

        // Construct the app model with the runtime's core.
        let mut app = AppModel {
            core,
            context_page: ContextPage::default(),
            about,
            nav,
            key_binds: app_key_binds(),
            config,
            files: Vec::new(),
            next_id: 1,
            output_dir,
            global_target_idx: None,
            is_converting: false,
            toasts: widget::toaster::Toasts::new(toast_close),
            quality_text,
            resize_w_text,
            resize_h_text,
            warning: None,
            drag_hovered: false,
            ffmpeg_info,
            history,
            audio_bitrate_idx,
        };

        // Create a startup command that sets the window title.
        let command = app.update_title();

        (app, command)
    }

    /// Elements to pack at the start of the header bar.
    fn header_start(&self) -> Vec<Element<'_, Self::Message>> {
        let menu_bar = menu::bar(vec![
            menu::Tree::with_children(
                menu::root("File").apply(Element::from),
                menu::items(
                    &self.key_binds,
                    vec![
                        menu::Item::Button("Add files…".to_string(), None, MenuAction::OpenFiles),
                        menu::Item::Button("Retry failed".to_string(), None, MenuAction::RetryFailed),
                        menu::Item::Divider,
                        menu::Item::Button(fl!("settings"), None, MenuAction::Settings),
                        menu::Item::Button(fl!("about"), None, MenuAction::About),
                    ],
                ),
            ),
            menu::Tree::with_children(
                menu::root("Edit").apply(Element::from),
                menu::items(
                    &self.key_binds,
                    vec![
                        menu::Item::Button("Select all", None, MenuAction::SelectAll),
                        menu::Item::Button("Deselect all", None, MenuAction::DeselectAll),
                    ],
                ),
            ),
        ]);

        vec![menu_bar.into()]
    }

    /// Header actions: quick-add + selection controls + settings shortcut.
    fn header_end(&self) -> Vec<Element<'_, Self::Message>> {
        let selected_count = self.files.iter().filter(|f| f.selected).count();
        let has_files = !self.files.is_empty();

        vec![
            widget::button::standard("Add files")
                .on_press(Message::OpenFiles)
                .into(),
            if has_files {
                widget::button::standard("Select all")
                    .on_press(Message::SelectAllFiles)
                    .into()
            } else {
                widget::button::standard("Select all")
                    .into()
            },
            if selected_count > 0 {
                widget::button::standard("Deselect all")
                    .on_press(Message::DeselectAllFiles)
                    .into()
            } else {
                widget::button::standard("Deselect all")
                    .into()
            },
            if selected_count > 0 {
                widget::button::destructive("Remove selected")
                    .on_press(Message::RemoveSelectedFiles)
                    .into()
            } else {
                widget::button::destructive("Remove selected")
                    .into()
            },
            widget::button::icon(icon::from_name("emblem-system-symbolic"))
                .tooltip("Settings")
                .on_press(Message::ToggleContextPage(ContextPage::Settings))
                .into(),
        ]
    }

    /// Enables the COSMIC application to create a nav bar with this model.
    fn nav_model(&self) -> Option<&nav_bar::Model> {
        Some(&self.nav)
    }

    /// Batch controls live in the footer so they are always reachable.
    fn footer(&self) -> Option<Element<'_, Self::Message>> {
        Some(self.batch_bar())
    }

    /// Display a context drawer if the context page is requested.
    fn context_drawer(&self) -> Option<context_drawer::ContextDrawer<'_, Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }

        Some(match self.context_page {
            ContextPage::About => context_drawer::about(
                &self.about,
                |url| Message::LaunchUrl(url.to_string()),
                Message::ToggleContextPage(ContextPage::About),
            ),
            ContextPage::Settings => context_drawer::context_drawer(
                self.settings_view(),
                Message::ToggleContextPage(ContextPage::Settings),
            )
            .title(fl!("settings")),
        })
    }

    /// Dialog slot: surface non-modal warnings as a dismissible banner card.
    fn dialog(&self) -> Option<Element<'_, Self::Message>> {
        self.warning.as_deref().map(|text| {
            widget::container(
                widget::column::with_capacity(2)
                    .push(widget::text::title2("Heads up"))
                    .push(widget::text::body(text.to_string()))
                    .push(
                        widget::row::with_capacity(2)
                            .push(widget::button::standard("Dismiss").on_press(Message::DismissWarning))
                            .push(widget::button::text("Open output folder").on_press(Message::OpenOutputDir))
                            .spacing(8),
                    )
                    .spacing(8),
            )
            .padding(16)
            .width(Length::Fixed(420.0))
            .into()
        })
    }

    /// Describes the interface based on the current state of the application model.
    fn view(&self) -> Element<'_, Self::Message> {
        let space_s = cosmic::theme::spacing().space_s;
        let space_m = cosmic::theme::spacing().space_m;

        let mut content = widget::column::with_capacity(4)
            .spacing(space_m)
            .height(Length::Fill);

        content = content.push(self.drop_zone());

        // History tab renders past conversions instead of the live queue.
        if self.active_page() == Page::History {
            content = content.push(self.history_view());
            let body: Element<'_, Message> = widget::container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding([16, 16])
                .into();
            return widget::toaster(&self.toasts, body);
        }

        // Queue header row: count + helpers + selection info.
        let visible = self.visible_files();
        let selected_count = self.files.iter().filter(|f| f.selected).count();
        let header = widget::row::with_capacity(4)
            .push(widget::text::title2(format!(
                "Queue ({} file{})",
                visible.len(),
                if visible.len() == 1 { "" } else { "s" }
            )))
            .push(widget::space::horizontal().width(Length::Fill))
            .push(if selected_count > 0 {
                widget::text::caption(format!("{selected_count} selected"))
            } else {
                widget::text::caption("")
            })
            .push(
                widget::button::text("Clear finished").on_press_maybe(
                    if self.files.iter().any(|f| f.status == FileStatus::Done) {
                        Some(Message::ClearFinished)
                    } else {
                        None
                    },
                ),
            )
            .push(widget::button::text("Clear all").on_press_maybe(if self.files.is_empty() {
                None
            } else {
                Some(Message::ClearQueue)
            }))
            .spacing(space_s)
            .align_y(Alignment::Center);
        content = content.push(header);

        // Queue list (scrollable) with per-tab empty states.
        if visible.is_empty() {
            let hint = match self.active_page() {
                Page::Data => "No data files here yet. Add CSV, XLSX or JSON — or check All files.",
                Page::Images => "No images here yet. Add PNG, JPG, WebP or GIF — or check All files.",
                Page::Media => "No audio/video here yet. Add MP3, WAV, MP4 or MKV (needs ffmpeg).",
                Page::Documents => "No documents here yet. Add TXT, MD, PDF or HTML.",
                Page::All => "No files yet. Drop files above or use Add files — conversions run in the background.",
                Page::History => "No history.",
            };
            content = content.push(
                widget::container(widget::text::body(hint.to_string()))
                .padding(12)
                .width(Length::Fill),
            );
        } else {
            let mut list = widget::column::with_capacity(visible.len()).spacing(space_s);
            for rendered in visible {
                list = list.push(rendered);
            }
            content = content.push(widget::scrollable(list).height(Length::Fill));
        }

        let body: Element<'_, Message> = widget::container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([16, 16])
            .into();

        widget::toaster(&self.toasts, body)
    }

    /// Register subscriptions for this application.
    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::batch(vec![
            // Watch for application configuration changes.
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|update| Message::UpdateConfig(update.config)),
            // Native OS drag & drop of files onto the window.
            event::listen_with(event_mapper),
            // Keyboard shortcuts for queue management (Ctrl+A/D/O/R,
            // Delete/Backspace, Escape). Ignored while typing in inputs.
            event::listen_with(shortcut_mapper),
        ])
    }

    /// Handles messages emitted by the application and its widgets.
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::ToggleContextPage(context_page) => {
                if self.context_page == context_page {
                    self.core.window.show_context = !self.core.window.show_context;
                } else {
                    self.context_page = context_page;
                    self.core.window.show_context = true;
                }
            }

            Message::UpdateConfig(config) => {
                self.audio_bitrate_idx = audio_bitrate_index(if config.audio_bitrate_kbps == 0 {
                    192
                } else {
                    config.audio_bitrate_kbps
                });
                self.config = config;
            }

            Message::LaunchUrl(url) => match open::that_detached(&url) {
                Ok(()) => {}
                Err(err) => {
                    eprintln!("failed to open {url:?}: {err}");
                }
            },

            // --- File picking (XDG portal dialogs) ---
            Message::OpenFiles => {
                return cosmic::task::future(async move {
                    let dialog = cosmic::dialog::file_chooser::open::Dialog::new()
                        .title("Add files to convert");
                    match dialog.open_files().await {
                        Ok(response) => {
                            let paths: Vec<PathBuf> = response
                                .urls()
                                .iter()
                                .filter_map(|url| url.to_file_path().ok())
                                .collect();
                            Message::FilesChosen(paths)
                        }
                        Err(cosmic::dialog::file_chooser::Error::Cancelled) => {
                            Message::DismissWarning
                        }
                        Err(why) => Message::DialogError(format!("Could not open file picker: {why}")),
                    }
                });
            }

            Message::OpenFolderAsInput => {
                return cosmic::task::future(async move {
                    let dialog = cosmic::dialog::file_chooser::open::Dialog::new()
                        .title("Add a folder ( Queued as one archive job )");
                    match dialog.open_folder().await {
                        Ok(response) => {
                            let paths: Vec<PathBuf> = response
                                .url()
                                .to_file_path()
                                .ok()
                                .into_iter()
                                .collect();
                            Message::FilesChosen(paths)
                        }
                        Err(cosmic::dialog::file_chooser::Error::Cancelled) => {
                            Message::DismissWarning
                        }
                        Err(why) => Message::DialogError(format!("Could not open folder picker: {why}")),
                    }
                });
            }

            Message::FilesChosen(paths) => {
                self.add_paths(paths);
            }

            Message::ChooseOutputDir => {
                return cosmic::task::future(async move {
                    let dialog = cosmic::dialog::file_chooser::open::Dialog::new()
                        .title("Choose output folder");
                    match dialog.open_folder().await {
                        Ok(response) => match response.url().to_file_path() {
                            Ok(path) => Message::OutputDirChosen(path),
                            Err(_) => Message::DialogError(
                                "The chosen folder is not a local path.".to_string(),
                            ),
                        },
                        Err(cosmic::dialog::file_chooser::Error::Cancelled) => {
                            Message::DismissWarning
                        }
                        Err(why) => {
                            Message::DialogError(format!("Could not open folder picker: {why}"))
                        }
                    }
                });
            }

            Message::OutputDirChosen(path) => {
                self.output_dir = path.clone();
                self.config.output_dir = path.display().to_string();
                self.save_config();
                self.warning = None;
            }

            // --- Queue editing ---
            Message::RemoveFile(id) => {
                self.files.retain(|f| f.id != id);
                // A removed file may invalidate the batch dropdown selection.
                if self
                    .global_target_idx
                    .is_some_and(|i| self.batch_targets().get(i).is_none())
                {
                    self.global_target_idx = None;
                }
            }

            Message::ClearQueue => {
                // Never drop in-flight conversions from under the workers;
                // they finish and simply report into their queue slots.
                if self.is_converting {
                    self.files.retain(|f| f.status == FileStatus::Converting);
                    self.warning = Some(
                        "Conversions are still running — remaining files were kept until they finish.".to_string(),
                    );
                } else {
                    self.files.clear();
                }
                self.global_target_idx = None;
            }

            Message::ClearFinished => {
                self.files.retain(|f| f.status != FileStatus::Done);
            }

            Message::ToggleFileSelection(id) => {
                if let Some(file) = self.files.iter_mut().find(|f| f.id == id) {
                    file.selected = !file.selected;
                }
            }

            Message::SelectAllFiles => {
                for file in &mut self.files {
                    file.selected = true;
                }
            }

            Message::DeselectAllFiles => {
                for file in &mut self.files {
                    file.selected = false;
                }
            }

            Message::RemoveSelectedFiles => {
                if self.is_converting {
                    // Only remove non-converting selected files
                    self.files.retain(|f| !(f.selected && f.status != FileStatus::Converting));
                    self.warning = Some(
                        "Converting files were kept; other selected files were removed.".to_string(),
                    );
                } else {
                    self.files.retain(|f| !f.selected);
                }
                // Reset selection
                for file in &mut self.files {
                    file.selected = false;
                }
                // Reset batch selection if needed
                if self
                    .global_target_idx
                    .is_some_and(|i| self.batch_targets().get(i).is_none())
                {
                    self.global_target_idx = None;
                }
            }

            Message::RetryFile(id) => {
                if let Some(file) = self.files.iter_mut().find(|f| f.id == id) {
                    if file.status == FileStatus::Failed {
                        file.status = FileStatus::Pending;
                        file.progress = 0.0;
                        file.error = None;
                        file.start_time = None;
                        file.end_time = None;
                    }
                }
            }

            Message::RetryFailed => {
                let mut retried = 0;
                for file in &mut self.files {
                    if file.status == FileStatus::Failed {
                        file.status = FileStatus::Pending;
                        file.progress = 0.0;
                        file.error = None;
                        file.start_time = None;
                        file.end_time = None;
                        retried += 1;
                    }
                }
                if retried == 0 {
                    self.warning = Some("Nothing to retry — no failed conversions.".to_string());
                } else {
                    self.warning = None;
                }
            }

            Message::MoveFileUp(id) => {
                if let Some(pos) = self.files.iter().position(|f| f.id == id) {
                    if pos > 0 {
                        self.files.swap(pos, pos - 1);
                    }
                }
            }

            Message::MoveFileDown(id) => {
                if let Some(pos) = self.files.iter().position(|f| f.id == id) {
                    if pos < self.files.len() - 1 {
                        self.files.swap(pos, pos + 1);
                    }
                }
            }

            Message::SetFileTarget(id, idx) => {
                if let Some(file) = self.files.iter_mut().find(|f| f.id == id) {
                    if idx < file.targets.len() {
                        file.target_idx = Some(idx);
                        file.error = None;
                        // Re-validate instantly so unsupported pairs alert at once.
                        if let Some(err) = file.validation_error() {
                            file.error = Some(err);
                        }
                        if file.status == FileStatus::Failed {
                            file.status = FileStatus::Pending;
                            file.progress = 0.0;
                        }
                    }
                }
            }

            Message::SetGlobalTarget(idx) => {
                self.global_target_idx = Some(idx);
            }

            Message::ApplyGlobalTarget => {
                if let Some(target) = self
                    .global_target_idx
                    .and_then(|i| self.batch_targets().get(i).cloned())
                {
                    let mut applied = 0;
                    let mut skipped = 0;
                    for file in &mut self.files {
                        if file.status == FileStatus::Converting {
                            continue;
                        }
                        if is_supported(&file.source_ext, &target) {
                            if let Some(pos) = file.targets.iter().position(|t| *t == target) {
                                file.target_idx = Some(pos);
                                file.error = None;
                                if file.status == FileStatus::Failed {
                                    file.status = FileStatus::Pending;
                                    file.progress = 0.0;
                                }
                                applied += 1;
                            } else {
                                skipped += 1;
                            }
                        } else {
                            skipped += 1;
                        }
                    }
                    if skipped > 0 {
                        self.warning = Some(format!(
                            "Applied “.{target}” to {applied} file(s); {skipped} file(s) do not support that target and were left unchanged."
                        ));
                    } else {
                        self.warning = None;
                    }
                }
            }

            // --- Conversion ---
            Message::ConvertAll => {
                return self.start_batch(false);
            }

            Message::ConvertSelected => {
                return self.start_batch(true);
            }

            Message::StopConversion => {
                if self.is_converting {
                    self.is_converting = false;
                    // Mark all converting files as failed
                    for file in &mut self.files {
                        if file.status == FileStatus::Converting {
                            file.status = FileStatus::Failed;
                            file.progress = 0.0;
                            file.error = Some("Conversion cancelled by user".to_string());
                            file.end_time = Some(std::time::Instant::now());
                        }
                    }
                    self.warning = Some("Conversion stopped by user".to_string());
                }
            }

            Message::ConversionFinished(id, result) => {
                let mut finished_task = Task::none();
                // Capture queue metadata for history before the mutable borrow ends.
                let mut history_entry: Option<HistoryEntry> = None;
                if let Some(file) = self.files.iter_mut().find(|f| f.id == id) {
                    file.end_time = Some(std::time::Instant::now());
                    match result {
                        Ok(path) => {
                            file.status = FileStatus::Done;
                            file.progress = 1.0;
                            file.error = None;
                            file.output_path = Some(path.clone());
                            history_entry = Some(HistoryEntry {
                                input_name: file.file_name.clone(),
                                source_ext: file.source_ext.clone(),
                                target: file
                                    .selected_target()
                                    .unwrap_or_default()
                                    .to_string(),
                                output_display: path.display().to_string(),
                                success: true,
                                finished_at: HistoryEntry::now_secs(),
                                favorite: false,
                            });
                        }
                        Err(err) => {
                            file.status = FileStatus::Failed;
                            file.progress = 0.0;
                            file.error = Some(err.clone());
                            history_entry = Some(HistoryEntry {
                                input_name: file.file_name.clone(),
                                source_ext: file.source_ext.clone(),
                                target: file
                                    .selected_target()
                                    .unwrap_or_default()
                                    .to_string(),
                                output_display: String::new(),
                                success: false,
                                finished_at: HistoryEntry::now_secs(),
                                favorite: false,
                            });
                        }
                    }
                }
                if let Some(entry) = history_entry {
                    self.history.push(entry);
                    if self.history.len() > 100 {
                        let overflow = self.history.len() - 100;
                        self.history.drain(..overflow);
                    }
                    save_history(&self.history);
                }
                if self.files.iter().all(|f| f.status != FileStatus::Converting) {
                    self.is_converting = false;
                    finished_task = self.finish_batch();
                }
                return finished_task;
            }

            // --- Settings fields ---
            Message::QualityInput(value) => {
                self.quality_text = value;
                if let Ok(q) = self.quality_text.trim().parse::<u32>() {
                    if (1..=100).contains(&q) {
                        self.config.jpeg_quality = q;
                        self.save_config();
                    }
                }
            }
            Message::ResizeWInput(value) => {
                self.resize_w_text = value;
                self.config.resize_width =
                    self.resize_w_text.trim().parse::<u32>().unwrap_or(0);
                self.save_config();
            }
            Message::ResizeHInput(value) => {
                self.resize_h_text = value;
                self.config.resize_height =
                    self.resize_h_text.trim().parse::<u32>().unwrap_or(0);
                self.save_config();
            }
            Message::ToggleOverwrite(enabled) => {
                self.config.overwrite_existing = enabled;
                self.save_config();
            }
            Message::ToggleGroupByKind(enabled) => {
                self.config.group_by_kind = enabled;
                self.save_config();
            }
            Message::ToggleAutoOpen(enabled) => {
                self.config.auto_open_output = enabled;
                self.save_config();
            }
            Message::SetAudioBitrate(idx) => {
                if let Some(bitrate) = AUDIO_BITRATES.get(idx).copied() {
                    self.audio_bitrate_idx = Some(idx);
                    self.config.audio_bitrate_kbps = bitrate;
                    self.save_config();
                }
            }
            Message::ClearHistory => {
                self.history.clear();
                save_history(&self.history);
            }
            Message::ToggleHistoryFavorite(idx) => {
                if let Some(entry) = self.history.get_mut(idx) {
                    entry.favorite = !entry.favorite;
                    save_history(&self.history);
                }
            }
            Message::OpenPath(path) => {
                if let Err(err) = open::that_detached(&path) {
                    self.warning = Some(format!("Could not open {}: {err}", path.display()));
                }
            }

            // --- Drag & drop ---
            Message::FilesHovered => {
                self.drag_hovered = true;
            }
            Message::FilesHoverLeft => {
                self.drag_hovered = false;
            }
            Message::FilesDropped(paths) => {
                self.drag_hovered = false;
                // Directories are accepted (queued for archiving); the worker
                // validates readability per file at conversion time.
                self.add_paths(paths);
            }

            // --- Misc ---
            Message::ToastClose(id) => {
                self.toasts.remove(id);
            }
            Message::DismissWarning => {
                self.warning = None;
            }
            Message::OpenOutputDir => {
                let dir = self.output_dir.clone();
                std::fs::create_dir_all(&dir).ok();
                if let Err(err) = open::that_detached(&dir) {
                    self.warning = Some(format!("Could not open output folder: {err}"));
                } else {
                    self.warning = None;
                }
            }
            Message::DialogError(err) => {
                self.warning = Some(err);
            }
        }
        Task::none()
    }

    /// Called when a nav item is selected.
    fn on_nav_select(&mut self, id: nav_bar::Id) -> Task<cosmic::Action<Self::Message>> {
        // Activate the page in the model.
        self.nav.activate(id);

        self.update_title()
    }
}

// ---------------------------------------------------------------------------
// Business logic (queue + batch)
// ---------------------------------------------------------------------------

impl AppModel {
    /// Updates the header and window titles.
    pub fn update_title(&mut self) -> Task<cosmic::Action<Message>> {
        let mut window_title = fl!("app-title");

        if let Some(page) = self.nav.text(self.nav.active()) {
            window_title.push_str(" — ");
            window_title.push_str(page);
        }

        if let Some(id) = self.core.main_window_id() {
            self.set_window_title(window_title, id)
        } else {
            Task::none()
        }
    }

    /// Persist [`Config`] to cosmic-config (best-effort).
    fn save_config(&self) {
        if let Ok(context) = cosmic_config::Config::new(Self::APP_ID, Config::VERSION) {
            let _ = self.config.write_entry(&context);
        }
    }

    /// Active tab, defaulting to `All` when nothing is selected.
    fn active_page(&self) -> Page {
        self.nav
            .active_data::<Page>()
            .copied()
            .unwrap_or(Page::All)
    }

    /// Queue entries visible under the active tab.
    fn visible_files(&self) -> Vec<Element<'_, Message>> {
        let page = self.active_page();
        self.files
            .iter()
            .filter(|f| page.matches(f.kind))
            .map(|f| self.file_card(f))
            .collect()
    }

    /// Add dropped/picked paths to the queue (deduped, validated).
    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let mut added = 0;
        let mut rejected: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        for path in paths {
            // Skip duplicates already queued.
            if self.files.iter().any(|f| f.path == path) {
                warnings.push(format!(
                    "{} (already in queue)",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                ));
                continue;
            }
            if !path.is_file() && !path.is_dir() {
                rejected.push(format!(
                    "{} (not found or unreadable)",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                ));
                continue;
            }
            let mut entry = QueuedFile::from_path(self.next_id, path);
            self.next_id += 1;
            if entry.targets.is_empty() {
                // Truly unknown extension with no pack route (shouldn't
                // happen since Unknown → zip/tar.gz, but stay total).
                entry.error = Some("Unsupported file type".to_string());
            } else if let Some(err) = entry.validation_error() {
                entry.error = Some(err);
            }
            self.files.push(entry);
            added += 1;
        }

        // Build comprehensive warning message
        let mut warning_parts = Vec::new();
        if added > 0 {
            warning_parts.push(format!("Added {added} file(s)"));
        }
        if !rejected.is_empty() {
            warning_parts.push(format!("Skipped: {}", rejected.join(", ")));
        }
        if !warnings.is_empty() {
            warning_parts.push(format!("Notes: {}", warnings.join(", ")));
        }

        if !warning_parts.is_empty() {
            self.warning = Some(warning_parts.join(". "));
        } else {
            self.warning = None;
        }

        // Reset an out-of-range batch selection after queue changes.
        if self
            .global_target_idx
            .is_some_and(|i| self.batch_targets().get(i).is_none())
        {
            self.global_target_idx = None;
        }
    }

    /// Batch dropdown options for the current queue contents.
    fn batch_targets(&self) -> Vec<String> {
        let exts: Vec<String> = self
            .files
            .iter()
            .filter(|f| f.status != FileStatus::Converting)
            .map(|f| f.source_ext.clone())
            .collect();
        batch_targets_for(&exts)
    }

    /// Build [`ConvertOptions`] from the settings fields.
    fn convert_options(&self) -> ConvertOptions {
        let bitrate = self
            .audio_bitrate_idx
            .and_then(|i| AUDIO_BITRATES.get(i).copied())
            .unwrap_or_else(|| {
                let cfg = self.config.audio_bitrate_kbps;
                if cfg == 0 { 192 } else { cfg.clamp(64, 320) }
            });
        ConvertOptions {
            jpeg_quality: self.quality_text.trim().parse::<u8>().unwrap_or(90).clamp(1, 100),
            webp_quality: 90,
            resize_width: self.resize_w_text.trim().parse::<u32>().ok().filter(|w| *w > 0),
            resize_height: self.resize_h_text.trim().parse::<u32>().ok().filter(|h| *h > 0),
            csv_delimiter: None, // auto-detect; a manual override lives in a future update
            audio_bitrate_kbps: bitrate,
        }
    }

    /// Destination path honoring grouping + overwrite preferences.
    /// (Kept as a pure helper for tests; `start_batch` inlines the same
    /// logic to avoid borrow conflicts while iterating `&mut files`.)
    #[allow(dead_code)]
    fn output_path_for(&self, file: &QueuedFile, target: &str) -> PathBuf {
        let mut dir = self.output_dir.clone();
        if self.config.group_by_kind {
            dir = dir.join(kind_dir_name(file.kind));
            let _ = std::fs::create_dir_all(&dir);
        }
        let base = resolve_output_path(&file.path, target, &dir);
        if self.config.overwrite_existing {
            base
        } else {
            unique_output_path(base)
        }
    }

    /// Kick off a batch conversion; returns the worker [`Task`]s.
    /// If `selected_only` is true, only convert selected files.
    fn start_batch(&mut self, selected_only: bool) -> Task<cosmic::Action<Message>> {
        if self.is_converting {
            return Task::none();
        }
        // Ensure the destination exists before spawning workers.
        if let Err(err) = std::fs::create_dir_all(&self.output_dir) {
            self.warning = Some(format!(
                "Cannot use output folder {}: {err}",
                self.output_dir.display()
            ));
            return Task::none();
        }

        let opts = self.convert_options();
        let out_dir = self.output_dir.clone();
        let group_by_kind = self.config.group_by_kind;
        let overwrite = self.config.overwrite_existing;
        let mut tasks = Vec::new();
        let mut skipped = 0;

        for file in &mut self.files {
            // Skip if selected_only and file not selected
            if selected_only && !file.selected {
                continue;
            }

            if file.status == FileStatus::Converting || file.status == FileStatus::Done {
                continue;
            }
            let Some(target) = file.selected_target().map(str::to_string) else {
                file.error = Some("Pick a target format first".to_string());
                file.status = FileStatus::Failed;
                skipped += 1;
                continue;
            };
            if !is_supported(&file.source_ext, &target) {
                file.error = Some(format!(
                    "Unsupported conversion: .{} → .{target}",
                    file.source_ext
                ));
                file.status = FileStatus::Failed;
                skipped += 1;
                continue;
            }
            // Directories can only be packed, never "converted" cell-wise.
            if file.path.is_dir() && target != "zip" && target != "tar.gz" {
                file.error = Some("Folders can only be packed into .zip / .tar.gz".to_string());
                file.status = FileStatus::Failed;
                skipped += 1;
                continue;
            }

            file.status = FileStatus::Converting;
            file.progress = 0.5;
            file.error = None;
            file.start_time = Some(std::time::Instant::now());
            file.end_time = None;
            // Grouped outputs (images/, data/, …) + safe rename when needed.
            let mut dir = out_dir.clone();
            if group_by_kind {
                dir = dir.join(kind_dir_name(file.kind));
                let _ = std::fs::create_dir_all(&dir);
            }
            let base = resolve_output_path(&file.path, &target, &dir);
            let output = if overwrite { base } else { unique_output_path(base) };
            file.output_path = Some(output.clone());

            let input = file.path.clone();
            let id = file.id;
            let opts = opts.clone();
            tasks.push(cosmic::task::future(async move {
                // Heavy lifting off the GUI thread (pooled workers).
                let outcome = tokio::task::spawn_blocking(move || {
                    converters::convert_file(&input, &output, &opts)
                        .map(|()| output)
                        .map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(format!("worker failed: {e}")));
                Message::ConversionFinished(id, outcome)
            }));
        }

        if tasks.is_empty() {
            self.warning = Some(if skipped > 0 {
                format!("Nothing to convert — {skipped} file(s) need a valid target format.")
            } else if selected_only {
                "Nothing to convert — select files first.".to_string()
            } else {
                "Nothing to convert — add files first.".to_string()
            });
            return Task::none();
        }
        self.is_converting = true;
        if skipped > 0 {
            self.warning = Some(format!("{skipped} file(s) skipped (invalid target)."));
        } else {
            self.warning = None;
        }
        Task::batch(tasks)
    }

    /// Summarise a finished batch: toast + native desktop notification.
    fn finish_batch(&mut self) -> Task<cosmic::Action<Message>> {
        let done = self.files.iter().filter(|f| f.status == FileStatus::Done).count();
        let failed = self.files.iter().filter(|f| f.status == FileStatus::Failed).count();
        let summary = if failed == 0 {
            format!("Converted {done} file(s) to {}", self.output_dir.display())
        } else {
            format!("Finished: {done} converted, {failed} failed — see the queue for details")
        };

        // Native desktop notification (best-effort; headless sessions ignore).
        // An OS thread is used deliberately: `update` must not depend on an
        // ambient tokio runtime context.
        let body = summary.clone();
        std::thread::spawn(move || {
            let _ = notify_rust::Notification::new()
                .summary("Hawellha — conversion finished")
                .body(&body)
                .appname("Hawellha")
                .show();
        });

        // Auto-open output folder when enabled (best-effort).
        if self.config.auto_open_output && done > 0 {
            let dir = self.output_dir.clone();
            std::fs::create_dir_all(&dir).ok();
            let _ = open::that_detached(&dir);
        }

        self.toasts
            .push(widget::toaster::Toast::new(summary))
            .map(cosmic::Action::App)
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

impl AppModel {
    /// Drop-zone card with hover highlight + picker buttons + drag stats.
    fn drop_zone(&self) -> Element<'_, Message> {
        let space_s = cosmic::theme::spacing().space_s;
        let body = crate::ui::components::drop_zone_body::<Message>(self.drag_hovered);

        // Add file type statistics when files are queued
        let stats = if !self.files.is_empty() {
            let mut counts: std::collections::HashMap<FileKind, usize> = std::collections::HashMap::new();
            for file in &self.files {
                *counts.entry(file.kind).or_insert(0) += 1;
            }

            let stats_text = if counts.len() > 1 {
                let kinds: Vec<String> = counts.iter()
                    .map(|(kind, count)| format!("{} {}", count, self.kind_name(*kind)))
                    .collect();
                format!("Queue: {}", kinds.join(", "))
            } else {
                let (kind, count) = counts.iter().next().unwrap();
                format!("Queue: {} {}", count, self.kind_name(*kind))
            };

            widget::text::caption(stats_text)
        } else {
            widget::text::caption("")
        };

        let buttons = widget::row::with_capacity(3)
            .push(widget::button::suggested("Add files").on_press(Message::OpenFiles))
            .push(widget::button::standard("Add folder").on_press(Message::OpenFolderAsInput))
            .push(widget::button::standard("Choose output folder").on_press(Message::ChooseOutputDir))
            .spacing(space_s)
            .align_y(Alignment::Center);
        let inner = widget::column::with_capacity(3)
            .push(body)
            .push(stats)
            .push(buttons)
            .spacing(space_s)
            .align_x(Alignment::Center)
            .width(Length::Fill);
        widget::container(inner)
            .padding(24)
            .width(Length::Fill)
            .class(if self.drag_hovered {
                cosmic::theme::Container::Primary
            } else {
                cosmic::theme::Container::Card
            })
            .into()
    }

    /// Get human-readable name for a file kind.
    fn kind_name(&self, kind: FileKind) -> &'static str {
        match kind {
            FileKind::Tabular => "data files",
            FileKind::Image => "images",
            FileKind::Audio => "audio files",
            FileKind::Video => "videos",
            FileKind::Document => "documents",
            FileKind::Archive => "archives",
            FileKind::Unknown => "files",
        }
    }

    /// One queue card: icon, name/meta, target dropdown, progress, actions.
    fn file_card(&self, file: &QueuedFile) -> Element<'_, Message> {
        let space_xxs = cosmic::theme::spacing().space_xxs;
        let space_s = cosmic::theme::spacing().space_s;

        let kind_icon = crate::ui::components::file_icon::<Message>(file.kind);
        let badge = crate::ui::components::source_badge::<Message>(&file.source_ext);

        let name = widget::text::body(file.file_name.clone());

        // Calculate duration if available
        let duration_text = if let (Some(start), Some(end)) = (file.start_time, file.end_time) {
            let duration = end.duration_since(start);
            let secs = duration.as_secs();
            let millis = duration.subsec_millis();
            if secs > 0 {
                format!("· {}.{:03}s", secs, millis)
            } else {
                format!("· {}ms", millis)
            }
        } else {
            String::new()
        };

        let meta = widget::text::caption(format!(
            "{} · {} · {}{}",
            crate::ui::components::human_size(file.size),
            if file.mime.is_empty() { "—".to_string() } else { file.mime.clone() },
            crate::ui::components::status_label(&file.status),
            duration_text
        ));

        // Target picker limited to formats valid for this source — the
        // primary instant-validation mechanism (invalid pairs can't even
        // be selected, except via batch-apply which re-validates).
        let id = file.id;
        let picker: Element<'_, Message> = if file.targets.is_empty() {
            widget::text::caption("no targets available").into()
        } else {
            widget::dropdown(file.targets.clone(), file.selected_index(), move |idx| {
                Message::SetFileTarget(id, idx)
            })
            .into()
        };

        let progress: Element<'_, Message> = match file.status {
            FileStatus::Converting => widget::progress_bar::indeterminate_linear().into(),
            _ => widget::progress_bar::determinate_linear(file.progress).into(),
        };

        let mut details = widget::column::with_capacity(6).push(name);
        details = details.push(meta);
        if !file.preview.is_empty() {
            details = details.push(widget::text::caption(format!("◷ {}", file.preview)));
        }
        let target_row = widget::row::with_capacity(3)
            .push(badge)
            .push(widget::text::caption("→"))
            .push(picker)
            .spacing(space_xxs)
            .align_y(Alignment::Center);
        details = details.push(target_row);
        details = details.push(progress);
        if let Some(err) = file.error.as_deref() {
            details = details.push(widget::text::caption(format!("⚠ {err}")));
        } else if let Some(out) = file.output_path.as_ref() {
            if file.status == FileStatus::Done {
                details = details.push(widget::text::caption(format!("Saved to {}", out.display())));
            }
        }

        // Selection checkbox
        let checkbox = widget::checkbox(file.selected)
            .on_toggle(move |_| Message::ToggleFileSelection(id));

        // Action buttons based on status
        let mut action_widgets: Vec<Element<'_, Message>> = Vec::new();

        // Retry button for failed files
        if file.status == FileStatus::Failed {
            action_widgets.push(
                widget::button::icon(icon::from_name("view-refresh-symbolic"))
                    .tooltip("Retry conversion")
                    .on_press(Message::RetryFile(file.id))
                    .into()
            );
        }

        // Open output for finished files (smarter queue: one click to result).
        if file.status == FileStatus::Done {
            if let Some(out) = file.output_path.clone() {
                action_widgets.push(
                    widget::button::icon(icon::from_name("document-open-symbolic"))
                        .tooltip("Open converted file")
                        .on_press(Message::OpenPath(out))
                        .into(),
                );
            }
        }

        // Move up/down buttons
        let pos = self.files.iter().position(|f| f.id == file.id).unwrap_or(0);
        let mut move_buttons: Vec<Element<'_, Message>> = Vec::new();
        if pos > 0 {
            move_buttons.push(
                widget::button::icon(icon::from_name("go-up-symbolic"))
                    .tooltip("Move up")
                    .on_press(Message::MoveFileUp(file.id))
                    .into()
            );
        }
        if pos < self.files.len() - 1 {
            move_buttons.push(
                widget::button::icon(icon::from_name("go-down-symbolic"))
                    .tooltip("Move down")
                    .on_press(Message::MoveFileDown(file.id))
                    .into()
            );
        }
        if !move_buttons.is_empty() {
            action_widgets.push(
                widget::row::with_children(move_buttons)
                    .spacing(space_xxs)
                    .into()
            );
        }

        // Remove button
        let remove = if file.status == FileStatus::Converting {
            widget::button::icon(icon::from_name("process-stop-symbolic")).tooltip("Converting…")
        } else {
            widget::button::icon(icon::from_name("user-trash-symbolic"))
                .tooltip("Remove from queue")
                .on_press(Message::RemoveFile(file.id))
        };
        action_widgets.push(remove.into());

        let actions = if action_widgets.is_empty() {
            widget::column::with_capacity(0).spacing(space_xxs)
        } else {
            let mut col = widget::column::with_capacity(action_widgets.len()).spacing(space_xxs);
            for widget in action_widgets {
                col = col.push(widget);
            }
            col
        };

        let row = widget::row::with_capacity(4)
            .push(checkbox)
            .push(kind_icon)
            .push(details.width(Length::Fill).spacing(space_xxs))
            .push(actions)
            .spacing(space_s)
            .align_y(Alignment::Center);

        widget::container(row)
            .padding(12)
            .width(Length::Fill)
            .class(if file.selected {
                cosmic::theme::Container::Primary
            } else {
                cosmic::theme::Container::Card
            })
            .into()
    }

    /// Footer batch bar: global target, output dir, Convert All/Selected, Stop.
    fn batch_bar(&self) -> Element<'_, Message> {
        let space_s = cosmic::theme::spacing().space_s;
        let targets = self.batch_targets();
        let selected = self
            .global_target_idx
            .filter(|i| targets.get(*i).is_some());

        let picker: Element<'_, Message> = if targets.is_empty() {
            widget::text::caption("Add files to choose a target").into()
        } else {
            widget::row::with_capacity(2)
                .push(widget::text::body("Target:"))
                .push(widget::dropdown(targets, selected, Message::SetGlobalTarget))
                .push(
                    widget::button::standard("Apply to all")
                        .on_press_maybe(selected.map(|_| Message::ApplyGlobalTarget)),
                )
                .spacing(space_s)
                .align_y(Alignment::Center)
                .into()
        };

        let out_label = self
            .output_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("Output: …/{n}"))
            .unwrap_or_else(|| format!("Output: {}", self.output_dir.display()));
        let pending = self
            .files
            .iter()
            .filter(|f| f.status == FileStatus::Pending || f.status == FileStatus::Failed)
            .count();
        let selected_count = self.files.iter().filter(|f| f.selected).count();

        widget::container(
            widget::row::with_capacity(5)
                .push(picker)
                .push(widget::space::horizontal().width(Length::Fill))
                .push(
                    widget::button::text(out_label).on_press(Message::ChooseOutputDir),
                )
                .push(if self.is_converting {
                    widget::button::destructive("Stop")
                        .on_press(Message::StopConversion)
                } else if selected_count > 0 {
                    widget::button::suggested(format!("Convert {} selected", selected_count))
                        .on_press(Message::ConvertSelected)
                } else if pending > 0 {
                    widget::button::suggested("Convert all")
                        .on_press(Message::ConvertAll)
                } else {
                    widget::button::suggested("Convert")
                        .into()
                })
                .spacing(space_s)
                .align_y(Alignment::Center),
        )
        .padding([8, 16])
        .width(Length::Fill)
        .into()
    }

    /// Settings context drawer: output dir, JPEG quality, resize, ffmpeg, presets, shortcuts.
    fn settings_view(&self) -> Element<'_, Message> {
        let space_s = cosmic::theme::spacing().space_s;
        let space_xxs = cosmic::theme::spacing().space_xxs;

        widget::settings::section()
            .title("Conversion settings")
            .add(
                widget::settings::item::builder("Output folder").description("Where converted files will be saved").control(
                    widget::button::text(self.output_dir.display().to_string())
                        .on_press(Message::ChooseOutputDir),
                ),
            )
            .add(
                widget::settings::item::builder("Overwrite existing").description("Off = auto-rename to file_1.ext").control(
                    widget::toggler(self.config.overwrite_existing).on_toggle(Message::ToggleOverwrite),
                ),
            )
            .add(
                widget::settings::item::builder("Group by type").description("Save into images/, data/, … subfolders").control(
                    widget::toggler(self.config.group_by_kind).on_toggle(Message::ToggleGroupByKind),
                ),
            )
            .add(
                widget::settings::item::builder("Auto-open output").description("Open the folder when a batch finishes").control(
                    widget::toggler(self.config.auto_open_output).on_toggle(Message::ToggleAutoOpen),
                ),
            )
            .add(
                widget::settings::item::builder("JPEG quality (1–100)").description("Higher quality = larger file size").control(
                    widget::text_input("90", self.quality_text.clone())
                        .on_input(Message::QualityInput),
                ),
            )
            .add(
                widget::settings::item::builder("Image width px").description("Empty = keep original width").control(
                    widget::text_input("", self.resize_w_text.clone())
                        .on_input(Message::ResizeWInput),
                ),
            )
            .add(
                widget::settings::item::builder("Image height px").description("Empty = keep original height").control(
                    widget::text_input("", self.resize_h_text.clone())
                        .on_input(Message::ResizeHInput),
                ),
            )
            .add(
                widget::settings::item::builder("Audio bitrate").description("Quality for MP3/AAC/OGG targets").control(
                    widget::dropdown(
                        audio_bitrate_labels(),
                        self.audio_bitrate_idx,
                        Message::SetAudioBitrate,
                    ),
                ),
            )
            .add(
                widget::settings::item::builder("Quick presets").description("Common quality/size combinations").control(
                    widget::row::with_capacity(3)
                        .spacing(space_s)
                        .push(widget::button::standard("Web (80%)").on_press(Message::QualityInput("80".to_string())))
                        .push(widget::button::standard("Print (95%)").on_press(Message::QualityInput("95".to_string())))
                        .push(widget::button::standard("Max (100%)").on_press(Message::QualityInput("100".to_string())))
                ),
            )
            .add(
                widget::settings::item::builder("Media backend").description("FFmpeg for audio/video conversion").control(
                    widget::text::caption(self.ffmpeg_info.clone()),
                ),
            )
            .add(
                widget::settings::item::builder("Keyboard shortcuts").description("Quick actions — disabled while typing in text fields").control(
                    widget::column::with_capacity(6)
                        .spacing(space_xxs)
                        .push(widget::text::caption("Ctrl+O — Add files"))
                        .push(widget::text::caption("Ctrl+A — Select all files"))
                        .push(widget::text::caption("Ctrl+D or Esc — Deselect all"))
                        .push(widget::text::caption("Delete / Backspace — Remove selected"))
                        .push(widget::text::caption("Ctrl+R — Retry failed conversions"))
                ),
            )
            .into()
    }

    /// History tab: past conversions with favorites, open + clear.
    fn history_view(&self) -> Element<'_, Message> {
        let space_s = cosmic::theme::spacing().space_s;
        let space_xxs = cosmic::theme::spacing().space_xxs;

        let header = widget::row::with_capacity(3)
            .push(widget::text::title2(format!(
                "History ({} {})",
                self.history.len(),
                if self.history.len() == 1 {
                    "entry"
                } else {
                    "entries"
                }
            )))
            .push(widget::space::horizontal().width(Length::Fill))
            .push(widget::button::text("Clear history").on_press_maybe(if self.history.is_empty() {
                None
            } else {
                Some(Message::ClearHistory)
            }))
            .spacing(space_s)
            .align_y(Alignment::Center);

        let mut content = widget::column::with_capacity(2)
            .spacing(space_s)
            .push(header);

        if self.history.is_empty() {
            content = content.push(
                widget::container(widget::text::body(
                    "No conversions yet. Converted files will appear here with ★ favorites, even after restart.",
                ))
                .padding(12)
                .width(Length::Fill),
            );
            return content.into();
        }

        // Newest first.
        let mut list = widget::column::with_capacity(self.history.len()).spacing(space_xxs);
        for (rev_idx, entry) in self.history.iter().rev().enumerate() {
            let idx = self.history.len() - 1 - rev_idx;
            let status = if entry.success { "✓" } else { "✗" };
            let target = if entry.target.is_empty() {
                String::new()
            } else {
                format!(".{} → .{}", entry.source_ext, entry.target)
            };
            let detail = if entry.success && !entry.output_display.is_empty() {
                entry.output_display.clone()
            } else if entry.success {
                String::new()
            } else {
                "failed — see queue for details".to_string()
            };
            let star = if entry.favorite { "★" } else { "☆" };
            let open_btn: Element<'_, Message> = if entry.success && !entry.output_display.is_empty() {
                let path = PathBuf::from(entry.output_display.clone());
                widget::button::icon(icon::from_name("document-open-symbolic"))
                    .tooltip("Open result")
                    .on_press(Message::OpenPath(path))
                    .into()
            } else {
                widget::space::horizontal().width(Length::Fixed(0.0)).into()
            };
            let row = widget::row::with_capacity(4)
                .push(widget::text::caption(status.to_string()))
                .push(
                    widget::column::with_capacity(2)
                        .push(widget::text::body(format!("{} {target}", entry.input_name)))
                        .push(widget::text::caption(detail))
                        .spacing(2)
                        .width(Length::Fill),
                )
                .push(
                    widget::button::icon(icon::from_name(if entry.favorite {
                        "starred-symbolic"
                    } else {
                        "non-starred-symbolic"
                    }))
                    .tooltip(if entry.favorite {
                        "Unfavorite"
                    } else {
                        "Favorite"
                    })
                    // Fall back to a text star when the icon theme lacks stars.
                    .on_press(Message::ToggleHistoryFavorite(idx)),
                )
                .push(widget::text::caption(star.to_string()))
                .push(open_btn)
                .spacing(space_s)
                .align_y(Alignment::Center);
            list = list.push(
                widget::container(row)
                    .padding(10)
                    .width(Length::Fill)
                    .class(cosmic::theme::Container::Card),
            );
        }
        content = content.push(widget::scrollable(list).height(Length::Fill));
        content.into()
    }
}

/// The page to display in the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    All,
    Data,
    Images,
    Media,
    Documents,
    History,
}

impl Page {
    /// Whether a file kind belongs under this tab. Archives surface under
    /// Documents (pack/unpack is document-adjacent) as well as All.
    /// History never matches files — it renders `history` instead of the queue.
    fn matches(self, kind: FileKind) -> bool {
        match self {
            Page::All => true,
            Page::Data => matches!(kind, FileKind::Tabular),
            Page::Images => matches!(kind, FileKind::Image),
            Page::Media => matches!(kind, FileKind::Audio | FileKind::Video),
            Page::Documents => matches!(
                kind,
                FileKind::Document | FileKind::Archive | FileKind::Unknown
            ),
            Page::History => false,
        }
    }
}

/// Audio bitrate choices offered in settings (kbps labels + values).
const AUDIO_BITRATES: &[u32] = &[128, 192, 320];

fn audio_bitrate_labels() -> Vec<String> {
    AUDIO_BITRATES.iter().map(|b| format!("{b} kbps")).collect()
}

fn audio_bitrate_index(bitrate: u32) -> Option<usize> {
    AUDIO_BITRATES.iter().position(|b| *b == bitrate)
}

/// The context page to display in the context drawer.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ContextPage {
    #[default]
    About,
    Settings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuAction {
    About,
    Settings,
    OpenFiles,
    SelectAll,
    DeselectAll,
    RetryFailed,
}

impl menu::action::MenuAction for MenuAction {
    type Message = Message;

    fn message(&self) -> Self::Message {
        match self {
            MenuAction::About => Message::ToggleContextPage(ContextPage::About),
            MenuAction::Settings => Message::ToggleContextPage(ContextPage::Settings),
            MenuAction::OpenFiles => Message::OpenFiles,
            MenuAction::SelectAll => Message::SelectAllFiles,
            MenuAction::DeselectAll => Message::DeselectAllFiles,
            MenuAction::RetryFailed => Message::RetryFailed,
        }
    }
}
