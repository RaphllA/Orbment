use crate::{
    D2dContext,
    main_host::{CanvasPainter, SettingsEvent},
    theme::{self, IconKind, Tone},
    treemap::{NestedTreemap, TreemapEvent, bytes},
};
use egui::RichText;
use orbment_analyzer::{
    disk::{DiskEntry, DiskReport, ExtensionStat},
    file_search::SearchResult,
    file_unlock::FileLock,
    treemap::Node,
};
use orbment_core::{config::DiskAnalyzerConfig, i18n::tr};
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
pub enum FileToolsAction {
    Analyze { target: String, filter: String },
    Duplicates,
    QueryLocks { path: String },
    TerminateLock { lock: FileLock },
    CloseHandle { lock: FileLock },
    Open { path: String },
    Explorer { path: String },
    Terminal { path: String },
    Recycle { path: String },
    Search { query: String },
    Ports { port: u16 },
    DiskOpened,
    DiskClosed,
    SearchOpened,
    SearchClosed,
}

pub struct SharedDiskReport {
    pub target: String,
    pub treemap: Arc<Node>,
    pub top_files: Vec<DiskEntry>,
    pub extensions: Vec<ExtensionStat>,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub file_count: u64,
    pub directory_count: u64,
}

impl From<DiskReport> for SharedDiskReport {
    fn from(mut report: DiskReport) -> Self {
        for child in &mut report.treemap.children {
            if child.id.starts_with("orbment:free-space:") {
                child.label = tr("file_tools.free_space");
            }
        }
        Self {
            target: report.target,
            treemap: Arc::new(report.treemap),
            top_files: report.top_files,
            extensions: report.extensions,
            logical_size: report.logical_size,
            allocated_size: report.allocated_size,
            file_count: report.file_count,
            directory_count: report.directory_count,
        }
    }
}

#[derive(Default)]
pub struct FileToolsState {
    pub disk: Option<SharedDiskReport>,
    pub search_results: Vec<SearchResult>,
    pub search_query: Option<String>,
    pub locks: Vec<FileLock>,
    pub duplicates: Vec<Vec<String>>,
    pub busy: bool,
    pub error: Option<String>,
    pub revision: u64,
    pub disk_revision: u64,
}
pub type SharedFileToolsState = Arc<Mutex<FileToolsState>>;

fn emit(events: &mut Vec<SettingsEvent>, action: FileToolsAction) {
    events.push(SettingsEvent::FileTools(action));
}

pub(crate) struct DiskPanel {
    pub page: String,
    state: SharedFileToolsState,
    tree: NestedTreemap,
    revision: Option<u64>,
    target: String,
    last_target: String,
    selected_file: Option<String>,
    filter: String,
    lock_path: String,
    confirmation: Option<FileToolsAction>,
    config: DiskAnalyzerConfig,
}
impl DiskPanel {
    pub fn new(
        page: &str,
        state: SharedFileToolsState,
        config: DiskAnalyzerConfig,
        context: egui::Context,
    ) -> Self {
        let mut tree = NestedTreemap::new(context);
        tree.configure_disk(&config);
        Self {
            page: page.into(),
            state,
            tree,
            revision: None,
            target: String::new(),
            last_target: String::new(),
            selected_file: None,
            filter: String::new(),
            lock_path: String::new(),
            confirmation: None,
            config,
        }
    }
    pub fn set_page(&mut self, page: &str) {
        if page != self.page && page == "disk_analysis" {
            self.tree.refresh_if_empty();
        }
        self.page = page.into();
    }
    fn update_report(&mut self, report: &SharedDiskReport) {
        self.target = report.target.clone();
        if report.target != self.last_target {
            self.tree.set_root(report.treemap.clone());
            self.last_target = report.target.clone();
            self.selected_file = None;
        } else {
            self.tree.update_root(report.treemap.clone());
        }
    }
    pub fn sync_config(&mut self, config: DiskAnalyzerConfig) {
        self.tree.configure_disk(&config);
        self.config = config;
    }
    pub fn canvas(&self, graphics: Rc<D2dContext>) -> Option<(egui::Rect, CanvasPainter)> {
        if self.page == "disk_analysis" && self.confirmation.is_none() {
            self.tree.canvas(graphics)
        } else {
            None
        }
    }
    pub fn draw(&mut self, ui: &mut egui::Ui, events: &mut Vec<SettingsEvent>) {
        let shared = self.state.clone();
        let Ok(state) = shared.lock() else {
            theme::page_content_frame().show(ui, |ui| {
                theme::callout(ui, Tone::Danger, tr("file_tools.state_error"));
            });
            return;
        };
        if self.revision != Some(state.disk_revision) {
            if let Some(report) = &state.disk {
                self.update_report(report);
            }
            self.revision = Some(state.disk_revision);
        }
        if let Some(error) = &state.error {
            theme::page_content_frame().show(ui, |ui| {
                theme::callout(ui, Tone::Danger, error);
            });
        }
        if let Some(action) = self.confirmation.clone() {
            theme::page_content_frame().show(ui, |ui| {
                confirm(ui, &action, &mut self.confirmation, events);
            });
            return;
        }
        if self.page == "file_unlock" {
            theme::page_content_frame().show(ui, |ui| {
                lock_toolbar(ui, &mut self.lock_path, state.busy, events);
            });
            lock_rows(ui, &state, &mut self.confirmation, events);
            return;
        }
        theme::page_content_frame().show(ui, |ui| {
            theme::card_frame()
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.target)
                                .hint_text(tr("file_tools.target_hint"))
                                .margin(egui::Margin::symmetric(12, 8))
                                .desired_width((ui.available_width() - 250.0).max(120.0)),
                        );
                        if ui
                            .add(theme::secondary_button("浏览…").icon(IconKind::Folder))
                            .clicked()
                            && let Some(path) = theme::pick_folder_dialog(Some(&self.target))
                        {
                            self.target = path;
                        }
                        if ui
                            .add_enabled(
                                !state.busy && !self.target.trim().is_empty(),
                                theme::primary_button(tr("file_tools.start_scan"))
                                    .icon(IconKind::Play),
                            )
                            .clicked()
                        {
                            emit(
                                events,
                                FileToolsAction::Analyze {
                                    target: self.target.trim().into(),
                                    filter: self.filter.clone(),
                                },
                            );
                        }
                        if state.busy {
                            ui.spinner();
                        }
                    });
                    if let Some(report) = &state.disk {
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            for (title, value) in [
                                (
                                    tr("file_tools.files_and_folders"),
                                    format!("{} / {}", report.file_count, report.directory_count),
                                ),
                                (tr("file_tools.allocated"), self.size(report.allocated_size)),
                                (tr("file_tools.logical"), self.size(report.logical_size)),
                            ] {
                                stat_pill(ui, &title, &value);
                            }
                        });
                    }
                });
        });
        ui.add_space(2.0);
        let Some(report) = &state.disk else {
            theme::page_content_frame().show(ui, |ui| {
                theme::callout(ui, Tone::Info, tr("file_tools.scan_hint"));
            });
            return;
        };
        let page = self.page.clone();
        match page.as_str() {
            "disk_analysis" => {
                for event in theme::page_content_frame()
                    .show(ui, |ui| self.tree.draw(ui))
                    .inner
                {
                    match event {
                        TreemapEvent::Filter(filter) => {
                            self.filter = filter;
                            emit(
                                events,
                                FileToolsAction::Analyze {
                                    target: report.target.clone(),
                                    filter: self.filter.clone(),
                                },
                            );
                        }
                        TreemapEvent::FileAction(action) => self.dispatch(action, events),
                        _ => {}
                    }
                }
            }
            "disk_top_files" => {
                theme::page_content_frame().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !state.busy,
                                theme::secondary_button(tr("file_tools.find_duplicates")),
                            )
                            .clicked()
                        {
                            emit(events, FileToolsAction::Duplicates);
                        }
                        ui.label(theme::faint(tr("file_tools.duplicates_hint")));
                    });
                });
                let start = events.len();
                if !state.duplicates.is_empty() {
                    egui::CollapsingHeader::new(tr("file_tools.find_duplicates"))
                        .default_open(true)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("disk_duplicates")
                                .max_height(160.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    theme::page_content_frame().show(ui, |ui| {
                                        for (index, group) in state.duplicates.iter().enumerate() {
                                            ui.label(theme::muted(format!(
                                                "{} {} · {}",
                                                tr("file_tools.duplicate_group"),
                                                index + 1,
                                                group.len()
                                            )));
                                            for path in group {
                                                let response = ui
                                                    .add(
                                                        egui::Label::new(theme::muted(path))
                                                            .truncate()
                                                            .sense(egui::Sense::click()),
                                                    )
                                                    .on_hover_text(path);
                                                response.context_menu(|ui| {
                                                    path_menu(
                                                        ui,
                                                        path,
                                                        events,
                                                        &mut self.confirmation,
                                                    )
                                                });
                                            }
                                        }
                                    });
                                });
                        });
                }
                egui::ScrollArea::vertical()
                    .id_salt("disk_top_files")
                    .auto_shrink([false, false])
                    .show_rows(ui, DISK_ROW_HEIGHT, report.top_files.len(), |ui, rows| {
                        theme::page_content_frame().show(ui, |ui| {
                            for index in rows {
                                let entry = &report.top_files[index];
                                let response = disk_file_row(
                                    ui,
                                    entry,
                                    index + 1,
                                    report.logical_size,
                                    &self.config.display_unit,
                                    self.selected_file.as_deref() == Some(&entry.path),
                                );
                                if response.clicked() || response.secondary_clicked() {
                                    self.selected_file = Some(entry.path.clone());
                                }
                                if response.double_clicked() {
                                    emit(
                                        events,
                                        FileToolsAction::Open {
                                            path: entry.path.clone(),
                                        },
                                    );
                                }
                                response.context_menu(|ui| {
                                    path_menu(ui, &entry.path, events, &mut self.confirmation)
                                });
                            }
                        });
                    });
                if let Some(path) = events[start..].iter().find_map(|event| match event {
                    SettingsEvent::FileTools(FileToolsAction::QueryLocks { path }) => {
                        Some(path.clone())
                    }
                    _ => None,
                }) {
                    self.lock_path = path;
                    self.page = "file_unlock".into();
                }
            }
            _ => {
                let total = if self.config.show_allocated_size {
                    report.allocated_size
                } else {
                    report.logical_size
                };
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        theme::page_content_frame().show(ui, |ui| {
                            theme::section_card(ui, None, |ui| {
                                egui::Grid::new("disk_extensions")
                                    .num_columns(5)
                                    .spacing([18.0, 8.0])
                                    .show(ui, |ui| {
                                        for key in
                                            ["extensions", "files", "logical", "allocated", "share"]
                                        {
                                            ui.label(theme::muted(tr(&format!(
                                                "file_tools.{key}"
                                            ))));
                                        }
                                        ui.end_row();
                                        for entry in &report.extensions {
                                            let color = crate::treemap::disk_color(
                                                &entry.extension,
                                                &self.config.palette_colors,
                                            );
                                            ui.horizontal(|ui| {
                                                let (dot, _) = ui.allocate_exact_size(
                                                    egui::vec2(10.0, 10.0),
                                                    egui::Sense::hover(),
                                                );
                                                ui.painter().rect_filled(
                                                    dot,
                                                    3,
                                                    egui::Color32::from_rgb(
                                                        color[0], color[1], color[2],
                                                    ),
                                                );
                                                ui.label(if entry.extension.is_empty() {
                                                    tr("file_tools.no_extension")
                                                } else {
                                                    entry.extension.clone()
                                                });
                                            });
                                            ui.label(theme::muted(entry.file_count.to_string()));
                                            ui.label(theme::muted(self.size(entry.logical_size)));
                                            ui.label(theme::muted(self.size(entry.allocated_size)));
                                            let value = if self.config.show_allocated_size {
                                                entry.allocated_size
                                            } else {
                                                entry.logical_size
                                            };
                                            ui.label(theme::muted(format!(
                                                "{:.1}%",
                                                value as f64 * 100.0 / total.max(1) as f64
                                            )));
                                            ui.end_row();
                                        }
                                    });
                            });
                        });
                    });
            }
        }
    }
    fn size(&self, value: u64) -> String {
        crate::treemap::display_bytes(value, &self.config.display_unit)
    }
    fn dispatch(&mut self, action: FileToolsAction, events: &mut Vec<SettingsEvent>) {
        match action {
            FileToolsAction::Recycle { .. } => self.confirmation = Some(action),
            FileToolsAction::QueryLocks { ref path } => {
                self.lock_path = path.clone();
                self.page = "file_unlock".into();
                emit(events, action);
            }
            _ => emit(events, action),
        }
    }
}

const DISK_ROW_HEIGHT: f32 = 78.0;

fn disk_file_row(
    ui: &mut egui::Ui,
    entry: &DiskEntry,
    rank: usize,
    total: u64,
    unit: &str,
    selected: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), DISK_ROW_HEIGHT),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return response;
    }
    ui.painter().rect(
        rect,
        8,
        if selected {
            theme::ACCENT_SOFT
        } else if response.hovered() {
            theme::NAV_HOVER
        } else {
            theme::CARD
        },
        theme::stroke(
            1.0,
            if selected {
                theme::CONTROL_HOVER_BORDER
            } else {
                theme::CARD_BORDER
            },
        ),
        egui::StrokeKind::Inside,
    );
    let logical = crate::treemap::display_bytes(entry.logical_size, unit);
    let allocated = crate::treemap::display_bytes(entry.allocated_size, unit);
    let content = rect.shrink2(egui::vec2(12.0, 8.0));
    // Reserve the size column before allocating any space to untrusted-length paths.
    let sizes = egui::Rect::from_min_max(
        egui::pos2((content.right() - 200.0).max(content.left()), content.top()),
        content.max,
    );
    let mut right = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(sizes)
            .layout(egui::Layout::top_down(egui::Align::RIGHT)),
    );
    right.set_clip_rect(sizes.intersect(ui.clip_rect()));
    right.spacing_mut().item_spacing.y = 2.0;
    right.add(
        egui::Label::new(theme::strong(ui, &logical, 17.0).color(theme::ACCENT_LIGHT)).truncate(),
    );
    right.add(egui::Label::new(theme::muted(format!("分配: {allocated}"))).truncate());
    theme::pill(
        &mut right,
        format!(
            "{:.1}%",
            entry.logical_size as f64 * 100.0 / total.max(1) as f64
        ),
        theme::ACCENT_LIGHT,
        theme::ACCENT_SOFT,
    );
    let text = egui::Rect::from_min_max(
        content.min,
        egui::pos2((sizes.left() - 12.0).max(content.left()), content.bottom()),
    );
    if text.width() > 0.0 {
        let mut left = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(text)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        left.set_clip_rect(text.intersect(ui.clip_rect()));
        left.spacing_mut().item_spacing.y = 6.0;
        left.add(
            egui::Label::new(theme::strong(ui, format!("#{rank}  {}", entry.name), 14.0))
                .truncate(),
        );
        let folder = std::path::Path::new(&entry.path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new(&entry.path))
            .to_string_lossy();
        left.add(egui::Label::new(theme::muted(folder)).truncate());
    }
    response.on_hover_text(format!(
        "{}\n{}: {logical}\n{}: {allocated}",
        entry.path,
        tr("file_tools.logical"),
        tr("file_tools.allocated"),
    ))
}

fn stat_pill(ui: &mut egui::Ui, title: &str, value: &str) {
    egui::Frame::new()
        .fill(theme::NAV_BG)
        .stroke(theme::stroke(1.0, theme::CARD_BORDER))
        .corner_radius(14)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                ui.label(theme::faint(title));
                ui.label(theme::strong(ui, value, 12.5));
            });
        });
}

fn lock_toolbar(
    ui: &mut egui::Ui,
    lock_path: &mut String,
    busy: bool,
    events: &mut Vec<SettingsEvent>,
) {
    theme::card_frame()
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                let width = (ui.available_width() - 390.0).max(140.0);
                theme::search_field(ui, lock_path, &tr("file_tools.lock_path"), width, 34.0);
                if ui
                    .add(theme::secondary_button("选择文件…").icon(IconKind::Folder))
                    .clicked()
                    && let Some(path) = theme::pick_file_dialog(Some(lock_path), false)
                {
                    *lock_path = path;
                }
                if ui
                    .add(theme::secondary_button("选择文件夹…").icon(IconKind::Folder))
                    .clicked()
                    && let Some(path) = theme::pick_folder_dialog(Some(lock_path))
                {
                    *lock_path = path;
                }
                if ui
                    .add_enabled(
                        !busy && !lock_path.trim().is_empty(),
                        theme::primary_button(tr("file_tools.query_locks")).icon(IconKind::Unlock),
                    )
                    .clicked()
                {
                    emit(
                        events,
                        FileToolsAction::QueryLocks {
                            path: lock_path.trim().into(),
                        },
                    );
                }
                if busy {
                    ui.spinner();
                }
            });
        });
    ui.label(theme::faint(tr("file_tools.lock_hint")));
}

fn lock_rows(
    ui: &mut egui::Ui,
    state: &FileToolsState,
    confirmation: &mut Option<FileToolsAction>,
    events: &mut Vec<SettingsEvent>,
) {
    if state.locks.is_empty() && !state.busy {
        theme::page_content_frame().show(ui, |ui| {
            theme::callout(ui, Tone::Info, tr("file_tools.no_locks"));
        });
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            theme::page_content_frame().show(ui, |ui| {
                for (index, lock) in state.locks.iter().enumerate() {
                    ui.push_id(index, |ui| {
                        theme::list_row_frame().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                theme::pill(
                                    ui,
                                    format!("PID {}", lock.process_id),
                                    theme::ACCENT_LIGHT,
                                    theme::ACCENT_SOFT,
                                );
                                ui.label(theme::strong(ui, &lock.process_name, 14.0));
                                if let Some(handle) = lock.handle {
                                    theme::pill(
                                        ui,
                                        format!("{} 0x{handle:X}", tr("file_tools.handle")),
                                        theme::TEXT_MUTED,
                                        theme::CONTROL,
                                    );
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add_enabled(
                                                !state.busy,
                                                theme::danger_button(tr("file_tools.terminate")),
                                            )
                                            .clicked()
                                        {
                                            *confirmation = Some(FileToolsAction::TerminateLock {
                                                lock: lock.clone(),
                                            });
                                        }
                                        if ui
                                            .add_enabled(
                                                !state.busy && lock.handle.is_some(),
                                                theme::secondary_button(tr(
                                                    "file_tools.close_handle",
                                                )),
                                            )
                                            .clicked()
                                        {
                                            *confirmation = Some(FileToolsAction::CloseHandle {
                                                lock: lock.clone(),
                                            });
                                        }
                                        if ui
                                            .add(theme::secondary_button(tr("file_tools.locate")))
                                            .clicked()
                                        {
                                            emit(
                                                events,
                                                FileToolsAction::Explorer {
                                                    path: lock.path.clone(),
                                                },
                                            );
                                        }
                                    },
                                );
                            });
                            ui.add(egui::Label::new(theme::faint(lock.path.as_str())).truncate());
                        });
                    });
                }
            });
        });
}
fn confirm(
    ui: &mut egui::Ui,
    action: &FileToolsAction,
    pending: &mut Option<FileToolsAction>,
    events: &mut Vec<SettingsEvent>,
) {
    let (key, target) = match action {
        FileToolsAction::TerminateLock { lock } => (
            "terminate_warning",
            format!(
                "{} · {} {}\n{}",
                lock.process_name,
                tr("file_tools.pid"),
                lock.process_id,
                lock.path
            ),
        ),
        FileToolsAction::CloseHandle { lock } => (
            "handle_warning",
            format!(
                "{} · {} {} · {}: 0x{:X}\n{}",
                lock.process_name,
                tr("file_tools.pid"),
                lock.process_id,
                tr("file_tools.handle"),
                lock.handle.unwrap_or_default(),
                lock.path
            ),
        ),
        FileToolsAction::Recycle { path } => ("recycle_warning", path.clone()),
        _ => return,
    };
    theme::section_card(ui, None, |ui| {
        theme::callout(ui, Tone::Warning, tr(&format!("file_tools.{key}")));
        ui.label(RichText::new(target).monospace().color(theme::TEXT_MUTED));
        ui.horizontal(|ui| {
            if ui
                .add(theme::danger_button(tr("file_tools.confirm_risk")))
                .clicked()
            {
                emit(events, action.clone());
                *pending = None;
            }
            if ui
                .add(theme::secondary_button(tr("settings.cancel")))
                .clicked()
            {
                *pending = None;
            }
        });
    });
}
fn path_menu(
    ui: &mut egui::Ui,
    path: &str,
    events: &mut Vec<SettingsEvent>,
    confirmation: &mut Option<FileToolsAction>,
) {
    for (key, action) in [
        ("open", FileToolsAction::Open { path: path.into() }),
        ("explorer", FileToolsAction::Explorer { path: path.into() }),
        ("terminal", FileToolsAction::Terminal { path: path.into() }),
        (
            "query_locks",
            FileToolsAction::QueryLocks { path: path.into() },
        ),
        ("recycle", FileToolsAction::Recycle { path: path.into() }),
    ] {
        if ui.button(tr(&format!("file_tools.{key}"))).clicked() {
            if matches!(action, FileToolsAction::Recycle { .. }) {
                *confirmation = Some(action);
            } else {
                emit(events, action);
            }
            ui.close_menu();
        }
    }
}

pub(crate) struct SearchView {
    state: SharedFileToolsState,
    query: String,
    focus: bool,
    selected: usize,
    confirmation: Option<FileToolsAction>,
    show_locks: bool,
}
impl SearchView {
    pub fn new(state: SharedFileToolsState) -> Self {
        Self {
            state,
            query: String::new(),
            focus: true,
            selected: 0,
            confirmation: None,
            show_locks: false,
        }
    }
    /// Search view with an existing query and highlighted result, for rendered previews.
    pub fn with_query(state: SharedFileToolsState, query: &str, selected: usize) -> Self {
        Self {
            query: query.into(),
            selected,
            ..Self::new(state)
        }
    }
    pub fn draw(&mut self, context: &egui::Context, events: &mut Vec<SettingsEvent>) -> bool {
        let mut close = context.input(|input| input.key_pressed(egui::Key::Escape));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::strong(ui, tr("file_tools.search_title"), 15.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("×").color(theme::TEXT_MUTED))
                                    .frame(false)
                                    .min_size(egui::vec2(28.0, 28.0)),
                            )
                            .on_hover_text(tr("file_tools.close"))
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
                if let Some(action) = self.confirmation.clone() {
                    confirm(ui, &action, &mut self.confirmation, events);
                    return;
                }
                let response = theme::search_field(
                    ui,
                    &mut self.query,
                    &tr("file_tools.search_hint"),
                    f32::INFINITY,
                    44.0,
                );
                if self.focus {
                    response.request_focus();
                    self.focus = false;
                }
                let changed = response.changed();
                if changed {
                    self.selected = 0;
                    self.show_locks = false;
                }
                let port_query = self.query.trim().strip_prefix(':');
                if changed && port_query.is_none() {
                    emit(
                        events,
                        FileToolsAction::Search {
                            query: self.query.clone(),
                        },
                    );
                }
                if let Some(query) = port_query {
                    let query = query
                        .strip_prefix("ports ")
                        .or_else(|| query.strip_prefix("port "))
                        .unwrap_or(query)
                        .trim();
                    match query.parse::<u16>() {
                        Ok(port) if port != 0 => {
                            if ui
                                .add(
                                    theme::primary_button(format!(
                                        "{} :{port}",
                                        tr("file_tools.port_lookup")
                                    ))
                                    .icon(IconKind::Plug),
                                )
                                .clicked()
                                || context.input(|input| input.key_pressed(egui::Key::Enter))
                            {
                                emit(events, FileToolsAction::Ports { port });
                                close = true;
                            }
                        }
                        _ => theme::callout(ui, Tone::Info, tr("file_tools.port_hint")),
                    }
                    return;
                }
                let shared = self.state.clone();
                let Ok(state) = shared.lock() else {
                    theme::callout(ui, Tone::Danger, tr("file_tools.state_error"));
                    return;
                };
                if let Some(error) = &state.error {
                    theme::callout(ui, Tone::Danger, error);
                }
                if self.show_locks {
                    if ui
                        .add(
                            theme::secondary_button(tr("file_tools.back_results"))
                                .icon(IconKind::Back),
                        )
                        .clicked()
                    {
                        self.show_locks = false;
                    }
                    lock_rows(ui, &state, &mut self.confirmation, events);
                    return;
                }
                ui.horizontal(|ui| {
                    ui.label(theme::faint(format!(
                        "{} {}",
                        state.search_results.len(),
                        tr("file_tools.result_count")
                    )));
                    if state.busy {
                        ui.spinner();
                        ui.label(theme::faint(tr("file_tools.busy")));
                    }
                });
                let results_ready = !changed && search_results_ready(&state, &self.query);
                if context.input(|input| input.key_pressed(egui::Key::ArrowDown)) {
                    self.selected =
                        (self.selected + 1).min(state.search_results.len().saturating_sub(1));
                }
                if context.input(|input| input.key_pressed(egui::Key::ArrowUp)) {
                    self.selected = self.selected.saturating_sub(1);
                }
                if results_ready
                    && context.input(|input| input.key_pressed(egui::Key::Enter))
                    && let Some(row) = state.search_results.get(self.selected)
                {
                    emit(
                        events,
                        FileToolsAction::Open {
                            path: row.path.clone(),
                        },
                    );
                    close = true;
                }
                if state.search_results.is_empty() && !state.busy {
                    theme::callout(ui, Tone::Info, tr("file_tools.no_results"));
                }
                let start = events.len();
                ui.spacing_mut().item_spacing.y = 4.0;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(
                        ui,
                        RESULT_HEIGHT,
                        state.search_results.len(),
                        |ui, range| {
                            for index in range {
                                let row = &state.search_results[index];
                                let response = result_row(ui, row, self.selected == index);
                                if response.clicked() {
                                    self.selected = index;
                                }
                                if results_ready && response.double_clicked() {
                                    emit(
                                        events,
                                        FileToolsAction::Open {
                                            path: row.path.clone(),
                                        },
                                    );
                                    close = true;
                                }
                                response.context_menu(|ui| {
                                    ui.add_enabled_ui(results_ready, |ui| {
                                        path_menu(ui, &row.path, events, &mut self.confirmation)
                                    });
                                });
                            }
                        },
                    );
                if events[start..].iter().any(|event| {
                    matches!(
                        event,
                        SettingsEvent::FileTools(FileToolsAction::QueryLocks { .. })
                    )
                }) {
                    self.show_locks = true;
                }
            });
        close
    }
}

fn search_results_ready(state: &FileToolsState, query: &str) -> bool {
    !state.busy && state.error.is_none() && state.search_query.as_deref() == Some(query)
}

const RESULT_HEIGHT: f32 = 48.0;

/// Search result row: type badge, bold name over the folder path, size on the right.
fn result_row(ui: &mut egui::Ui, row: &SearchResult, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), RESULT_HEIGHT),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 8, theme::ACCENT_SOFT);
    } else if response.hovered() {
        // Unselected rows share one background; hover is a faint lift, not a second selection.
        painter.rect_filled(rect, 8, theme::NAV_HOVER);
    }
    let badge = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 34.0, rect.center().y),
        egui::vec2(40.0, 26.0),
    );
    if row.is_directory {
        painter.rect_filled(badge, 7, theme::ORANGE_BG);
        theme::draw_icon(
            painter,
            egui::Rect::from_center_size(badge.center(), egui::vec2(14.0, 14.0)),
            IconKind::Folder,
            theme::WARNING,
        );
    } else {
        let extension = std::path::Path::new(&row.name)
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.chars().take(4).collect::<String>().to_uppercase())
            .unwrap_or_else(|| "FILE".into());
        let (fg, bg) = extension_colors(&extension);
        painter.rect_filled(badge, 7, bg);
        painter.text(
            badge.center(),
            egui::Align2::CENTER_CENTER,
            extension,
            theme::bold(ui.ctx(), 10.5),
            fg,
        );
    }
    let text_left = badge.right() + 14.0;
    let size_text = if row.is_directory {
        tr("file_tools.folder")
    } else {
        bytes(row.logical_size)
    };
    let size = painter.text(
        egui::pos2(rect.right() - 14.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        size_text,
        egui::FontId::proportional(13.0),
        theme::TEXT_MUTED,
    );
    let width = (size.left() - 16.0 - text_left).max(40.0);
    let clip = egui::Rect::from_min_max(
        egui::pos2(text_left, rect.top()),
        egui::pos2(text_left + width, rect.bottom()),
    );
    let painter = painter.with_clip_rect(clip.intersect(ui.clip_rect()));
    painter.text(
        egui::pos2(text_left, rect.top() + 15.0),
        egui::Align2::LEFT_CENTER,
        &row.name,
        theme::bold(ui.ctx(), 14.0),
        theme::TEXT,
    );
    let folder = std::path::Path::new(&row.path)
        .parent()
        .map_or_else(|| row.path.clone(), |parent| parent.display().to_string());
    painter.text(
        egui::pos2(text_left, rect.top() + 34.0),
        egui::Align2::LEFT_CENTER,
        folder,
        egui::FontId::proportional(12.0),
        theme::TEXT_MUTED,
    );
    response
}

fn extension_colors(extension: &str) -> (egui::Color32, egui::Color32) {
    match extension {
        "RS" | "TOML" | "PY" | "JS" | "TS" | "CPP" | "C" | "H" | "JSON" => {
            (theme::ACCENT_LIGHT, theme::ACCENT_SOFT)
        }
        "PNG" | "JPG" | "JPEG" | "GIF" | "WEBP" | "SVG" => (theme::GREEN, theme::GREEN_BG),
        "MP4" | "MKV" | "MOV" | "MP3" | "WAV" | "FLAC" => (theme::PURPLE, theme::PURPLE_BG),
        "ZIP" | "7Z" | "RAR" | "EXE" | "MSI" | "DLL" => (theme::ORANGE, theme::ORANGE_BG),
        "PDF" | "DOCX" | "XLSX" | "PPTX" | "MD" | "TXT" => (theme::DANGER, theme::DANGER_BG),
        _ => (theme::TEXT_MUTED, theme::CONTROL),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_report_updates_preserve_selection_until_the_scan_target_changes() {
        let mut panel = DiskPanel::new(
            "disk_analysis",
            Default::default(),
            Default::default(),
            egui::Context::default(),
        );
        let mut report = SharedDiskReport {
            target: r"C:\first".into(),
            treemap: Arc::new(Node {
                id: r"C:\first".into(),
                label: "first".into(),
                bytes: 1,
                children: Vec::new(),
            }),
            top_files: Vec::new(),
            extensions: Vec::new(),
            logical_size: 1,
            allocated_size: 1,
            file_count: 1,
            directory_count: 0,
        };
        panel.update_report(&report);
        panel.selected_file = Some("selected".into());
        panel.set_page("disk_top_files");
        panel.update_report(&report);
        panel.set_page("disk_analysis");
        assert_eq!(panel.selected_file.as_deref(), Some("selected"));
        report.target = r"C:\".into();
        panel.update_report(&report);
        assert_eq!(panel.last_target, r"C:\");
        assert!(panel.selected_file.is_none());
    }

    #[test]
    fn top_file_cards_keep_both_sizes_visible_with_very_long_paths() {
        let entry = DiskEntry {
            name: "a-very-long-file-name".repeat(40),
            path: format!(r"C:\{}\file.bin", r"very-long-directory\".repeat(100)),
            logical_size: 4 * 1024 * 1024 * 1024,
            allocated_size: 4 * 1024 * 1024 * 1024,
            is_directory: false,
            file_ref_number: 1,
            volume_serial: 1,
        };
        for width in [280.0, 480.0, 1000.0] {
            let context = egui::Context::default();
            let mut row_rect = egui::Rect::NOTHING;
            let output = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 200.0),
                    )),
                    ..Default::default()
                },
                |context| {
                    egui::CentralPanel::default().show(context, |ui| {
                        row_rect =
                            disk_file_row(ui, &entry, 1, entry.logical_size * 2, "Dynamic", true)
                                .rect;
                    });
                },
            );
            assert!(row_rect.right() <= width);
            for label in ["4.0 GiB", "分配: 4.0 GiB", "50.0%"] {
                let (clip, text) = output
                    .shapes
                    .iter()
                    .find_map(|shape| {
                        let egui::Shape::Text(text) = &shape.shape else {
                            return None;
                        };
                        (text.galley.text() == label).then_some((shape.clip_rect, text))
                    })
                    .expect("size and share labels must remain visible");
                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                assert!(
                    row_rect.contains_rect(bounds),
                    "{width}: {label}: {bounds:?}"
                );
                assert!(clip.contains_rect(bounds), "{width}: {label} was clipped");
            }
            for shape in &output.shapes {
                if let egui::Shape::Text(text) = &shape.shape {
                    assert!(
                        text.galley.rect.translate(text.pos.to_vec2()).right()
                            <= row_rect.right() + 0.5
                    );
                }
            }
        }
    }

    #[test]
    fn stale_or_failed_search_results_cannot_be_opened() {
        let mut state = FileToolsState {
            search_query: Some("first".into()),
            ..Default::default()
        };
        assert!(search_results_ready(&state, "first"));
        assert!(!search_results_ready(&state, "second"));
        state.error = Some("invalid regex".into());
        assert!(!search_results_ready(&state, "first"));
        state.error = None;
        state.busy = true;
        assert!(!search_results_ready(&state, "first"));
    }
}
