use crate::{
    D2dContext,
    main_host::{CanvasPainter, SettingsEvent},
    theme::{self, IconKind, Tone},
    treemap::{NestedTreemap, TreemapEvent, bytes},
};
use anyhow::Result;
use egui::RichText;
use orbment_analyzer::{
    memory::{self, MemorySnapshot},
    port::{self, PortEntry},
    process::{self, ProcessIdentity},
    reaper::{self, ReapMode, ReapPreview, ReapReport},
    treemap::Node,
};
use orbment_core::{
    config::{MemoryConfig, ProcessFamily, ReaperConfig},
    i18n::tr,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

enum Job {
    Overview(ReaperConfig),
    Virtual(ProcessIdentity),
    Ports(Option<u16>, Option<String>),
    Preview(ReaperConfig),
    Reap(ReaperConfig, ReapPreview, Vec<ProcessIdentity>, ReapMode),
    Kill(ProcessIdentity),
}
enum Reply {
    Overview(MemorySnapshot, Option<ReapPreview>, Option<String>),
    Virtual(Node),
    Ports(Vec<PortEntry>),
    Preview(ReapPreview),
    Reaped(ReapReport),
    Killed,
}
enum Confirmation {
    Kill(ProcessIdentity),
    Reap(ReapMode),
}
pub(crate) struct AnalyzerPanel {
    pub page: String,
    config: MemoryConfig,
    tree: NestedTreemap,
    overview: Option<MemorySnapshot>,
    /// True while the treemap shows one process's virtual memory instead of the system view.
    drilled: bool,
    preview: Option<ReapPreview>,
    ports: Vec<PortEntry>,
    selected: BTreeSet<ProcessIdentity>,
    port_query: String,
    pending_port_refresh: bool,
    name_query: String,
    error: Option<String>,
    report: Option<ReapReport>,
    busy: bool,
    confirmation: Option<Confirmation>,
    family: Option<Value>,
    sender: mpsc::Sender<Job>,
    receiver: mpsc::Receiver<Result<Reply>>,
    worker: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
}
impl AnalyzerPanel {
    pub fn new(page: &str, config: Value, context: egui::Context) -> Self {
        let mut panel = Self::spawn(page, config, context);
        if panel.worker.is_some() {
            panel.refresh();
        }
        panel
    }
    /// Panel with injected data and no background refresh, for rendered previews.
    pub fn with_data(
        page: &str,
        config: Value,
        context: egui::Context,
        overview: Option<MemorySnapshot>,
        preview: Option<ReapPreview>,
        ports: Vec<PortEntry>,
    ) -> Self {
        let mut panel = Self::spawn(page, config, context);
        if let Some(snapshot) = &overview {
            let mut root = memory::memory_tree(snapshot);
            if let Some(preview) = &preview {
                mark_residuals(&mut root, preview);
            }
            panel.tree.set_root(root);
        }
        panel.overview = overview;
        panel.preview = preview;
        panel.ports = ports;
        panel
    }
    fn spawn(page: &str, config: Value, context: egui::Context) -> Self {
        let parsed = serde_json::from_value::<MemoryConfig>(config["memory"].clone());
        let (config, error) = match parsed {
            Ok(config) => (config, None),
            Err(error) => (MemoryConfig::default(), Some(error.to_string())),
        };
        let (sender, jobs) = mpsc::channel();
        let (replies, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = cancel.clone();
        let wake = context.clone();
        let worker = thread::Builder::new()
            .name("orbment-analyzer-panel".into())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let result = run_job(job, &cancelled);
                    if cancelled.load(Ordering::Acquire) || replies.send(result).is_err() {
                        break;
                    }
                    wake.request_repaint();
                }
            });
        let (worker, error) = match worker {
            Ok(worker) => (Some(worker), error),
            Err(failure) => (None, Some(failure.to_string())),
        };
        Self {
            page: page.into(),
            config,
            tree: NestedTreemap::new(context),
            drilled: false,
            overview: None,
            preview: None,
            ports: Vec::new(),
            selected: BTreeSet::new(),
            port_query: String::new(),
            name_query: String::new(),
            pending_port_refresh: false,
            error,
            report: None,
            busy: false,
            confirmation: None,
            family: None,
            sender,
            receiver,
            worker,
            cancel,
        }
    }
    pub fn set_port_filter(&mut self, port: u16) {
        self.port_query = port.to_string();
        self.name_query.clear();
        self.pending_port_refresh = true;
    }
    pub fn sync_config(&mut self, config: &Value) {
        match serde_json::from_value(config["memory"].clone()) {
            Ok(config) => self.config = config,
            Err(error) => self.error = Some(error.to_string()),
        }
    }
    fn submit(&mut self, job: Job) {
        if self.busy {
            return;
        }
        match self.sender.send(job) {
            Ok(()) => {
                self.busy = true;
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }
    fn refresh(&mut self) {
        match self.page.as_str() {
            "memory_reaper" => self.submit(Job::Preview(self.config.reaper.clone())),
            "memory_ports" => match parse_port(&self.port_query) {
                Ok(port) => self.submit(Job::Ports(
                    port,
                    (!self.name_query.trim().is_empty()).then(|| self.name_query.trim().to_owned()),
                )),
                Err(error) => self.error = Some(error.to_string()),
            },
            _ => self.submit(Job::Overview(self.config.reaper.clone())),
        }
    }
    fn receive(&mut self) {
        while let Ok(reply) = self.receiver.try_recv() {
            self.busy = false;
            match reply {
                Ok(Reply::Overview(snapshot, preview, warning)) => {
                    let mut root = memory::memory_tree(&snapshot);
                    if self.config.reaper.show_in_treemap
                        && let Some(preview) = &preview
                    {
                        mark_residuals(&mut root, preview);
                    }
                    self.tree.set_root(root);
                    self.drilled = false;
                    self.overview = Some(snapshot);
                    self.preview = preview;
                    self.error = warning;
                }
                Ok(Reply::Virtual(root)) => {
                    self.tree.set_root(root);
                    self.drilled = true;
                }
                Ok(Reply::Ports(ports)) => self.ports = ports,
                Ok(Reply::Preview(preview)) => {
                    self.selected = preview
                        .families
                        .iter()
                        .flat_map(|family| {
                            family
                                .processes
                                .iter()
                                .filter_map(|process| process.identity)
                        })
                        .collect();
                    self.preview = Some(preview);
                    self.report = None;
                }
                Ok(Reply::Reaped(report)) => {
                    self.report = Some(report);
                    self.selected.clear();
                    self.preview = None;
                }
                Ok(Reply::Killed) => self.refresh(),
                Err(error) => self.error = Some(format!("{error:#}")),
            }
        }
        if self.pending_port_refresh && !self.busy {
            self.pending_port_refresh = false;
            self.refresh();
        }
    }
    pub fn draw(&mut self, ui: &mut egui::Ui, events: &mut Vec<SettingsEvent>) {
        self.receive();
        if !self.config.enabled {
            theme::page_content_frame().show(ui, |ui| {
                theme::callout(ui, Tone::Warning, tr("analyzer.disabled"));
            });
            return;
        }
        if let Some(error) = &self.error {
            theme::page_content_frame().show(ui, |ui| {
                theme::callout(ui, Tone::Danger, error);
            });
        }
        if self.family.is_some() {
            self.family_editor(ui, events);
            return;
        }
        if self.confirmation.is_some() {
            theme::page_content_frame().show(ui, |ui| self.confirm(ui));
            return;
        }
        match self.page.as_str() {
            "memory_ports" => self.draw_ports(ui),
            "memory_reaper" => self.draw_reaper(ui),
            _ => {
                theme::page_content_frame().show(ui, |ui| self.draw_memory(ui, events));
            }
        }
    }
    fn residual_count(&self) -> usize {
        self.preview.as_ref().map_or(0, |preview| {
            preview
                .families
                .iter()
                .map(|family| family.processes.len())
                .sum()
        })
    }
    fn draw_stats(&mut self, ui: &mut egui::Ui, events: &mut Vec<SettingsEvent>) {
        let actions_width = 176.0;
        let gap = ui.spacing().item_spacing.x;
        let card_width = ((ui.available_width() - actions_width - gap * 3.0) / 3.0).max(160.0);
        ui.horizontal_top(|ui| {
            match &self.overview {
                Some(snapshot) => {
                    let used = snapshot
                        .physical_total
                        .saturating_sub(snapshot.physical_available);
                    theme::stat_card(
                        ui,
                        card_width,
                        &tr("analyzer.physical"),
                        &format!("{} / {}", bytes(used), bytes(snapshot.physical_total)),
                        &format!(
                            "{}% {} · {} {}",
                            snapshot.load_pct,
                            tr("analyzer.used"),
                            tr("analyzer.available"),
                            bytes(snapshot.physical_available)
                        ),
                        Some(snapshot.load_pct as f32 / 100.0),
                    );
                    theme::stat_card(
                        ui,
                        card_width,
                        &format!("{} (Commit)", tr("analyzer.commit")),
                        &format!(
                            "{} / {}",
                            bytes(snapshot.commit_total),
                            bytes(snapshot.commit_limit)
                        ),
                        &format!(
                            "{} {} · {} {}",
                            tr("analyzer.system_cache"),
                            bytes(snapshot.system_cache),
                            tr("analyzer.kernel"),
                            bytes(snapshot.kernel_paged + snapshot.kernel_nonpaged)
                        ),
                        Some(snapshot.commit_total as f32 / snapshot.commit_limit.max(1) as f32),
                    );
                    theme::stat_card(
                        ui,
                        card_width,
                        &tr("analyzer.process_overview"),
                        &format!(
                            "{} {}",
                            snapshot.processes.len(),
                            tr("analyzer.process_unit")
                        ),
                        &format!(
                            "{} {} · {} {}",
                            tr("analyzer.residual"),
                            self.residual_count(),
                            tr("analyzer.inaccessible"),
                            snapshot.inaccessible_processes
                        ),
                        None,
                    );
                }
                None => {
                    for title in [
                        tr("analyzer.physical"),
                        tr("analyzer.commit"),
                        tr("analyzer.process_overview"),
                    ] {
                        theme::stat_card(
                            ui,
                            card_width,
                            &title,
                            "—",
                            &tr("analyzer.loading"),
                            None,
                        );
                    }
                }
            }
            // Two equal buttons split the stat-card height so both columns share top and bottom.
            let button_size = egui::vec2(actions_width, (theme::STAT_CARD_HEIGHT - 8.0) / 2.0);
            ui.allocate_ui_with_layout(
                egui::vec2(actions_width, theme::STAT_CARD_HEIGHT),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    if ui
                        .add(
                            theme::primary_button(tr("analyzer.clean"))
                                .icon(IconKind::Bolt)
                                .min_size(button_size),
                        )
                        .clicked()
                    {
                        events.push(SettingsEvent::Action {
                            action: "memory.clean".into(),
                            params: json!({"regions_mask":self.config.clean_regions_mask}),
                        });
                    }
                    if ui
                        .add_enabled(
                            !self.busy,
                            theme::secondary_button(tr("analyzer.refresh"))
                                .icon(IconKind::Refresh)
                                .min_size(button_size),
                        )
                        .clicked()
                    {
                        self.refresh();
                    }
                },
            );
        });
    }
    fn draw_memory(&mut self, ui: &mut egui::Ui, events: &mut Vec<SettingsEvent>) {
        self.draw_stats(ui, events);
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(theme::faint(tr("analyzer.approximate")));
            if self.busy {
                ui.spinner();
                ui.label(theme::muted(tr("analyzer.loading")));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !orbment_core::single_instance::is_elevated().unwrap_or(false)
                    && ui
                        .add(theme::small_button(tr("analyzer.elevate")).icon(IconKind::Shield))
                        .clicked()
                {
                    events.push(SettingsEvent::Action {
                        action: "memory.elevate".into(),
                        params: json!({}),
                    });
                }
                if self.drilled
                    && let Some(snapshot) = &self.overview
                    && ui
                        .add(theme::small_button(tr("analyzer.system_view")).icon(IconKind::Back))
                        .clicked()
                {
                    let mut root = memory::memory_tree(snapshot);
                    if self.config.reaper.show_in_treemap
                        && let Some(preview) = &self.preview
                    {
                        mark_residuals(&mut root, preview);
                    }
                    self.tree.set_root(root);
                    self.drilled = false;
                }
            });
        });
        for event in self.tree.draw(ui) {
            match event {
                TreemapEvent::Drill(id) => {
                    if let Some(pid) = process_pid(&id) {
                        if let Some(identity) = self
                            .overview
                            .as_ref()
                            .and_then(|overview| {
                                overview.processes.iter().find(|process| process.pid == pid)
                            })
                            .and_then(|process| process.identity)
                        {
                            self.submit(Job::Virtual(identity));
                        } else {
                            self.error = Some(tr("analyzer.process_inaccessible"));
                        }
                    }
                }
                TreemapEvent::Register(id) => {
                    if let Some(pid) = process_pid(id.strip_prefix("family:").unwrap_or(&id))
                        && let Some(overview) = &self.overview
                        && let Some(identity) = overview
                            .processes
                            .iter()
                            .find(|process| process.pid == pid)
                            .and_then(|process| process.identity)
                    {
                        match reaper::prefill_family(&overview.processes, identity)
                            .and_then(|family| serde_json::to_value(family).map_err(Into::into))
                        {
                            Ok(family) => self.family = Some(family),
                            Err(error) => self.error = Some(error.to_string()),
                        }
                    }
                }
                TreemapEvent::Filter(_) | TreemapEvent::FileAction(_) => {}
            }
        }
    }
    fn draw_ports(&mut self, ui: &mut egui::Ui) {
        let mut lookup = false;
        theme::page_content_frame().show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(2, 4))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let response = theme::search_field(
                            ui,
                            &mut self.port_query,
                            &tr("analyzer.port_hint"),
                            180.0,
                            34.0,
                        );
                        lookup |= response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.name_query)
                                .hint_text(tr("analyzer.process_filter"))
                                .margin(egui::Margin::symmetric(12, 8))
                                .desired_width(theme::TEXT_WIDTH),
                        );
                        lookup |= ui
                            .add_enabled(
                                !self.busy,
                                theme::primary_button(tr("analyzer.lookup")).icon(IconKind::Search),
                            )
                            .clicked();
                        if self.busy {
                            ui.spinner();
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            theme::pill(
                                ui,
                                format!("{} {}", self.ports.len(), tr("analyzer.port_count")),
                                theme::ACCENT_LIGHT,
                                theme::ACCENT_SOFT,
                            );
                        });
                    });
                });
        });
        ui.add_space(14.0);
        if lookup {
            self.refresh();
        }
        if !self.busy && self.ports.is_empty() {
            theme::page_content_frame().show(ui, |ui| {
                theme::callout(ui, Tone::Info, tr("analyzer.no_ports"));
            });
        }
        let mut kill = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::page_content_frame().show(ui, |ui| {
                    for (index, entry) in self.ports.iter().enumerate() {
                        ui.push_id(index, |ui| {
                            theme::list_row_frame().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    let (fg, bg) = match entry.protocol {
                                        port::Protocol::Tcp => {
                                            (theme::ACCENT_LIGHT, theme::ACCENT_SOFT)
                                        }
                                        port::Protocol::Udp => (theme::PURPLE, theme::PURPLE_BG),
                                    };
                                    theme::pill(
                                        ui,
                                        format!("{:?}", entry.protocol).to_uppercase(),
                                        fg,
                                        bg,
                                    );
                                    ui.label(
                                        RichText::new(format!(":{}", entry.local_port))
                                            .monospace()
                                            .size(14.0)
                                            .color(theme::TEXT),
                                    );
                                    ui.label(theme::strong(
                                        ui,
                                        entry.process_name.as_deref().unwrap_or("—"),
                                        14.0,
                                    ));
                                    theme::pill(
                                        ui,
                                        format!("PID {}", entry.pid),
                                        theme::TEXT_MUTED,
                                        theme::CONTROL,
                                    );
                                    if let Some(state) = &entry.state {
                                        let (fg, bg) = match state {
                                            port::TcpState::Listen => {
                                                (theme::GREEN, theme::GREEN_BG)
                                            }
                                            port::TcpState::Established => {
                                                (theme::ACCENT_LIGHT, theme::ACCENT_SOFT)
                                            }
                                            _ => (theme::TEXT_MUTED, theme::CONTROL),
                                        };
                                        theme::pill(
                                            ui,
                                            match state {
                                                port::TcpState::Unknown(value) => {
                                                    format!(
                                                        "{} ({value})",
                                                        tr("analyzer.tcp.Unknown")
                                                    )
                                                }
                                                _ => tr(&format!("analyzer.tcp.{state:?}")),
                                            },
                                            fg,
                                            bg,
                                        );
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let size = egui::vec2(76.0, 28.0);
                                            if ui
                                                .add_enabled(
                                                    !self.busy && entry.identity.is_some(),
                                                    theme::danger_button(tr("analyzer.kill"))
                                                        .min_size(size),
                                                )
                                                .clicked()
                                            {
                                                kill = entry.identity;
                                            }
                                            if ui
                                                .add_enabled(
                                                    entry.path.is_some(),
                                                    theme::secondary_button(tr("analyzer.locate"))
                                                        .icon(IconKind::Folder)
                                                        .min_size(size),
                                                )
                                                .clicked()
                                                && let Some(path) = &entry.path
                                                && let Err(error) =
                                                    std::process::Command::new("explorer.exe")
                                                        .arg(format!("/select,{path}"))
                                                        .spawn()
                                            {
                                                self.error = Some(error.to_string());
                                            }
                                            if ui
                                                .add_enabled(
                                                    entry.command_line.is_some(),
                                                    theme::secondary_button(tr(
                                                        "analyzer.copy_command",
                                                    ))
                                                    .icon(IconKind::Copy)
                                                    .min_size(size),
                                                )
                                                .clicked()
                                                && let Some(command) = &entry.command_line
                                            {
                                                ui.ctx().copy_text(command.clone());
                                            }
                                        },
                                    );
                                });
                                let mut endpoint =
                                    format!("{}:{}", entry.local_address, entry.local_port);
                                if let Some(address) = &entry.remote_address {
                                    endpoint.push_str(&format!(
                                        "  →  {}: {}:{}",
                                        tr("analyzer.remote"),
                                        address,
                                        entry.remote_port.unwrap_or_default()
                                    ));
                                }
                                ui.label(
                                    RichText::new(endpoint)
                                        .monospace()
                                        .size(12.0)
                                        .color(theme::TEXT_MUTED),
                                );
                                if let Some(path) = &entry.path {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        theme::icon(ui, IconKind::Folder, theme::TEXT_FAINT);
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(path)
                                                    .size(12.0)
                                                    .color(theme::TEXT_MUTED),
                                            )
                                            .truncate(),
                                        );
                                    });
                                }
                                if let Some(command) = &entry.command_line {
                                    egui::Frame::new()
                                        .fill(theme::CODE_BG)
                                        .stroke(theme::stroke(1.0, theme::ROW_DIVIDER))
                                        .corner_radius(4)
                                        .inner_margin(egui::Margin::symmetric(8, 4))
                                        .show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(command)
                                                        .monospace()
                                                        .size(12.0)
                                                        .color(theme::ACCENT_LIGHT),
                                                )
                                                .truncate(),
                                            );
                                        });
                                }
                            });
                        });
                    }
                });
            });
        if let Some(identity) = kill {
            self.confirmation = Some(Confirmation::Kill(identity));
        }
    }
    fn draw_reaper(&mut self, ui: &mut egui::Ui) {
        theme::page_content_frame().show(ui, |ui| {
            theme::callout(ui, Tone::Info, tr("analyzer.reaper_explanation"));
            ui.horizontal(|ui| {
                let enabled = !self.busy && !self.selected.is_empty();
                if ui
                    .add_enabled(
                        enabled,
                        theme::primary_button(tr("analyzer.close_gracefully")),
                    )
                    .clicked()
                {
                    self.confirmation = Some(Confirmation::Reap(ReapMode::Graceful));
                }
                if ui
                    .add_enabled(enabled, theme::danger_button(tr("analyzer.force")))
                    .clicked()
                {
                    self.confirmation = Some(Confirmation::Reap(ReapMode::Force));
                }
                if ui
                    .add(theme::secondary_button(tr("analyzer.new_family")).icon(IconKind::Plus))
                    .clicked()
                {
                    self.family = Some(
                        serde_json::to_value(ProcessFamily::default())
                            .expect("Process family serializes"),
                    );
                }
                if ui
                    .add_enabled(
                        !self.busy,
                        theme::secondary_button(tr("analyzer.refresh")).icon(IconKind::Refresh),
                    )
                    .clicked()
                {
                    self.refresh();
                }
                if self.busy {
                    ui.spinner();
                }
            });
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::page_content_frame().show(ui, |ui| {
                    if let Some(preview) = &self.preview {
                        if preview.families.is_empty() {
                            theme::callout(ui, Tone::Info, tr("analyzer.no_residuals"));
                        }
                        for family in &preview.families {
                            theme::section_card(ui, Some(&family.family.name), |ui| {
                                for process in &family.processes {
                                    ui.push_id(process.pid, |ui| {
                                        theme::list_row_frame().show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.horizontal(|ui| {
                                                if let Some(identity) = process.identity {
                                                    let mut selected =
                                                        self.selected.contains(&identity);
                                                    if theme::toggle_switch(ui, &mut selected)
                                                        .changed()
                                                    {
                                                        if selected {
                                                            self.selected.insert(identity);
                                                        } else {
                                                            self.selected.remove(&identity);
                                                        }
                                                    }
                                                }
                                                theme::pill(
                                                    ui,
                                                    format!("PID {}", process.pid),
                                                    theme::TEXT_MUTED,
                                                    theme::CONTROL,
                                                );
                                                ui.label(theme::strong(ui, &process.name, 14.0));
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        ui.label(
                                                            RichText::new(bytes(
                                                                process.working_set,
                                                            ))
                                                            .size(13.0)
                                                            .color(theme::TEXT_MUTED),
                                                        );
                                                    },
                                                );
                                            });
                                            if let Some(path) = &process.path {
                                                ui.add(
                                                    egui::Label::new(theme::faint(path.as_str()))
                                                        .truncate(),
                                                );
                                            }
                                        });
                                    });
                                }
                            });
                        }
                    }
                    if let Some(report) = &self.report {
                        theme::section_card(ui, None, |ui| {
                            if let Some(reason) = &report.aborted {
                                theme::callout(ui, Tone::Danger, reason);
                            }
                            for process in &report.processes {
                                theme::setting_row(
                                    ui,
                                    theme::row_label(format!("PID {}", process.identity.pid)),
                                    |ui| {
                                        ui.label(theme::muted(tr(&format!(
                                            "analyzer.reap.{:?}",
                                            process.outcome
                                        ))));
                                    },
                                );
                                if let Some(error) = &process.error {
                                    theme::callout(ui, Tone::Danger, error);
                                }
                            }
                            ui.label(theme::faint(tr("analyzer.refresh_before_force")));
                        });
                    }
                });
            });
    }
    fn confirm(&mut self, ui: &mut egui::Ui) {
        let force = matches!(
            self.confirmation,
            Some(Confirmation::Kill(_)) | Some(Confirmation::Reap(ReapMode::Force))
        );
        theme::section_card(ui, None, |ui| {
            theme::callout(
                ui,
                if force { Tone::Danger } else { Tone::Warning },
                tr(if force {
                    "analyzer.force_warning"
                } else {
                    "analyzer.close_warning"
                }),
            );
            if let Some(Confirmation::Kill(identity)) = &self.confirmation {
                ui.label(theme::row_label(format!("PID {}", identity.pid)));
            } else {
                ui.label(theme::row_label(format!(
                    "{}: {}",
                    tr("analyzer.selected"),
                    self.selected.len()
                )));
            }
            ui.horizontal(|ui| {
                if ui
                    .add(theme::danger_button(tr("analyzer.confirm")))
                    .clicked()
                    && let Some(confirmation) = self.confirmation.take()
                {
                    match confirmation {
                        Confirmation::Kill(identity) => self.submit(Job::Kill(identity)),
                        Confirmation::Reap(mode) => {
                            if let Some(preview) = self.preview.clone() {
                                self.submit(Job::Reap(
                                    self.config.reaper.clone(),
                                    preview,
                                    self.selected.iter().copied().collect(),
                                    mode,
                                ));
                            }
                        }
                    }
                }
                if ui
                    .add(theme::secondary_button(tr("analyzer.cancel")))
                    .clicked()
                {
                    self.confirmation = None;
                }
            });
        });
    }
    fn family_editor(&mut self, ui: &mut egui::Ui, events: &mut Vec<SettingsEvent>) {
        theme::page_content_frame().show(ui, |ui| {
            theme::callout(ui, Tone::Info, tr("analyzer.family_help"));
        });
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::page_content_frame().show(ui, |ui| {
                    theme::section_card(ui, Some(&tr("analyzer.new_family")), |ui| {
                        if let Some(family) = &mut self.family {
                            crate::settings::edit_value(
                                ui,
                                "family",
                                "memory.reaper.families.draft",
                                family,
                            );
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .add(theme::primary_button(tr("analyzer.save_family")))
                            .clicked()
                            && let Some(value) = self.family.clone()
                        {
                            match serde_json::from_value::<ProcessFamily>(value) {
                                Ok(family)
                                    if !family.name.trim().is_empty()
                                        && !family.owners.is_empty()
                                        && !family.companions.is_empty()
                                        && family
                                            .owners
                                            .iter()
                                            .chain(&family.companions)
                                            .all(|name| !name.trim().is_empty()) =>
                                {
                                    self.config.reaper.families.push(family);
                                    match serde_json::to_value(&self.config.reaper) {
                                        Ok(value) => {
                                            events.push(SettingsEvent::ConfigChanged {
                                                path: "memory.reaper".into(),
                                                value,
                                            });
                                            self.family = None;
                                            self.refresh();
                                        }
                                        Err(error) => self.error = Some(error.to_string()),
                                    }
                                }
                                Ok(_) => self.error = Some(tr("analyzer.family_invalid")),
                                Err(error) => self.error = Some(error.to_string()),
                            }
                        }
                        if ui
                            .add(theme::secondary_button(tr("analyzer.cancel")))
                            .clicked()
                        {
                            self.family = None;
                        }
                    });
                });
            });
    }
    pub fn canvas(&self, graphics: Rc<D2dContext>) -> Option<(egui::Rect, CanvasPainter)> {
        if self.page != "memory_analyzer"
            || self.family.is_some()
            || self.confirmation.is_some()
            || !self.config.enabled
        {
            None
        } else {
            self.tree.canvas(graphics)
        }
    }
}
impl Drop for AnalyzerPanel {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
fn run_job(job: Job, cancel: &AtomicBool) -> Result<Reply> {
    match job {
        Job::Overview(config) => {
            let snapshot = memory::snapshot()?;
            let (preview, warning) = match reaper::preview(&config) {
                Ok(preview) => (Some(preview), None),
                Err(error) => (None, Some(error.to_string())),
            };
            Ok(Reply::Overview(snapshot, preview, warning))
        }
        Job::Virtual(identity) => Ok(Reply::Virtual(memory::virtual_tree(
            &memory::virtual_regions(identity)?,
        ))),
        Job::Ports(port, name) => Ok(Reply::Ports(port::lookup(port, name.as_deref())?)),
        Job::Preview(config) => Ok(Reply::Preview(reaper::preview(&config)?)),
        Job::Reap(config, preview, identities, mode) => Ok(Reply::Reaped(reaper::execute_with_cancel(
            &config,
            &preview,
            &identities,
            mode,
            cancel,
        )?)),
        Job::Kill(identity) => {
            process::kill(identity)?;
            Ok(Reply::Killed)
        }
    }
}
fn parse_port(value: &str) -> Result<Option<u16>> {
    if value.trim().is_empty() {
        Ok(None)
    } else {
        port::parse_port_filter(value).map(Some)
    }
}
fn process_pid(id: &str) -> Option<u32> {
    id.strip_prefix("process:")
        .and_then(|value| value.parse().ok())
}
fn mark_residuals(root: &mut Node, preview: &ReapPreview) {
    let pids: BTreeSet<_> = preview
        .families
        .iter()
        .flat_map(|family| family.processes.iter().map(|process| process.pid))
        .collect();
    fn mark(node: &mut Node, pids: &BTreeSet<u32>) -> bool {
        let mut residual = process_pid(&node.id).is_some_and(|pid| pids.contains(&pid));
        for child in &mut node.children {
            residual |= mark(child, pids);
        }
        if residual && (node.id.starts_with("family:") || node.id.starts_with("process:")) {
            node.label = format!("{} [{}]", node.label, tr("analyzer.residual"));
        }
        residual
    }
    mark(root, &pids);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destroying_panel_cancels_without_joining_running_work() {
        let mut panel = AnalyzerPanel::spawn(
            "memory_analyzer",
            json!({"memory": MemoryConfig::default()}),
            egui::Context::default(),
        );
        let cancel = panel.cancel.clone();
        let (release, blocked) = mpsc::channel();
        panel.worker = Some(thread::spawn(move || {
            let _ = blocked.recv_timeout(std::time::Duration::from_secs(5));
        }));
        drop(panel);
        assert!(cancel.load(Ordering::Acquire));
        assert!(release.send(()).is_ok(), "panel destruction waited for the worker");
    }
}
