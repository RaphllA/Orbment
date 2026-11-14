use crate::theme::{self, IconKind};
use egui::{Context, Key};
use orbment_core::{
    config::{ClipboardConfig, ClipboardDeletePolicy},
    i18n::tr,
};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use windows::Win32::Foundation::HWND;

#[derive(Clone, Debug)]
pub struct ClipboardRow {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub summary: Option<String>,
    pub note: Option<String>,
    pub is_sensitive: bool,
    pub is_favorite: bool,
    pub is_pinned: bool,
    pub image_path: Option<PathBuf>,
    /// Display name of the application the item was copied from.
    pub source: Option<String>,
    /// Payload size in bytes as captured.
    pub size: Option<u64>,
    /// RFC 3339 UTC capture time.
    pub created_at: Option<String>,
}
#[derive(Clone, Debug)]
pub struct ClipboardGroup {
    pub id: String,
    pub name: String,
    pub is_hidden: bool,
}
#[derive(Clone, Debug, Default)]
pub struct PopupQuery {
    pub text: String,
    pub category: String,
    pub group_id: Option<String>,
    pub offset: usize,
    pub limit: usize,
}
#[derive(Clone, Debug)]
pub enum ClipboardPopupEvent {
    Query {
        generation: u64,
        query: PopupQuery,
    },
    Action {
        id: String,
        action: String,
        value: Option<String>,
        confirmed: bool,
    },
    Paste {
        id: String,
        plain: bool,
        as_path: bool,
        target: HWND,
    },
    Hidden,
    Reclaimed,
    DeviceLost,
    Error(String),
}

pub(crate) enum PopupCommand {
    Event(ClipboardPopupEvent),
    Hide,
    Paste {
        id: String,
        plain: bool,
        as_path: bool,
    },
    Fixed(bool),
    Preview(bool),
    FileDrag(Vec<PathBuf>),
    Drag,
}

const PAGE: usize = 128;
static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);
type ImageKey = (PathBuf, bool);
type DecodedImage = (ImageKey, Result<egui::ColorImage, String>);
struct Images {
    requests: mpsc::SyncSender<ImageKey>,
    results: mpsc::Receiver<DecodedImage>,
    pending: BTreeSet<ImageKey>,
    textures: BTreeMap<ImageKey, Option<egui::TextureHandle>>,
    _context: Arc<Context>,
}
impl Images {
    fn new(context: Context) -> anyhow::Result<Self> {
        let (requests, receiver) = mpsc::sync_channel::<ImageKey>(16);
        let (sender, results) = mpsc::sync_channel(2);
        let context = Arc::new(context);
        let weak_context = Arc::downgrade(&context);
        let _worker = std::thread::Builder::new()
            .name("orbment-popup-images".into())
            .spawn(move || {
                while let Ok(key) = receiver.recv() {
                    if weak_context.strong_count() == 0 {
                        break;
                    }
                    let result = (|| -> anyhow::Result<egui::ColorImage> {
                        let mut reader = image::ImageReader::open(&key.0)?;
                        let mut limits = image::Limits::default();
                        limits.max_alloc = Some(128 * 1024 * 1024);
                        reader.limits(limits);
                        let image = reader.decode()?;
                        let edge = if key.1 { 1600 } else { 512 };
                        let image = image.thumbnail(edge, edge).to_rgba8();
                        Ok(egui::ColorImage::from_rgba_unmultiplied(
                            [image.width() as usize, image.height() as usize],
                            image.as_raw(),
                        ))
                    })()
                    .map_err(|error| error.to_string());
                    if sender.send((key, result)).is_err() {
                        break;
                    }
                    if let Some(context) = weak_context.upgrade() {
                        context.request_repaint();
                    }
                }
            })?;
        Ok(Self {
            requests,
            results,
            pending: BTreeSet::new(),
            textures: BTreeMap::new(),
            _context: context,
        })
    }
    fn texture(
        &mut self,
        context: &Context,
        path: &std::path::Path,
        preview: bool,
    ) -> Option<egui::TextureHandle> {
        while let Ok((key, result)) = self.results.try_recv() {
            self.pending.remove(&key);
            let texture = result.ok().map(|image| {
                context.load_texture("clipboard-image", image, egui::TextureOptions::LINEAR)
            });
            // Bounded CPU/GPU cache, released with the popup's view tree.
            if key.1 {
                self.textures.retain(|(_, full), _| !full);
            }
            if self.textures.len() >= 48 {
                self.textures.pop_first();
            }
            self.textures.insert(key, texture);
        }
        let key = (path.to_owned(), preview);
        if let Some(texture) = self.textures.get(&key) {
            return texture.clone();
        }
        if !self.pending.contains(&key) && self.requests.try_send(key.clone()).is_ok() {
            self.pending.insert(key);
        }
        None
    }
}

pub(crate) struct ClipboardView {
    pub config: ClipboardConfig,
    query: PopupQuery,
    generation: u64,
    total: usize,
    rows: BTreeMap<usize, Rc<ClipboardRow>>,
    row_heights: BTreeMap<usize, f32>,
    layout_key: Option<[u32; 7]>,
    pending: BTreeSet<usize>,
    loaded_pages: BTreeSet<usize>,
    visible_range: std::ops::Range<usize>,
    groups: Vec<ClipboardGroup>,
    selected: usize,
    scroll_selection: bool,
    focus_search: bool,
    reset_scroll: bool,
    preview: Option<Rc<ClipboardRow>>,
    reveal_sensitive: bool,
    copied_feedback: Option<(String, Instant)>,
    fixed: bool,
    confirm_delete: Option<Rc<ClipboardRow>>,
    note_editor: Option<(String, String)>,
    group_editor: Option<(String, String)>,
    manage_groups: bool,
    images: Option<Images>,
    commands: Vec<PopupCommand>,
    composing: bool,
}
impl ClipboardView {
    pub fn new(config: ClipboardConfig) -> Self {
        Self {
            config,
            query: PopupQuery {
                category: "all".into(),
                limit: PAGE,
                ..Default::default()
            },
            generation: 0,
            total: 0,
            rows: BTreeMap::new(),
            row_heights: BTreeMap::new(),
            layout_key: None,
            pending: BTreeSet::new(),
            loaded_pages: BTreeSet::new(),
            visible_range: 0..0,
            groups: Vec::new(),
            selected: 0,
            scroll_selection: false,
            focus_search: false,
            reset_scroll: true,
            preview: None,
            reveal_sensitive: false,
            copied_feedback: None,
            fixed: false,
            confirm_delete: None,
            note_editor: None,
            group_editor: None,
            manage_groups: false,
            images: None,
            commands: Vec::new(),
            composing: false,
        }
    }
    pub fn opened(&mut self, config: &ClipboardConfig) {
        self.config = config.clone();
        self.fixed = false;
        self.focus_search = config.search.default_focus;
        self.reset_scroll = config.window.scroll_to_top_on_open;
        if self.reset_scroll {
            self.selected = 0;
        }
        if config.window.select_category_on_open != "preserve" {
            self.query.category = match config.window.select_category_on_open.as_str() {
                "text" | "image" | "files" => config.window.select_category_on_open.clone(),
                _ => "all".into(),
            };
        }
        if config.window.select_range_on_open == "favorite" {
            self.query.category = "favorite".into();
        } else if config.window.select_range_on_open == "all" && self.query.category == "favorite" {
            self.query.category = "all".into();
        }
        if config.window.select_group_on_open != "preserve" {
            self.query.group_id = if config.window.select_group_on_open == "all"
                || config.window.select_group_on_open.is_empty()
            {
                None
            } else {
                Some(config.window.select_group_on_open.clone())
            };
        }
        self.refresh();
    }
    pub fn hidden(&mut self) {
        if self.config.search.clear_on_hide {
            self.query.text.clear();
        }
        self.preview = None;
        self.reveal_sensitive = false;
        self.confirm_delete = None;
        self.note_editor = None;
        self.group_editor = None;
        self.manage_groups = false;
        self.composing = false;
    }
    pub fn refresh(&mut self) {
        self.generation = NEXT_QUERY.fetch_add(1, Ordering::Relaxed);
        self.pending.clear();
        self.loaded_pages.clear();
        self.commands.retain(|command| {
            !matches!(
                command,
                PopupCommand::Event(ClipboardPopupEvent::Query { .. })
            )
        });
        if self.reset_scroll {
            self.visible_range = 0..0;
        }
        self.request_page(self.visible_range.start);
        if self.visible_range.end > self.visible_range.start {
            self.request_page(self.visible_range.end - 1);
        }
    }
    fn request_page(&mut self, index: usize) {
        let offset = index / PAGE * PAGE;
        if self.pending.insert(offset) {
            let mut query = self.query.clone();
            query.offset = offset;
            query.limit = PAGE;
            self.commands
                .push(PopupCommand::Event(ClipboardPopupEvent::Query {
                    generation: self.generation,
                    query,
                }));
        }
    }
    pub fn set_page(
        &mut self,
        generation: u64,
        total: usize,
        offset: usize,
        rows: Vec<ClipboardRow>,
    ) {
        if generation != self.generation {
            return;
        }
        self.pending.remove(&offset);
        self.loaded_pages.insert(offset);
        self.total = total;
        self.row_heights.retain(|index, _| *index < total);
        self.selected = self.selected.min(total.saturating_sub(1));
        let end = offset.saturating_add(PAGE).min(total);
        self.rows.retain(|index, _| {
            *index < total
                && !(*index >= offset && *index < end)
                && (self.visible_range.contains(index)
                    || index.abs_diff(self.visible_range.start) < PAGE * 3)
        });
        self.row_heights
            .retain(|index, _| *index < offset || *index >= end);
        for (index, row) in rows
            .into_iter()
            .take(end.saturating_sub(offset))
            .enumerate()
        {
            self.rows.insert(offset + index, Rc::new(row));
        }
    }
    pub fn set_groups(&mut self, groups: Vec<ClipboardGroup>) {
        self.groups = groups;
    }
    fn action(&mut self, row: &Rc<ClipboardRow>, action: &str, confirmed: bool) {
        let action = match action {
            "singleClickPaste" => "paste",
            "singleClickPastePlain" => "pastePlain",
            "singleClickCopy" => "copy",
            "singleClickCopyPlain" => "copyPlain",
            other => other,
        };
        match action {
            "paste" | "pastePlain" | "pastePath" => self.commands.push(PopupCommand::Paste {
                id: row.id.clone(),
                plain: action == "pastePlain" || self.config.content.paste_plain,
                as_path: action == "pastePath" || self.config.content.paste_files_as_path,
            }),
            "note" => {
                self.note_editor = Some((row.id.clone(), row.note.clone().unwrap_or_default()))
            }
            "delete" => {
                match self.config.content.delete_policy(
                    row.is_favorite,
                    row.is_pinned,
                    self.query.category == "favorite",
                ) {
                    ClipboardDeletePolicy::Protected => return,
                    ClipboardDeletePolicy::ConfirmationRequired if !confirmed => {
                        self.confirm_delete = Some(row.clone());
                        return;
                    }
                    _ => {}
                }
                self.emit_action(&row.id, action, None, confirmed);
            }
            "copy" | "copyPlain" => {
                self.copied_feedback = Some((row.id.clone(), Instant::now()));
                let action = if action == "copyPlain" || self.config.content.copy_plain {
                    "copyPlain"
                } else {
                    "copy"
                };
                self.emit_action(&row.id, action, None, confirmed);
                // The adapter hides only after a successful copy, using copy_then_hide_window.
            }
            _ => self.emit_action(&row.id, action, None, confirmed),
        }
    }
    fn emit_action(&mut self, id: &str, action: &str, value: Option<String>, confirmed: bool) {
        self.commands
            .push(PopupCommand::Event(ClipboardPopupEvent::Action {
                id: id.into(),
                action: action.into(),
                value,
                confirmed,
            }));
    }
    fn open_preview(&mut self, row: Rc<ClipboardRow>) {
        self.reveal_sensitive = false;
        if self.preview.is_none() {
            self.commands.push(PopupCommand::Preview(true));
        }
        self.preview = Some(row);
    }
    /// Loads a fixed page without the query round-trip, for rendered previews.
    pub fn with_rows(
        config: ClipboardConfig,
        rows: Vec<ClipboardRow>,
        groups: Vec<ClipboardGroup>,
        selected: usize,
    ) -> Self {
        let mut view = Self::new(config);
        view.refresh();
        view.commands.clear();
        view.set_groups(groups);
        let total = rows.len();
        view.set_page(view.generation, total, 0, rows);
        view.selected = selected;
        view
    }
    fn text_layout(&self, context: &Context, row: &ClipboardRow, width: f32) -> Arc<egui::Galley> {
        let lines = if row.kind == "files" {
            self.config.display.file_max_count
        } else {
            self.config.display.text_max_lines
        }
        .clamp(1, 12) as usize;
        let text = if row.kind == "files" {
            file_paths(&row.content)
                .into_iter()
                .take(lines)
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            row.summary
                .as_deref()
                .unwrap_or(&row.content)
                .chars()
                .take(4096)
                .collect()
        };
        let swatch_width = if row.kind == "text" && parse_color(&row.content).is_some() {
            50.0
        } else {
            0.0
        };
        let mut job = egui::text::LayoutJob::simple(
            text,
            egui::FontId::proportional(CARD_FONT),
            theme::TEXT,
            (width - swatch_width).max(40.0),
        );
        job.wrap.max_rows = lines;
        job.wrap.break_anywhere = true;
        context.fonts(|fonts| fonts.layout_job(job))
    }

    fn body_height(&self, context: &Context, row: &ClipboardRow, width: f32) -> f32 {
        if row.is_sensitive && self.config.sensitive.redact_secrets {
            return 22.0;
        }
        if row.kind == "image" && self.config.content.show_original_preview {
            return (self.config.display.image_max_height as f32).clamp(96.0, 110.0);
        }
        let line_height =
            context.fonts(|fonts| fonts.row_height(&egui::FontId::proportional(CARD_FONT)));
        let swatch_height = if row.kind == "text" && parse_color(&row.content).is_some() {
            22.0
        } else {
            0.0
        };
        self.text_layout(context, row, width)
            .size()
            .y
            .max(line_height)
            .max(swatch_height)
    }
    fn draw_header(&mut self, context: &Context) {
        egui::TopBottomPanel::top("clipboard-header")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::NAV_BG)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(context, |ui| {
                let drag_rect = egui::Rect::from_min_size(
                    ui.min_rect().min,
                    egui::vec2((ui.available_width() - 190.0).max(40.0), 30.0),
                );
                if ui
                    .interact(drag_rect, ui.id().with("header-drag"), egui::Sense::drag())
                    .drag_started()
                {
                    self.commands.push(PopupCommand::Drag);
                }
                ui.horizontal(|ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        theme::icon(ui, IconKind::Clipboard, theme::ACCENT_LIGHT);
                        ui.add(
                            egui::Label::new(theme::strong(ui, tr("clipboard.title"), 15.0))
                                .selectable(false),
                        );
                    });
                    theme::pill(
                        ui,
                        format!("{} {}", self.total, tr("clipboard.count_unit")),
                        theme::TEXT_MUTED,
                        theme::CARD,
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if theme::chip(ui, "×", false)
                            .on_hover_text(tr("clipboard.close"))
                            .clicked()
                        {
                            self.commands.push(PopupCommand::Hide);
                        }
                        if theme::icon_chip(
                            ui,
                            IconKind::Folder,
                            tr("clipboard.groups_short"),
                            self.manage_groups,
                        )
                        .on_hover_text(tr("clipboard.groups"))
                        .clicked()
                        {
                            self.manage_groups = !self.manage_groups;
                        }
                        if theme::icon_chip(
                            ui,
                            IconKind::Pin,
                            tr("clipboard.fixed_short"),
                            self.fixed,
                        )
                        .on_hover_text(tr("clipboard.fixed"))
                        .clicked()
                        {
                            self.fixed = !self.fixed;
                            self.commands.push(PopupCommand::Fixed(self.fixed));
                        }
                    });
                });
                ui.add_space(4.0);
                let search_id = egui::Id::new("clipboard-search");
                if self.focus_search {
                    ui.memory_mut(|memory| memory.request_focus(search_id));
                    self.focus_search = false;
                }
                let response = theme::edit_with_icon(
                    ui,
                    egui::TextEdit::singleline(&mut self.query.text)
                        .id(search_id)
                        .hint_text(
                            egui::RichText::new(tr("clipboard.search")).color(theme::TEXT_FAINT),
                        )
                        .margin(egui::Margin {
                            left: theme::ICON_GUTTER,
                            right: 10,
                            top: 7,
                            bottom: 7,
                        })
                        .desired_width(f32::INFINITY),
                    IconKind::Search,
                );
                if response.changed() {
                    self.selected = 0;
                    self.reset_scroll = true;
                    self.refresh();
                }
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                    let mut changed = false;
                    for category in ["all", "text", "image", "files", "favorite"] {
                        let label = tr(&format!("clipboard.category.{category}"));
                        let selected =
                            self.query.category == category && self.query.group_id.is_none();
                        let response = if category == "favorite" {
                            theme::icon_chip(ui, IconKind::Star, label, selected)
                        } else {
                            theme::chip(ui, label, selected)
                        };
                        if response.clicked() {
                            self.query.category = category.into();
                            self.query.group_id = None;
                            changed = true;
                        }
                    }
                    for group in self.groups.iter().filter(|group| !group.is_hidden) {
                        if theme::chip(
                            ui,
                            &group.name,
                            self.query.group_id.as_ref() == Some(&group.id),
                        )
                        .clicked()
                        {
                            self.query.group_id = Some(group.id.clone());
                            self.query.category = "all".into();
                            changed = true;
                        }
                    }
                    if changed {
                        self.selected = 0;
                        self.reset_scroll = true;
                        self.refresh();
                    }
                });
            });
    }
    fn draw_preview(&mut self, context: &Context, row: Rc<ClipboardRow>) {
        egui::SidePanel::right("clipboard-preview")
            .default_width(320.0)
            .min_width(140.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::NAV_BG)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::strong(ui, tr("clipboard.preview"), 15.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::chip(ui, "×", false).clicked() {
                            self.preview = None;
                            self.commands.push(PopupCommand::Preview(false));
                        }
                    });
                });
                let redacted = row.is_sensitive
                    && self.config.sensitive.redact_secrets
                    && !self.reveal_sensitive;
                if redacted {
                    theme::callout(ui, theme::Tone::Warning, tr("clipboard.sensitive"));
                    if ui
                        .add(theme::secondary_button(tr("clipboard.reveal_sensitive")))
                        .clicked()
                    {
                        self.reveal_sensitive = true;
                    }
                } else {
                    egui::ScrollArea::both()
                        .id_salt("clipboard-preview-scroll")
                        .show(ui, |ui| {
                            if row.kind == "image" {
                                self.image(ui, &row, true, 1200.0);
                            } else if row.kind == "files" {
                                for path in file_paths(&row.content) {
                                    ui.label(theme::muted(path));
                                }
                            } else {
                                ui.add(egui::Label::new(&row.content).wrap().selectable(true));
                            }
                            if let Some(note) = &row.note {
                                ui.separator();
                                ui.label(theme::muted(note.as_str()));
                            }
                        });
                }
            });
    }
    pub fn draw(&mut self, context: &Context) -> Vec<PopupCommand> {
        if let Some((_, since)) = &self.copied_feedback {
            if since.elapsed() >= Duration::from_secs(1) {
                self.copied_feedback = None;
            } else {
                context
                    .request_repaint_after(Duration::from_secs(1).saturating_sub(since.elapsed()));
            }
        }
        self.keyboard(context);
        self.draw_header(context);
        // The preview remains in the same native surface so it cannot steal activation.
        if let Some(row) = self.preview.clone() {
            self.draw_preview(context, row);
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show(context, |ui| {
                if self.total == 0 {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(theme::faint(tr(if self.pending.is_empty() {
                            "clipboard.empty"
                        } else {
                            "clipboard.loading"
                        })));
                    });
                }
                let overhead = CARD_PADDING * 2.0 + CARD_HEADER + CARD_BODY_GAP + CARD_GAP;
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt("clipboard-list")
                    .auto_shrink([false, false]);
                if self.reset_scroll {
                    scroll = scroll.vertical_scroll_offset(0.0);
                    self.reset_scroll = false;
                }
                scroll.show_viewport(ui, |ui, viewport| {
                    let width = ui.available_width();
                    let line_height = context
                        .fonts(|fonts| fonts.row_height(&egui::FontId::proportional(CARD_FONT)));
                    let layout_key = [
                        width.to_bits(),
                        line_height.to_bits(),
                        self.config.display.text_max_lines,
                        self.config.display.file_max_count,
                        self.config.display.image_max_height,
                        u32::from(self.config.content.show_original_preview),
                        u32::from(self.config.sensitive.redact_secrets),
                    ];
                    if self.layout_key != Some(layout_key) {
                        self.row_heights.clear();
                        self.layout_key = Some(layout_key);
                    }
                    for (&index, row) in &self.rows {
                        if !self.row_heights.contains_key(&index) {
                            let height = overhead
                                + self.body_height(context, row, width - CARD_PADDING * 2.0);
                            self.row_heights.insert(index, height);
                        }
                    }
                    let estimated = overhead
                        + line_height * self.config.display.text_max_lines.clamp(1, 12) as f32;
                    let geometry = RowGeometry::new(self.total, estimated, &self.row_heights);
                    ui.set_height(geometry.offset(self.total));
                    let origin = ui.max_rect().min;
                    if self.scroll_selection && self.total > 0 {
                        let selection = egui::Rect::from_min_max(
                            origin + egui::vec2(0.0, geometry.offset(self.selected)),
                            origin + egui::vec2(width, geometry.offset(self.selected + 1)),
                        );
                        ui.scroll_to_rect(selection, Some(egui::Align::Center));
                        self.scroll_selection = false;
                    }
                    let start = geometry.row_at(viewport.min.y);
                    let end = geometry
                        .row_at(viewport.max.y)
                        .saturating_add(1)
                        .min(self.total);
                    self.visible_range = start..end;
                    let rect = egui::Rect::from_min_max(
                        origin + egui::vec2(0.0, geometry.offset(start)),
                        origin + egui::vec2(width, geometry.offset(end)),
                    );
                    let mut visible = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                    visible.skip_ahead_auto_ids(start);
                    for index in start..end {
                        if !self.loaded_pages.contains(&(index / PAGE * PAGE)) {
                            self.request_page(index);
                        }
                        let row_height = geometry.offset(index + 1) - geometry.offset(index);
                        let Some(row) = self.rows.get(&index).cloned() else {
                            self.request_page(index);
                            visible.allocate_ui(egui::vec2(width, row_height), |ui| {
                                ui.label(theme::faint(tr("clipboard.loading")));
                            });
                            continue;
                        };
                        visible.push_id(&row.id, |ui| {
                            self.card(ui, index, &row, row_height, row_height - overhead);
                        });
                    }
                });
            });
        self.dialogs(context);
        std::mem::take(&mut self.commands)
    }
    /// Fixed geometry keeps quick actions and feedback from shifting neighbouring content.
    fn card(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        row: &Rc<ClipboardRow>,
        row_height: f32,
        body_height: f32,
    ) {
        let (outer, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            egui::Sense::click_and_drag(),
        );
        let rect = outer.shrink2(egui::vec2(0.0, CARD_GAP * 0.5));
        let is_hovered = ui.rect_contains_pointer(rect);
        let is_selected = index == self.selected;
        let (fill, stroke) = if is_selected {
            (
                theme::CARD_SELECTED,
                theme::stroke(1.0, theme::CONTROL_HOVER_BORDER),
            )
        } else if is_hovered {
            (
                theme::CARD_HOVER,
                theme::stroke(1.0, theme::CARD_HOVER_BORDER),
            )
        } else {
            (theme::CARD, theme::stroke(1.0, theme::CARD_BORDER))
        };
        ui.painter()
            .rect(rect, 8, fill, stroke, egui::StrokeKind::Inside);
        let inner = rect.shrink(CARD_PADDING);
        let mut card = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(inner)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        card.set_clip_rect(rect.intersect(ui.clip_rect()));
        card.spacing_mut().item_spacing = egui::vec2(8.0, 0.0);
        let color = (row.kind == "text")
            .then(|| parse_color(&row.content))
            .flatten();
        let actions_rect = egui::Rect::from_min_size(
            egui::pos2(inner.right() - 108.0, inner.top()),
            egui::vec2(108.0, 24.0),
        );
        let mut actions = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(actions_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        actions.spacing_mut().item_spacing.x = 4.0;
        let copied = self
            .copied_feedback
            .as_ref()
            .is_some_and(|(id, since)| id == &row.id && since.elapsed() < Duration::from_secs(1));
        for (action, icon, active, tint) in [
            (
                "copy",
                if copied {
                    IconKind::Check
                } else {
                    IconKind::Copy
                },
                copied,
                theme::SUCCESS,
            ),
            ("star", IconKind::Star, row.is_favorite, theme::WARNING),
            ("pinItem", IconKind::Pin, row.is_pinned, theme::ACCENT_LIGHT),
            ("delete", IconKind::Trash, false, theme::DANGER),
        ] {
            let response = theme::icon_button(
                &mut actions,
                icon,
                if active { tint } else { theme::TEXT_FAINT },
                active,
            )
            .on_hover_text(action_label(action, row));
            if response.hovered() && action == "delete" {
                theme::draw_icon(
                    actions.painter(),
                    response.rect.shrink(4.0),
                    icon,
                    theme::DANGER,
                );
            }
            if response.clicked() {
                self.action(row, action, false);
                ui.ctx().request_repaint();
            }
        }
        let meta_rect = egui::Rect::from_min_max(
            inner.min,
            egui::pos2(actions_rect.left() - 6.0, inner.top() + CARD_HEADER),
        );
        let mut meta = card.new_child(
            egui::UiBuilder::new()
                .max_rect(meta_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        meta.set_clip_rect(meta_rect.intersect(ui.clip_rect()));
        kind_badge(&mut meta, &row.kind, color);
        let mut details = Vec::new();
        if let Some(source) = &row.source {
            details.push(source.clone());
        }
        if let Some(size) = metadata(row) {
            details.push(size);
        }
        if let Some(time) = row.created_at.as_deref().and_then(local_time_label) {
            details.push(time);
        }
        meta.add(
            egui::Label::new(
                egui::RichText::new(details.join(" · "))
                    .size(12.5)
                    .color(theme::TEXT_MUTED),
            )
            .truncate(),
        );
        card.allocate_space(egui::vec2(inner.width(), CARD_HEADER));
        card.add_space(CARD_BODY_GAP);
        let redacted = row.is_sensitive && self.config.sensitive.redact_secrets;
        if redacted {
            card.horizontal(|ui| {
                theme::icon(ui, IconKind::Lock, theme::TEXT_MUTED);
                ui.label(theme::muted(tr("clipboard.sensitive")));
            });
        } else if row.kind == "image" && self.config.content.show_original_preview {
            let max_height = (self.config.display.image_max_height as f32)
                .clamp(96.0, 110.0)
                .min(body_height);
            self.image(&mut card, row, false, max_height);
        } else {
            let swatch_size = egui::vec2(40.0, 22.0);
            let text_left = inner.left() + color.map_or(0.0, |_| swatch_size.x + 10.0);
            let galley = self.text_layout(card.ctx(), row, inner.width());
            // The card reserves exactly its measured content height, with only the shared padding.
            let content_height = galley.size().y.max(color.map_or(0.0, |_| swatch_size.y));
            let top = card.cursor().top() + ((body_height - content_height) * 0.5).max(0.0);
            let middle = top + content_height * 0.5;
            if let Some(color) = color {
                card.painter().rect(
                    egui::Rect::from_min_size(
                        egui::pos2(inner.left(), middle - swatch_size.y * 0.5),
                        swatch_size,
                    ),
                    5,
                    color,
                    theme::stroke(1.0, theme::SWATCH_BORDER),
                    egui::StrokeKind::Inside,
                );
            }
            let text_top = middle - galley.size().y * 0.5;
            card.painter()
                .galley(egui::pos2(text_left, text_top), galley, theme::TEXT);
        }
        if response.drag_started() && !redacted {
            let paths = if let Some(path) = &row.image_path {
                vec![path.clone()]
            } else if row.kind == "files" {
                file_paths(&row.content)
                    .into_iter()
                    .map(PathBuf::from)
                    .collect()
            } else {
                Vec::new()
            };
            if !paths.is_empty() {
                self.commands.push(PopupCommand::FileDrag(paths));
            }
        }
        if response.clicked() {
            self.selected = index;
            match self.config.content.auto_paste.as_str() {
                "singleClickPaste" => self.action(row, "paste", false),
                "singleClickCopy" => self.action(row, "copy", false),
                _ => {}
            }
        }
        if response.double_clicked() && self.config.content.auto_paste == "doubleClickPaste" {
            self.action(row, "paste", false);
        }
        if response.double_clicked() && self.config.content.auto_paste == "doubleClickCopy" {
            self.action(row, "copy", false);
        }
        if response.clicked_by(egui::PointerButton::Middle) {
            let action = self.config.content.middle_click.clone();
            if action != "disabled" {
                self.action(row, &action, false);
            }
        }
        response.context_menu(|ui| {
            for (action, caption) in context_actions(row) {
                if ui.button(caption).clicked() {
                    if action == "preview" {
                        self.open_preview(row.clone());
                    } else {
                        self.action(row, action, false);
                    }
                    ui.close_menu();
                }
            }
            ui.menu_button(tr("clipboard.move_group"), |ui| {
                if ui.button(tr("clipboard.category.all")).clicked() {
                    self.emit_action(&row.id, "moveGroup", None, false);
                    ui.close_menu();
                }
                for group in self.groups.clone() {
                    if ui.button(&group.name).clicked() {
                        self.emit_action(&row.id, "moveGroup", Some(group.id), false);
                        ui.close_menu();
                    }
                }
            });
        });
    }
    fn keyboard(&mut self, context: &Context) {
        let editing = context.wants_keyboard_input();
        let events = context.input(|input| input.events.clone());
        let mut ime_commit = false;
        for event in &events {
            if let egui::Event::Ime(event) = event {
                match event {
                    egui::ImeEvent::Enabled | egui::ImeEvent::Preedit(_) => self.composing = true,
                    egui::ImeEvent::Commit(_) => {
                        self.composing = false;
                        ime_commit = true;
                    }
                    egui::ImeEvent::Disabled => self.composing = false,
                }
            }
        }
        let modal = self.note_editor.is_some()
            || self.group_editor.is_some()
            || self.confirm_delete.is_some()
            || self.manage_groups;
        if !editing
            && !modal
            && events.iter().any(|event| {
                matches!(event,
                    egui::Event::Text(text) if !text.trim().is_empty()
                ) || matches!(
                    event,
                    egui::Event::Ime(
                        egui::ImeEvent::Enabled
                            | egui::ImeEvent::Preedit(_)
                            | egui::ImeEvent::Commit(_)
                    )
                )
            })
        {
            self.focus_search = true;
        }
        if self.composing || ime_commit {
            return;
        }
        if context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            if modal {
                self.note_editor = None;
                self.group_editor = None;
                self.confirm_delete = None;
                self.manage_groups = false;
            } else if self.preview.take().is_none() {
                self.commands.push(PopupCommand::Hide);
            } else {
                self.commands.push(PopupCommand::Preview(false));
            }
            return;
        }
        if modal {
            return;
        }
        if context.input(|input| input.key_pressed(Key::ArrowDown)) {
            self.selected = self
                .selected
                .saturating_add(1)
                .min(self.total.saturating_sub(1));
            self.scroll_selection = true;
        }
        if context.input(|input| input.key_pressed(Key::ArrowUp)) {
            self.selected = self.selected.saturating_sub(1);
            self.scroll_selection = true;
        }
        if self.scroll_selection && !self.rows.contains_key(&self.selected) {
            self.request_page(self.selected);
        }
        if let Some(row) = self.rows.get(&self.selected).cloned() {
            if context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                self.action(&row, "paste", false);
            }
            if !editing
                && self.config.preview.space_enabled
                && context.input(|input| input.key_pressed(Key::Space))
            {
                self.open_preview(row.clone());
            }
            if !editing && context.input(|input| input.key_pressed(Key::Delete)) {
                self.action(&row, "delete", false);
            }
        }
    }
    fn image(&mut self, ui: &mut egui::Ui, row: &ClipboardRow, preview: bool, max_height: f32) {
        let Some(path) = &row.image_path else {
            ui.label(tr("clipboard.image_unavailable"));
            return;
        };
        if self.images.is_none() {
            match Images::new(ui.ctx().clone()) {
                Ok(images) => self.images = Some(images),
                Err(error) => {
                    ui.label(tr("clipboard.image_unavailable"));
                    self.commands
                        .push(PopupCommand::Event(ClipboardPopupEvent::Error(
                            error.to_string(),
                        )));
                    return;
                }
            }
        }
        if let Some(texture) = self
            .images
            .as_mut()
            .and_then(|images| images.texture(ui.ctx(), path, preview))
        {
            let size = texture.size_vec2();
            let factor = (ui.available_width() / size.x)
                .min(max_height.max(1.0) / size.y)
                .min(1.0);
            ui.add(
                egui::Image::new((texture.id(), size * factor)).corner_radius(if preview {
                    8
                } else {
                    6
                }),
            );
        } else {
            let failed = self.images.as_ref().is_some_and(|images| {
                matches!(images.textures.get(&(path.clone(), preview)), Some(None))
            });
            ui.label(theme::faint(tr(if failed {
                "clipboard.image_unavailable"
            } else {
                "clipboard.image_loading"
            })));
        }
    }
    fn dialogs(&mut self, context: &Context) {
        if let Some(row) = self.confirm_delete.clone() {
            egui::Window::new(tr("clipboard.delete_confirm"))
                .collapsible(false)
                .show(context, |ui| {
                    ui.label(tr("clipboard.delete_warning"));
                    if ui.button(tr("clipboard.confirm")).clicked() {
                        self.action(&row, "delete", true);
                        self.confirm_delete = None;
                    }
                    if ui.button(tr("clipboard.cancel")).clicked() {
                        self.confirm_delete = None;
                    }
                });
        }
        if let Some((id, mut note)) = self.note_editor.clone() {
            let mut close = false;
            egui::Window::new(tr("clipboard.action.note")).show(context, |ui| {
                ui.text_edit_multiline(&mut note);
                if ui.button(tr("clipboard.save")).clicked() {
                    self.emit_action(&id, "note", Some(note.clone()), false);
                    close = true;
                }
                if ui.button(tr("clipboard.cancel")).clicked() {
                    close = true;
                }
            });
            self.note_editor = if close { None } else { Some((id, note)) };
        }
        if self.manage_groups {
            egui::Window::new(tr("clipboard.groups")).show(context, |ui| {
                if ui.button(tr("clipboard.create_group")).clicked() {
                    self.group_editor = Some((String::new(), String::new()));
                }
                for group in self.groups.clone() {
                    ui.horizontal(|ui| {
                        ui.label(&group.name);
                        if ui.button(tr("clipboard.rename_group")).clicked() {
                            self.group_editor = Some((group.id.clone(), group.name.clone()));
                        }
                        if ui
                            .button(tr(if group.is_hidden {
                                "clipboard.show_group"
                            } else {
                                "clipboard.hide_group"
                            }))
                            .clicked()
                        {
                            self.emit_action(&group.id, "toggleGroup", None, false);
                        }
                        if ui.button(tr("clipboard.delete_group")).clicked() {
                            self.emit_action(&group.id, "deleteGroup", None, true);
                        }
                    });
                }
                if ui.button(tr("clipboard.close")).clicked() {
                    self.manage_groups = false;
                }
            });
        }
        if let Some((id, mut name)) = self.group_editor.clone() {
            let mut close = false;
            egui::Window::new(tr("clipboard.group_name")).show(context, |ui| {
                ui.text_edit_singleline(&mut name);
                if ui
                    .add_enabled(
                        !name.trim().is_empty(),
                        egui::Button::new(tr("clipboard.save")),
                    )
                    .clicked()
                {
                    self.emit_action(
                        &id,
                        if id.is_empty() {
                            "createGroup"
                        } else {
                            "renameGroup"
                        },
                        Some(name.trim().into()),
                        false,
                    );
                    close = true;
                }
                if ui.button(tr("clipboard.cancel")).clicked() {
                    close = true;
                }
            });
            self.group_editor = if close { None } else { Some((id, name)) };
        }
    }
}
fn file_paths(content: &str) -> Vec<String> {
    serde_json::from_str(content).unwrap_or_else(|_| vec![content.into()])
}

/// Sparse measured heights preserve virtual scrolling without allocating an entry for every DB row.
struct RowGeometry {
    total: usize,
    estimated: f32,
    corrections: Vec<(usize, f32)>,
}

impl RowGeometry {
    fn new(total: usize, estimated: f32, measured: &BTreeMap<usize, f32>) -> Self {
        let mut delta = 0.0;
        let corrections = measured
            .range(..total)
            .map(|(&index, &height)| {
                delta += height - estimated;
                (index, delta)
            })
            .collect();
        Self {
            total,
            estimated,
            corrections,
        }
    }

    fn offset(&self, index: usize) -> f32 {
        let index = index.min(self.total);
        let count = self.corrections.partition_point(|(row, _)| *row < index);
        index as f32 * self.estimated
            + count
                .checked_sub(1)
                .map_or(0.0, |last| self.corrections[last].1)
    }

    fn row_at(&self, y: f32) -> usize {
        let (mut low, mut high) = (0, self.total);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.offset(middle + 1) <= y {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }
}

const CARD_PADDING: f32 = 7.0;
const CARD_HEADER: f32 = 26.0;
const CARD_BODY_GAP: f32 = 4.0;
const CARD_GAP: f32 = 6.0;
const CARD_FONT: f32 = 14.5;

fn kind_badge(ui: &mut egui::Ui, kind: &str, color: Option<egui::Color32>) {
    let (text, fg, bg) = match (kind, color) {
        (_, Some(_)) => (tr("clipboard.kind_color"), theme::TEXT, theme::TOOL_CHIP),
        ("html", _) => ("HTML".to_owned(), theme::PURPLE, theme::PURPLE_BG),
        ("rtf", _) => ("RTF".to_owned(), theme::PURPLE, theme::PURPLE_BG),
        ("image", _) => (
            tr("clipboard.category.image"),
            theme::GREEN,
            theme::GREEN_BG,
        ),
        ("files", _) => (
            tr("clipboard.category.files"),
            theme::ORANGE,
            theme::ORANGE_BG,
        ),
        _ => (
            tr("clipboard.category.text"),
            theme::ACCENT_LIGHT,
            theme::ACCENT_SOFT,
        ),
    };
    theme::badge(ui, text, 12.5, fg, bg, egui::vec2(6.0, 1.0), 5);
}

fn metadata(row: &ClipboardRow) -> Option<String> {
    match row.kind.as_str() {
        "files" => Some(format!(
            "{} {}",
            file_paths(&row.content).len(),
            tr("clipboard.file_unit")
        )),
        "image" => row.size.map(crate::treemap::bytes),
        _ => Some(format!(
            "{} {}",
            row.content.chars().count(),
            tr("clipboard.char_unit")
        )),
    }
}

fn action_label(action: &str, row: &ClipboardRow) -> String {
    match action {
        "star" if row.is_favorite => tr("clipboard.unstar"),
        "star" => tr("clipboard.star"),
        "pinItem" if row.is_pinned => tr("clipboard.unpin"),
        "pinItem" => tr("clipboard.pin"),
        _ => tr(&format!("clipboard.action.{action}")),
    }
}

/// Recognises `#RGB`, `#RRGGBB`, `#RRGGBBAA` and `rgb(r, g, b)` colour values.
fn parse_color(text: &str) -> Option<egui::Color32> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        let digits = |range: std::ops::Range<usize>| u8::from_str_radix(hex.get(range)?, 16).ok();
        return match hex.len() {
            3 => {
                let channel = |index: usize| digits(index..index + 1).map(|value| value * 17);
                Some(egui::Color32::from_rgb(
                    channel(0)?,
                    channel(1)?,
                    channel(2)?,
                ))
            }
            6 | 8 => Some(egui::Color32::from_rgb(
                digits(0..2)?,
                digits(2..4)?,
                digits(4..6)?,
            )),
            _ => None,
        };
    }
    let inner = text
        .strip_prefix("rgb(")
        .or_else(|| text.strip_prefix("rgba("))?
        .strip_suffix(')')?;
    let mut channels = inner.split(',').map(|part| part.trim().parse::<u8>().ok());
    Some(egui::Color32::from_rgb(
        channels.next()??,
        channels.next()??,
        channels.next()??,
    ))
}

/// Converts an RFC 3339 UTC timestamp to local "HH:MM", or "MM-DD HH:MM" for other days.
fn local_time_label(created_at: &str) -> Option<String> {
    use windows::Win32::{
        Foundation::SYSTEMTIME,
        System::{SystemInformation::GetLocalTime, Time::SystemTimeToTzSpecificLocalTime},
    };
    let field = |range: std::ops::Range<usize>| created_at.get(range)?.parse::<u16>().ok();
    let utc = SYSTEMTIME {
        wYear: field(0..4)?,
        wMonth: field(5..7)?,
        wDay: field(8..10)?,
        wHour: field(11..13)?,
        wMinute: field(14..16)?,
        wSecond: field(17..19)?,
        ..Default::default()
    };
    let mut local = SYSTEMTIME::default();
    unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.ok()?;
    let now = unsafe { GetLocalTime() };
    Some(
        if (now.wYear, now.wMonth, now.wDay) == (local.wYear, local.wMonth, local.wDay) {
            format!("{:02}:{:02}", local.wHour, local.wMinute)
        } else {
            format!(
                "{:02}-{:02} {:02}:{:02}",
                local.wMonth, local.wDay, local.wHour, local.wMinute
            )
        },
    )
}

fn context_actions(row: &ClipboardRow) -> Vec<(&'static str, String)> {
    let mut actions = match row.kind.as_str() {
        "image" => vec![("preview", "查看大图"), ("reveal", "打开图片所在文件夹")],
        "files" => vec![
            ("reveal", "打开文件所在文件夹"),
            ("copyPlain", "复制文件路径"),
        ],
        "text" | "html" | "rtf" => vec![("preview", "查看完整内容")],
        _ => Vec::new(),
    };
    if matches!(row.kind.as_str(), "text" | "html" | "rtf")
        && (row.content.trim_start().starts_with("https://")
            || row.content.trim_start().starts_with("http://"))
    {
        actions.push(("openLink", "在浏览器中打开链接"));
    }
    if matches!(row.kind.as_str(), "html" | "rtf") {
        actions.push(("copyPlain", "仅复制无格式文本"));
    }
    actions.push(("note", "编辑备注"));
    actions
        .into_iter()
        .map(|(key, label)| (key, label.into()))
        .collect()
}

fn normalize_shell_path(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let native = canonical.to_string_lossy().replace('/', "\\");
    if let Some(unc) = native.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else {
        PathBuf::from(native.strip_prefix(r"\\?\").unwrap_or(&native))
    }
}

/// The caller owns the OLE apartment; the returned data object owns its PIDL copies.
fn shell_file_data_object(
    paths: &[PathBuf],
) -> anyhow::Result<windows::Win32::System::Com::IDataObject> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::UI::Shell::{
            BHID_DataObject, Common::ITEMIDLIST, ILCreateFromPathW, ILFree, IShellItemArray,
            SHCreateShellItemArrayFromIDLists,
        },
        core::PCWSTR,
    };
    struct Pidls(Vec<*const ITEMIDLIST>);
    impl Drop for Pidls {
        fn drop(&mut self) {
            for &pidl in &self.0 {
                unsafe { ILFree(Some(pidl)) };
            }
        }
    }
    anyhow::ensure!(!paths.is_empty(), "没有可拖出的文件");
    unsafe {
        let mut pidls = Pidls(Vec::with_capacity(paths.len()));
        for path in paths {
            anyhow::ensure!(path.exists(), "文件已不存在：{}", path.display());
            let path = normalize_shell_path(path);
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let pidl = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
            anyhow::ensure!(!pidl.is_null(), "无法创建文件拖放对象：{}", path.display());
            pidls.0.push(pidl);
        }
        let items: IShellItemArray = SHCreateShellItemArrayFromIDLists(&pidls.0)?;
        Ok(items.BindToHandler(None, &BHID_DataObject)?)
    }
}

/// Shell data objects supply CF_HDROP and the other formats expected by Explorer and upload targets.
pub(crate) fn start_file_drag(paths: &[PathBuf]) -> anyhow::Result<()> {
    use windows::Win32::{
        System::Ole::{
            DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, OleInitialize, OleUninitialize,
        },
        UI::{Input::KeyboardAndMouse::ReleaseCapture, Shell::SHDoDragDrop},
    };
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { OleUninitialize() };
        }
    }
    unsafe {
        OleInitialize(None)?;
        let _apartment = Apartment;
        let data = shell_file_data_object(paths)?;
        let _ = ReleaseCapture();
        SHDoDragDrop(
            None,
            &data,
            None,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, kind: &str) -> ClipboardRow {
        ClipboardRow {
            id: id.into(),
            kind: kind.into(),
            content: "example".into(),
            summary: None,
            note: None,
            is_sensitive: false,
            is_favorite: false,
            is_pinned: false,
            image_path: None,
            source: None,
            size: None,
            created_at: None,
        }
    }

    fn draw_frame(view: &mut ClipboardView, context: &Context, width: f32) {
        drop(context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 1600.0),
                )),
                ..Default::default()
            },
            |context| drop(view.draw(context)),
        ));
    }

    #[test]
    fn cached_row_heights_follow_width_and_display_settings() {
        let context = Context::default();
        let mut config = ClipboardConfig::default();
        config.display.text_max_lines = 12;
        config.display.file_max_count = 4;
        config.display.image_max_height = 110;
        config.content.show_original_preview = true;
        config.sensitive.redact_secrets = false;
        let mut text = row("text", "text");
        text.content = "word ".repeat(40);
        let mut files = row("files", "files");
        files.content = r#"["first","second","third","fourth"]"#.into();
        let mut secret = row("secret", "text");
        secret.content = "first\nsecond\nthird\nfourth".into();
        secret.is_sensitive = true;
        let mut view = ClipboardView::with_rows(
            config,
            vec![text, files, row("image", "image"), secret],
            vec![],
            0,
        );
        draw_frame(&mut view, &context, 620.0);
        let wide = view.row_heights.clone();
        draw_frame(&mut view, &context, 620.0);
        assert_eq!(view.row_heights, wide);

        draw_frame(&mut view, &context, 260.0);
        let narrow_text = view.row_heights[&0];
        assert!(narrow_text > wide[&0]);
        view.config.sensitive.redact_secrets = true;
        draw_frame(&mut view, &context, 260.0);
        assert!(view.row_heights[&3] < wide[&3]);

        view.config.display.text_max_lines = 1;
        draw_frame(&mut view, &context, 260.0);
        assert!(view.row_heights[&0] < narrow_text);
        view.config.display.file_max_count = 1;
        draw_frame(&mut view, &context, 260.0);
        assert!(view.row_heights[&1] < wide[&1]);

        view.config.display.image_max_height = 96;
        draw_frame(&mut view, &context, 260.0);
        let smaller_image = view.row_heights[&2];
        assert!(smaller_image < wide[&2]);
        view.config.content.show_original_preview = false;
        draw_frame(&mut view, &context, 260.0);
        assert!(view.row_heights[&2] < smaller_image);
    }

    #[test]
    fn refreshed_page_remeasures_changed_content_and_discards_removed_heights() {
        let context = Context::default();
        let mut config = ClipboardConfig::default();
        config.display.text_max_lines = 12;
        let mut view = ClipboardView::with_rows(
            config,
            vec![row("old", "text"), row("removed", "text")],
            vec![],
            0,
        );
        draw_frame(&mut view, &context, 620.0);
        let original = view.row_heights.clone();
        let previous = view.generation;
        view.refresh();
        view.set_page(previous, 0, 0, vec![]);
        assert_eq!(view.row_heights, original);

        let mut changed = row("changed", "text");
        changed.content = "first\nsecond\nthird".into();
        view.set_page(view.generation, 1, 0, vec![changed]);
        assert!(view.row_heights.is_empty());
        draw_frame(&mut view, &context, 620.0);
        assert!(view.row_heights[&0] > original[&0]);
        assert_eq!(view.row_heights.len(), 1);
    }

    #[test]
    fn refreshing_later_pages_preserves_visible_rows_and_geometry() {
        let mut view = ClipboardView::new(ClipboardConfig::default());
        view.reset_scroll = false;
        view.visible_range = PAGE * 4..PAGE * 4 + 8;
        view.set_page(0, PAGE * 8, PAGE * 4, vec![row("visible", "text")]);
        view.row_heights.insert(PAGE * 4, 93.0);
        view.refresh();
        assert!(view.commands.iter().any(|command| matches!(
            command,
            PopupCommand::Event(ClipboardPopupEvent::Query { query, .. })
                if query.offset == PAGE * 4
        )));
        view.set_page(view.generation, PAGE * 8, 0, vec![row("newest", "text")]);
        assert_eq!(view.rows[&(PAGE * 4)].id, "visible");
        assert_eq!(view.row_heights[&(PAGE * 4)], 93.0);
        view.set_page(
            view.generation,
            PAGE * 8,
            PAGE * 4,
            vec![row("updated", "text")],
        );
        assert_eq!(view.rows[&(PAGE * 4)].id, "updated");
        assert!(!view.row_heights.contains_key(&(PAGE * 4)));
    }

    #[test]
    fn refreshing_keeps_cards_until_current_generation_arrives() {
        let mut view = ClipboardView::with_rows(
            ClipboardConfig::default(),
            vec![row("old", "text"), row("removed", "text")],
            vec![],
            0,
        );
        let old_generation = view.generation;
        view.refresh();
        assert_eq!(view.rows[&0].id, "old");
        assert_eq!(view.total, 2);
        view.set_page(old_generation, 0, 0, vec![]);
        assert_eq!(view.rows.len(), 2);
        view.set_page(view.generation, 1, 0, vec![row("new", "text")]);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[&0].id, "new");
    }

    #[test]
    fn copy_feedback_is_local_and_does_not_refresh_or_remove_cards() {
        let mut view = ClipboardView::with_rows(
            ClipboardConfig::default(),
            vec![row("first", "text"), row("second", "text")],
            vec![],
            0,
        );
        let generation = view.generation;
        view.action(&view.rows[&1].clone(), "copy", false);
        assert_eq!(view.copied_feedback.as_ref().unwrap().0, "second");
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.generation, generation);
        assert!(!view.commands.iter().any(|command| matches!(
            command,
            PopupCommand::Event(ClipboardPopupEvent::Query { .. })
        )));
    }

    #[test]
    fn short_cards_use_content_height_while_images_keep_large_previews() {
        let context = Context::default();
        let view = ClipboardView::new(ClipboardConfig::default());
        let _ = context.run(egui::RawInput::default(), |context| {
            let short = row("short", "text");
            let mut color = row("color", "text");
            color.content = "#3B82F6".into();
            let mut multiline = row("multiline", "text");
            multiline.content = "first\nsecond\nthird".into();
            let short_height = view.body_height(context, &short, 360.0);
            assert!(short_height < 40.0);
            assert!(view.body_height(context, &color, 360.0) < 40.0);
            assert!(view.body_height(context, &multiline, 360.0) > short_height);
            assert!(view.body_height(context, &row("image", "image"), 360.0) >= 96.0);
        });
    }

    #[test]
    fn variable_height_virtual_rows_keep_correct_offsets_and_hit_ranges() {
        let geometry = RowGeometry::new(
            10_000,
            60.0,
            &BTreeMap::from([(0, 70.0), (1, 140.0), (5000, 80.0)]),
        );
        assert_eq!(geometry.offset(0), 0.0);
        assert_eq!(geometry.offset(1), 70.0);
        assert_eq!(geometry.offset(2), 210.0);
        assert_eq!(geometry.row_at(69.0), 0);
        assert_eq!(geometry.row_at(70.0), 1);
        assert_eq!(geometry.row_at(209.0), 1);
        assert_eq!(geometry.row_at(210.0), 2);
        assert_eq!(geometry.offset(10_000), 600_110.0);
        assert_eq!(geometry.row_at(600_110.0), 10_000);
    }

    #[test]
    fn context_menu_only_exposes_actions_meaningful_for_the_payload() {
        let keys = |row: &ClipboardRow| {
            context_actions(row)
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&row("image", "image")), ["preview", "reveal", "note"]);
        assert_eq!(
            keys(&row("files", "files")),
            ["reveal", "copyPlain", "note"]
        );
        for kind in ["text", "html", "rtf"] {
            let mut item = row(kind, kind);
            let rich = kind != "text";
            assert_eq!(
                keys(&item),
                if rich {
                    vec!["preview", "copyPlain", "note"]
                } else {
                    vec!["preview", "note"]
                }
            );
            for url in ["https://example.com", "http://example.com"] {
                item.content = url.into();
                assert_eq!(
                    keys(&item),
                    if rich {
                        vec!["preview", "openLink", "copyPlain", "note"]
                    } else {
                        vec!["preview", "openLink", "note"]
                    }
                );
            }
            item.content = "file:///C:/example.txt".into();
            assert!(!keys(&item).contains(&"openLink"));
        }
    }

    #[test]
    fn mixed_separator_media_path_creates_shell_data_object() -> anyhow::Result<()> {
        use windows::Win32::{
            Foundation::S_OK,
            System::{
                Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL},
                Ole::{OleInitialize, OleUninitialize, ReleaseStgMedium},
            },
            UI::Shell::{DragQueryFileW, HDROP},
        };
        struct TempDirectory(PathBuf);
        impl Drop for TempDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe { OleUninitialize() };
            }
        }
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root = TempDirectory(std::env::temp_dir().join(format!(
            "orbment-shell-drag-{}-{unique}",
            std::process::id()
        )));
        std::fs::create_dir_all(root.0.join("media").join("sub"))?;
        let mixed = root.0.join("media/sub/test.png");
        std::fs::write(&mixed, b"test image")?;
        assert!(mixed.to_string_lossy().contains('/'));
        let normalized = normalize_shell_path(&mixed);
        assert!(!normalized.to_string_lossy().contains('/'));
        assert!(!normalized.to_string_lossy().starts_with(r"\\?\"));
        assert!(normalized.is_file());
        unsafe { OleInitialize(None)? };
        let _apartment = Apartment;
        let data = shell_file_data_object(&[mixed])?;
        let format = FORMATETC {
            cfFormat: 15, // CF_HDROP
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        unsafe {
            // S_FALSE is a successful HRESULT, but does not advertise CF_HDROP.
            assert_eq!(data.QueryGetData(&format), S_OK);
            let mut medium = data.GetData(&format)?;
            assert_eq!(medium.tymed, TYMED_HGLOBAL.0 as u32);
            let drop = HDROP(medium.u.hGlobal.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let length = DragQueryFileW(drop, 0, None);
            let mut path = vec![0u16; length as usize + 1];
            let copied = DragQueryFileW(drop, 0, Some(&mut path));
            ReleaseStgMedium(&mut medium);
            assert_eq!(count, 1);
            assert!(length > 0);
            assert_eq!(copied, length);
            assert_eq!(
                PathBuf::from(String::from_utf16(&path[..copied as usize])?),
                normalized
            );
        }
        Ok(())
    }
}
