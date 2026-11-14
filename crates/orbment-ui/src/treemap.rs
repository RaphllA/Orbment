use crate::{D2dContext, main_host::CanvasPainter, theme};
use orbment_analyzer::treemap::{self, LayoutOptions, Node, Primitive, PrimitiveKind, Rect};
use orbment_core::i18n::tr;
use std::{
    collections::HashMap,
    rc::Rc,
    sync::{Arc, mpsc},
    thread,
};
use windows::Win32::Graphics::{
    Direct2D::{Common::*, *},
    DirectWrite::*,
};
use windows_numerics::Vector2;

const CANVAS_BG: egui::Color32 = theme::NAV_BG;
const CONTAINER_BG: egui::Color32 = theme::BG;
const CONTAINER_TITLE: egui::Color32 = theme::CARD_HOVER;
/// Matches `LayoutOptions::default().title_height`.
const TITLE_HEIGHT: f32 = 22.0;
/// Blocks smaller than this (px²) fold into the aggregate "other" block instead of rendering
/// as unreadable slivers.
const MIN_BLOCK_AREA: f32 = 480.0;
/// Low-saturation dashboard hues: blue, cyan, emerald, amber, violet, rose, indigo, teal.
const PALETTE: [[f32; 3]; 8] = [
    [0.231, 0.435, 0.714],
    [0.165, 0.549, 0.620],
    [0.184, 0.620, 0.455],
    [0.718, 0.525, 0.184],
    [0.486, 0.361, 0.769],
    [0.722, 0.361, 0.478],
    [0.310, 0.373, 0.780],
    [0.204, 0.561, 0.518],
];
const AGGREGATE: [f32; 3] = [0.650, 0.630, 0.580];

fn rgba(color: egui::Color32) -> [f32; 4] {
    [
        color.r() as f32 / 255.0,
        color.g() as f32 / 255.0,
        color.b() as f32 / 255.0,
        color.a() as f32 / 255.0,
    ]
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn memory_category_hue(value: &str) -> Option<usize> {
    let value = value.to_ascii_lowercase();
    match value.as_str() {
        "cache" => return Some(1),
        "pools" => return Some(2),
        _ => {}
    }
    [
        (["processes", "进程"], 0),
        (["standby", "缓存"], 1),
        (["kernel", "内核"], 2),
        (["modified", "修改"], 3),
        (["free", "可用"], 7),
    ]
    .into_iter()
    .find_map(|(names, hue)| names.iter().any(|name| value.contains(*name)).then_some(hue))
}

/// Colors depend on source identity, never on which siblings survive layout aggregation.
fn assign_hues(primitives: &[Primitive]) -> Vec<Option<(usize, usize)>> {
    let mut hues: HashMap<&str, usize> = HashMap::new();
    primitives
        .iter()
        .map(|primitive| {
            if primitive.depth == 0 || primitive.kind == PrimitiveKind::Aggregate {
                return None;
            }
            let identity = primitive
                .id
                .as_deref()
                .filter(|id| !id.is_empty())
                .unwrap_or(&primitive.label);
            let hash = stable_hash(identity);
            let own_hue = (hash % PALETTE.len() as u64) as usize;
            let hue = match primitive.depth {
                1 => memory_category_hue(identity)
                    .or_else(|| memory_category_hue(&primitive.label))
                    .unwrap_or(own_hue),
                2 => own_hue,
                _ => primitive
                    .parent_id
                    .as_deref()
                    .and_then(|parent| hues.get(parent).copied())
                    .unwrap_or(own_hue),
            };
            if let Some(id) = &primitive.id {
                hues.insert(id, hue);
            }
            Some((hue, ((hash >> 32) % 4) as usize))
        })
        .collect()
}

unsafe fn draw_label(
    target: &ID2D1DeviceContext,
    text: &str,
    format: &IDWriteTextFormat,
    brush: &ID2D1SolidColorBrush,
    rect: D2D_RECT_F,
) {
    if rect.right - rect.left < 8.0 {
        return;
    }
    let text: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        target.DrawText(
            &text,
            format,
            &rect,
            brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
}

struct Request {
    generation: u64,
    root: Arc<Node>,
    subpath: Option<String>,
    bounds: Rect,
    depth: usize,
    filter: String,
    min_area: f32,
}
enum Job {
    Layout(Request),
    Stop,
}
pub enum TreemapEvent {
    Drill(String),
    Register(String),
    Filter(String),
    FileAction(crate::FileToolsAction),
}
/// The same nested canvas accepts memory, virtual-memory, or future disk trees.
pub struct NestedTreemap {
    root: Option<Arc<Node>>,
    path: Vec<String>,
    selected: Option<String>,
    depth: usize,
    filter: String,
    primitives: Arc<Vec<Primitive>>,
    bounds: egui::Rect,
    generation: u64,
    minimum_generation: u64,
    dirty: bool,
    sender: mpsc::Sender<Job>,
    receiver: mpsc::Receiver<(u64, Vec<Primitive>)>,
    error: Option<String>,
    disk_config: Option<orbment_core::config::DiskAnalyzerConfig>,
}
impl NestedTreemap {
    pub fn new(context: egui::Context) -> Self {
        let (sender, requests) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("orbment-treemap-layout".into())
            .spawn(move || {
                while let Ok(job) = requests.recv() {
                    let mut job = job;
                    while let Ok(next) = requests.try_recv() {
                        job = next;
                    }
                    let Job::Layout(request) = job else { break };
                    let output = layout_request(&request);
                    if results.send((request.generation, output)).is_err() {
                        break;
                    }
                    context.request_repaint();
                }
            });
        let error = worker.err().map(|error| error.to_string());
        Self {
            root: None,
            path: Vec::new(),
            selected: None,
            depth: 3,
            filter: String::new(),
            primitives: Arc::new(Vec::new()),
            bounds: egui::Rect::NOTHING,
            generation: 0,
            minimum_generation: 0,
            dirty: true,
            sender,
            receiver,
            error,
            disk_config: None,
        }
    }
    pub fn set_root(&mut self, root: impl Into<Arc<Node>>) {
        self.root = Some(root.into());
        self.path.clear();
        self.selected = None;
        self.primitives = Arc::new(Vec::new());
        self.minimum_generation = self.generation + 1;
        self.dirty = true;
    }
    pub fn configure_disk(&mut self, config: &orbment_core::config::DiskAnalyzerConfig) {
        if self
            .disk_config
            .as_ref()
            .is_none_or(|old| old.default_detail_depth != config.default_detail_depth)
        {
            self.depth = config.default_detail_depth as usize;
        }
        self.disk_config = Some(config.clone());
        self.dirty = true;
    }
    pub fn update_root(&mut self, root: impl Into<Arc<Node>>) {
        let root = root.into();
        self.path.retain(|id| find(&root, id).is_some());
        if self
            .selected
            .as_ref()
            .is_some_and(|id| find(&root, id).is_none())
        {
            self.selected = None;
        }
        self.root = Some(root);
        self.dirty = true;
    }
    pub fn refresh_if_empty(&mut self) {
        if self.primitives.is_empty() && self.root.is_some() {
            self.dirty = true;
        }
    }
    fn navigate_to(&mut self, id: &str) {
        if let Some(root) = &self.root
            && find(root, id).is_some_and(|node| !node.children.is_empty())
            && let Some(path) = node_path(root, id)
        {
            self.path = path;
            self.invalidate_navigation();
        }
    }
    fn navigate_back(&mut self, length: usize) {
        self.path.truncate(length);
        self.invalidate_navigation();
    }
    fn invalidate_navigation(&mut self) {
        self.selected = None;
        self.primitives = Arc::new(Vec::new());
        self.minimum_generation = self.generation + 1;
        self.dirty = true;
    }
    pub fn draw(&mut self, ui: &mut egui::Ui) -> Vec<TreemapEvent> {
        let mut events = Vec::new();
        if let Some(error) = &self.error {
            theme::callout(ui, theme::Tone::Danger, error);
        }
        while let Ok((generation, primitives)) = self.receiver.try_recv() {
            if generation >= self.minimum_generation
                && generation <= self.generation
                && (!primitives.is_empty() || generation == self.generation)
            {
                self.primitives = Arc::new(primitives);
            }
        }
        let disk = self.disk_config.is_some();
        ui.horizontal_wrapped(|ui| {
            egui::Frame::new()
                .fill(theme::CONTROL)
                .stroke(theme::stroke(1.0, theme::CONTROL_BORDER))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(4, 2))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let step = |text: &str| {
                        egui::Button::new(egui::RichText::new(text).size(15.0).color(theme::TEXT))
                            .frame(false)
                            .min_size(egui::vec2(24.0, 24.0))
                    };
                    if ui.add_enabled(self.depth > 1, step("−")).clicked() {
                        self.depth -= 1;
                        self.dirty = true;
                    }
                    ui.label(theme::muted(format!(
                        "{} {}",
                        tr("analyzer.depth"),
                        self.depth
                    )));
                    if ui.add_enabled(self.depth < 8, step("+")).clicked() {
                        self.depth += 1;
                        self.dirty = true;
                    }
                });
            let hint = tr(if disk {
                "file_tools.filter"
            } else {
                "analyzer.filter"
            });
            if theme::search_field(ui, &mut self.filter, &hint, 240.0, 32.0).changed() {
                if disk {
                    events.push(TreemapEvent::Filter(self.filter.clone()));
                } else {
                    self.dirty = true;
                }
            }
            ui.add_space(4.0);
            ui.spacing_mut().item_spacing.x = 4.0;
            let crumb = |ui: &mut egui::Ui, text: &str, last: bool| {
                ui.add(
                    egui::Button::new(egui::RichText::new(text).size(12.5).color(if last {
                        theme::TEXT
                    } else {
                        theme::TEXT_MUTED
                    }))
                    .fill(if last {
                        theme::ACCENT_SOFT
                    } else {
                        theme::CARD
                    })
                    .stroke(theme::stroke(
                        1.0,
                        if last {
                            theme::ACCENT_SOFT
                        } else {
                            theme::CARD_BORDER
                        },
                    ))
                    .corner_radius(12)
                    .min_size(egui::vec2(0.0, 24.0)),
                )
            };
            if ui
                .add_enabled(!self.path.is_empty(), egui::Button::new("⬅ 返回上一级"))
                .clicked()
            {
                self.navigate_back(self.path.len().saturating_sub(1));
            }
            let mut target = None;
            if let Some(root) = &self.root {
                let label = if root.label.is_empty() {
                    &root.id
                } else {
                    &root.label
                };
                if crumb(ui, label, self.path.is_empty()).clicked() {
                    target = Some(0);
                }
                let count = self.path.len();
                for (index, id) in self.path.iter().enumerate() {
                    if let Some(node) = find(root, id) {
                        ui.label(theme::faint("›"));
                        if crumb(ui, &node.label, index + 1 == count).clicked() {
                            target = Some(index + 1);
                        }
                    }
                }
            }
            if let Some(length) = target {
                self.navigate_back(length);
            }
        });
        ui.horizontal(|ui| {
            let selected = self.selected.as_ref().and_then(|selected| {
                self.primitives
                    .iter()
                    .find(|block| block.id.as_ref() == Some(selected))
            });
            if let Some(block) = selected {
                theme::pill(ui, "●", theme::ACCENT_LIGHT, theme::ACCENT_SOFT);
                ui.label(theme::strong(ui, &block.label, 13.0));
                ui.label(theme::muted(display_bytes(
                    block.bytes,
                    self.disk_config
                        .as_ref()
                        .map_or("Dynamic", |config| config.display_unit.as_str()),
                )));
            } else {
                ui.label(theme::faint(tr(if disk {
                    "file_tools.tree_help"
                } else {
                    "analyzer.tree_help"
                })));
            }
        });
        let size = egui::vec2(
            ui.available_width().max(1.0),
            ui.available_height().max(180.0),
        );
        let (frame, response) = ui.allocate_exact_size(size, egui::Sense::click());
        ui.painter().rect(
            frame,
            8,
            CANVAS_BG,
            theme::stroke(1.0, theme::CARD_BORDER),
            egui::StrokeKind::Inside,
        );
        let bounds = frame.shrink(6.0);
        if bounds != self.bounds {
            self.bounds = bounds;
            self.dirty = true;
        }
        if self.dirty {
            if let Some(root) = &self.root {
                self.generation += 1;
                let request = Request {
                    generation: self.generation,
                    root: root.clone(),
                    subpath: self.path.last().cloned(),
                    bounds: Rect {
                        x: bounds.left(),
                        y: bounds.top(),
                        width: bounds.width(),
                        height: bounds.height(),
                    },
                    depth: self.depth,
                    filter: if self.disk_config.is_some() {
                        String::new()
                    } else {
                        self.filter.clone()
                    },
                    min_area: self.disk_config.as_ref().map_or(MIN_BLOCK_AREA, |config| {
                        (config.min_block_pixel_size as f32).powi(2)
                    }),
                };
                if self.sender.send(Job::Layout(request)).is_err() {
                    ui.colored_label(theme::DANGER, tr("analyzer.worker_stopped"));
                }
            }
            self.dirty = false;
        }
        if let Some(pos) = response.interact_pointer_pos()
            && let Some(block) = self
                .primitives
                .iter()
                .rev()
                .find(|block| block.rect.contains(pos.x, pos.y))
            && let Some(id) = block.id.clone()
        {
            self.selected = Some(id.clone());
            if response.double_clicked() {
                self.navigate_to(&id);
                events.push(TreemapEvent::Drill(id));
            }
        }
        response.context_menu(|ui| {
            if self.disk_config.is_some() {
                if let Some(id) = self
                    .selected
                    .clone()
                    .filter(|id| !id.starts_with("orbment:free-space:"))
                {
                    for (key, action) in [
                        (
                            "explorer",
                            crate::FileToolsAction::Explorer { path: id.clone() },
                        ),
                        (
                            "terminal",
                            crate::FileToolsAction::Terminal { path: id.clone() },
                        ),
                        (
                            "recycle",
                            crate::FileToolsAction::Recycle { path: id.clone() },
                        ),
                        (
                            "query_locks",
                            crate::FileToolsAction::QueryLocks { path: id },
                        ),
                    ] {
                        if ui.button(tr(&format!("file_tools.{key}"))).clicked() {
                            events.push(TreemapEvent::FileAction(action));
                            ui.close_menu();
                        }
                    }
                }
            } else if ui.button(tr("analyzer.register_family")).clicked() {
                if let Some(id) = &self.selected {
                    events.push(TreemapEvent::Register(id.clone()));
                }
                ui.close_menu();
            }
            if ui
                .add_enabled(
                    !self.path.is_empty(),
                    egui::Button::new(tr("analyzer.zoom_out")),
                )
                .clicked()
            {
                self.navigate_back(self.path.len().saturating_sub(1));
                ui.close_menu();
            }
        });
        if self.dirty {
            ui.ctx().request_repaint();
        }
        events
    }
    pub fn canvas(&self, graphics: Rc<D2dContext>) -> Option<(egui::Rect, CanvasPainter)> {
        if self.primitives.is_empty() || !self.bounds.is_positive() {
            return None;
        }
        let primitives = self.primitives.clone();
        let selected = self.selected.clone();
        let disk_config = self.disk_config.clone();
        Some((
            self.bounds,
            Box::new(move |target| {
                let format = |size: f32, bold: bool| -> anyhow::Result<IDWriteTextFormat> {
                    let format =
                        graphics.text_format("Microsoft YaHei UI", size, u32::from(bold))?;
                    unsafe {
                        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
                        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
                        let sign = graphics
                            .write_factory()
                            .CreateEllipsisTrimmingSign(&format)?;
                        format.SetTrimming(
                            &DWRITE_TRIMMING {
                                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                                delimiter: 0,
                                delimiterCount: 0,
                            },
                            &sign,
                        )?;
                    }
                    Ok(format)
                };
                let title_font = format(9.0, true)?;
                let name_font = format(9.0, true)?;
                let size_font = format(8.25, false)?;
                let unit = disk_config
                    .as_ref()
                    .map_or("Dynamic", |config| config.display_unit.as_str());
                let hues = assign_hues(&primitives);
                unsafe {
                    let solid = |color: [f32; 4]| {
                        target.CreateSolidColorBrush(
                            &D2D1_COLOR_F {
                                r: color[0],
                                g: color[1],
                                b: color[2],
                                a: color[3],
                            },
                            None,
                        )
                    };
                    let text = solid(rgba(theme::TEXT))?;
                    let muted = solid(rgba(theme::TEXT_MUTED))?;
                    let container_fill = solid(rgba(CONTAINER_BG))?;
                    let title_fill = solid(rgba(CONTAINER_TITLE))?;
                    let container_edge = solid(rgba(theme::CARD_BORDER))?;
                    let leaf_edge = solid(rgba(theme::CARD_BORDER))?;
                    let mut gradients = std::collections::HashMap::new();
                    for (primitive, hue) in primitives.iter().zip(&hues) {
                        let r = primitive.rect;
                        if r.width < 1.0 || r.height < 1.0 {
                            continue;
                        }
                        let rect = D2D_RECT_F {
                            left: r.x,
                            top: r.y,
                            right: r.x + r.width,
                            bottom: r.y + r.height,
                        };
                        let highlight = primitive.id.is_some() && primitive.id == selected;
                        if primitive.kind == PrimitiveKind::Container {
                            let rounded = D2D1_ROUNDED_RECT {
                                rect,
                                radiusX: 5.0,
                                radiusY: 5.0,
                            };
                            target.FillRoundedRectangle(&rounded, &container_fill);
                            let title = D2D_RECT_F {
                                bottom: (r.y + TITLE_HEIGHT).min(rect.bottom),
                                ..rect
                            };
                            target.FillRoundedRectangle(
                                &D2D1_ROUNDED_RECT {
                                    rect: title,
                                    radiusX: 5.0,
                                    radiusY: 5.0,
                                },
                                &title_fill,
                            );
                            // Accent bars and titles need room; slivers stay plain.
                            let roomy = r.width >= 64.0 && r.height >= 36.0;
                            if let Some(color) = hue
                                .filter(|_| roomy)
                                .map(|(index, _)| PALETTE[index % PALETTE.len()])
                            {
                                let accent = solid([color[0], color[1], color[2], 1.0])?;
                                target.FillRectangle(
                                    &D2D_RECT_F {
                                        left: r.x + 6.0,
                                        top: r.y + 7.0,
                                        right: r.x + 9.0,
                                        bottom: (r.y + TITLE_HEIGHT - 7.0).max(r.y + 8.0),
                                    },
                                    &accent,
                                );
                            }
                            target.DrawRoundedRectangle(
                                &rounded,
                                if highlight { &text } else { &container_edge },
                                if highlight { 2.0 } else { 1.0 },
                                None,
                            );
                            if roomy {
                                let size = display_bytes(primitive.bytes, unit);
                                let size_width =
                                    (size.chars().count() as f32 * 6.4 + 10.0).min(r.width * 0.45);
                                draw_label(
                                    target,
                                    &primitive.label,
                                    &title_font,
                                    &text,
                                    D2D_RECT_F {
                                        left: r.x + 14.0,
                                        top: r.y + 4.0,
                                        right: rect.right - size_width - 6.0,
                                        bottom: r.y + TITLE_HEIGHT,
                                    },
                                );
                                draw_label(
                                    target,
                                    &size,
                                    &size_font,
                                    &muted,
                                    D2D_RECT_F {
                                        left: rect.right - size_width,
                                        top: r.y + 5.0,
                                        right: rect.right - 6.0,
                                        bottom: r.y + TITLE_HEIGHT,
                                    },
                                );
                            }
                            continue;
                        }
                        let base = if primitive.kind == PrimitiveKind::Aggregate {
                            AGGREGATE
                        } else if let Some(config) = &disk_config {
                            let extension = primitive
                                .id
                                .as_deref()
                                .and_then(|path| std::path::Path::new(path).extension())
                                .and_then(|value| value.to_str())
                                .map(str::to_lowercase)
                                .unwrap_or_default();
                            let color = disk_color(&extension, &config.palette_colors);
                            color.map(|channel| channel as f32 / 255.0)
                        } else {
                            let (index, ordinal) = hue.unwrap_or((0, 0));
                            let color = PALETTE[index % PALETTE.len()];
                            let shade = [1.0, 0.9, 1.08, 0.82][ordinal % 4];
                            [color[0] * shade, color[1] * shade, color[2] * shade]
                        };
                        // Preserve category hue while keeping dark labels readable on paper tints.
                        let base = base.map(|channel| 0.70 + 0.30 * channel.min(1.0));
                        let key = base.map(|channel| (channel * 255.0) as u8);
                        let brush: &ID2D1LinearGradientBrush = match gradients.entry(key) {
                            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                let stop = |position: f32, scale: f32| D2D1_GRADIENT_STOP {
                                    position,
                                    color: D2D1_COLOR_F {
                                        r: (base[0] * scale).min(1.0),
                                        g: (base[1] * scale).min(1.0),
                                        b: (base[2] * scale).min(1.0),
                                        a: 1.0,
                                    },
                                };
                                let render_target: &ID2D1RenderTarget = target;
                                let stops = render_target.CreateGradientStopCollection(
                                    &[stop(0.0, 1.025), stop(1.0, 0.985)],
                                    D2D1_GAMMA_2_2,
                                    D2D1_EXTEND_MODE_CLAMP,
                                )?;
                                entry.insert(target.CreateLinearGradientBrush(
                                    &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                                        startPoint: Vector2 { X: 0.0, Y: 0.0 },
                                        endPoint: Vector2 { X: 0.0, Y: 1.0 },
                                    },
                                    None,
                                    &stops,
                                )?)
                            }
                        };
                        brush.SetStartPoint(Vector2 { X: r.x, Y: r.y });
                        brush.SetEndPoint(Vector2 {
                            X: r.x,
                            Y: r.y + r.height,
                        });
                        let inset = if r.width > 4.0 && r.height > 4.0 {
                            1.0
                        } else {
                            0.0
                        };
                        let rounded = D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: rect.left + inset,
                                top: rect.top + inset,
                                right: rect.right - inset,
                                bottom: rect.bottom - inset,
                            },
                            radiusX: 3.0,
                            radiusY: 3.0,
                        };
                        target.FillRoundedRectangle(&rounded, brush);
                        target.DrawRoundedRectangle(
                            &rounded,
                            if highlight { &text } else { &leaf_edge },
                            if highlight { 2.0 } else { 1.0 },
                            None,
                        );
                        if r.width >= 52.0 && r.height >= 22.0 {
                            draw_label(
                                target,
                                &primitive.label,
                                &name_font,
                                &text,
                                D2D_RECT_F {
                                    left: r.x + 7.0,
                                    top: r.y + 4.0,
                                    right: rect.right - 5.0,
                                    bottom: (r.y + 20.0).min(rect.bottom),
                                },
                            );
                            if r.height >= 38.0 {
                                draw_label(
                                    target,
                                    &display_bytes(primitive.bytes, unit),
                                    &size_font,
                                    &muted,
                                    D2D_RECT_F {
                                        left: r.x + 7.0,
                                        top: r.y + 20.0,
                                        right: rect.right - 5.0,
                                        bottom: (r.y + 36.0).min(rect.bottom),
                                    },
                                );
                            }
                        }
                    }
                }
                Ok(())
            }),
        ))
    }
}
impl Drop for NestedTreemap {
    fn drop(&mut self) {
        let _ = self.sender.send(Job::Stop);
    }
}
fn layout_request(request: &Request) -> Vec<Primitive> {
    let root = request.subpath.as_deref()
        .and_then(|id| find(&request.root, id))
        .unwrap_or(&request.root);
    let filtered = if request.filter.is_empty() {
        None
    } else {
        filter_node(root, &request.filter.to_lowercase())
    };
    if !request.filter.is_empty() && filtered.is_none() {
        return Vec::new();
    }
    treemap::layout(
        filtered.as_ref().unwrap_or(root),
        request.bounds,
        LayoutOptions {
            max_depth: request.depth,
            min_area: request.min_area,
            ..Default::default()
        },
    )
}

fn find<'a>(node: &'a Node, id: &str) -> Option<&'a Node> {
    if node.id == id {
        Some(node)
    } else {
        node.children.iter().find_map(|child| find(child, id))
    }
}
fn filter_node(node: &Node, query: &str) -> Option<Node> {
    if node.label.to_lowercase().contains(query) {
        return Some(node.clone());
    }
    let children: Vec<_> = node
        .children
        .iter()
        .filter_map(|child| filter_node(child, query))
        .collect();
    if children.is_empty() {
        None
    } else {
        Some(Node {
            id: node.id.clone(),
            label: node.label.clone(),
            bytes: children.iter().map(|child| child.bytes).sum(),
            children,
        })
    }
}
pub(crate) fn bytes(value: u64) -> String {
    let mut number = value as f64;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB"] {
        if number < 1024.0 {
            break;
        }
        number /= 1024.0;
        unit = next;
    }
    format!("{number:.1} {unit}")
}

pub(crate) fn display_bytes(value: u64, unit: &str) -> String {
    let power = match unit {
        "Bytes" => 0,
        "KB" => 1,
        "MB" => 2,
        "GB" => 3,
        "TB" => 4,
        _ => return bytes(value),
    };
    let suffix = tr(&format!("file_tools.unit.{unit}"));
    if power == 0 {
        format!("{value} {suffix}")
    } else {
        format!("{:.2} {suffix}", value as f64 / 1024_f64.powi(power))
    }
}

pub(crate) fn disk_color(extension: &str, palette: &[String]) -> [u8; 3] {
    let index = extension.bytes().fold(0usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as usize)
    });
    palette
        .get(index % palette.len().max(1))
        .and_then(|color| u32::from_str_radix(color.trim_start_matches('#'), 16).ok())
        .map(|value| [(value >> 16) as u8, (value >> 8) as u8, value as u8])
        .unwrap_or([46, 94, 138])
}

fn node_path(root: &Node, id: &str) -> Option<Vec<String>> {
    if root.id == id {
        return Some(Vec::new());
    }
    for child in &root.children {
        if let Some(mut path) = node_path(child, id) {
            path.insert(0, child.id.clone());
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: &str, bytes: u64) -> Node {
        Node {
            id: id.into(),
            label: id.into(),
            bytes,
            children: Vec::new(),
        }
    }

    fn colors(primitives: &[Primitive]) -> HashMap<String, (usize, usize)> {
        primitives
            .iter()
            .zip(assign_hues(primitives))
            .filter_map(|(primitive, color)| Some((primitive.id.clone()?, color?)))
            .collect()
    }

    fn disk_root() -> Arc<Node> {
        Arc::new(Node {
            children: vec![
                Node {
                    children: vec![Node {
                        children: vec![leaf("file", 50)],
                        ..leaf("nested", 50)
                    }],
                    ..leaf("directory", 50)
                },
                leaf("sibling", 50),
            ],
            ..leaf(r"C:\", 100)
        })
    }

    fn draw_tree(
        tree: &mut NestedTreemap,
        context: &egui::Context,
        input: Vec<egui::Event>,
    ) -> (egui::FullOutput, Vec<TreemapEvent>) {
        let mut events = Vec::new();
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 700.0),
                )),
                events: input,
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| events.extend(tree.draw(ui)));
            },
        );
        (output, events)
    }

    #[test]
    fn double_click_drills_into_cached_descendants_without_scanning() {
        let context = egui::Context::default();
        let mut tree = NestedTreemap::new(context.clone());
        tree.configure_disk(&Default::default());
        let root = disk_root();
        tree.set_root(root.clone());
        let (sender, _requests) = mpsc::channel();
        tree.sender = sender;
        let (_results, receiver) = mpsc::channel();
        tree.receiver = receiver;
        let _ = draw_tree(&mut tree, &context, Vec::new());
        let position = tree.bounds.center();
        tree.primitives = Arc::new(vec![Primitive {
            id: Some("nested".into()),
            parent_id: Some("directory".into()),
            label: "nested".into(),
            bytes: 50,
            rect: Rect {
                x: position.x - 50.0,
                y: position.y - 50.0,
                width: 100.0,
                height: 100.0,
            },
            depth: 2,
            kind: PrimitiveKind::Container,
        }]);
        let mut events = Vec::new();
        for pressed in [true, false, true, false] {
            let (_, emitted) = draw_tree(
                &mut tree,
                &context,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            events.extend(emitted);
        }
        assert_eq!(tree.path, ["directory", "nested"]);
        assert!(Arc::ptr_eq(tree.root.as_ref().unwrap(), &root));
        assert!(events.iter().any(|event| matches!(
            event, TreemapEvent::Drill(id) if id == "nested"
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            TreemapEvent::FileAction(crate::FileToolsAction::Analyze { .. })
        )));
        assert!(
            context.has_requested_repaint(),
            "navigation schedules its new layout"
        );
    }

    #[test]
    fn disk_breadcrumbs_and_parent_button_navigate_the_cached_tree() {
        let context = egui::Context::default();
        let root = disk_root();
        let mut tree = NestedTreemap::new(context.clone());
        tree.configure_disk(&Default::default());
        tree.set_root(root.clone());
        for (button, expected) in [
            ("⬅ 返回上一级", vec!["directory"]),
            ("directory", vec!["directory"]),
            (r"C:\", vec![]),
        ] {
            tree.navigate_to("nested");
            assert_eq!(tree.path, ["directory", "nested"]);
            let (output, _) = draw_tree(&mut tree, &context, Vec::new());
            let position = output
                .shapes
                .iter()
                .find_map(|clipped| {
                    let egui::Shape::Text(text) = &clipped.shape else {
                        return None;
                    };
                    (text.galley.text() == button)
                        .then(|| egui::Rect::from_min_size(text.pos, text.galley.size()).center())
                })
                .expect("navigation button");
            for pressed in [true, false] {
                let (_, events) = draw_tree(
                    &mut tree,
                    &context,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
                assert!(!events.iter().any(|event| matches!(
                    event,
                    TreemapEvent::FileAction(crate::FileToolsAction::Analyze { .. })
                )));
            }
            assert_eq!(tree.path, expected);
            assert!(Arc::ptr_eq(tree.root.as_ref().unwrap(), &root));
        }
    }

    #[test]
    fn refreshing_the_same_tree_keeps_navigation_but_new_scan_resets_it() {
        let mut tree = NestedTreemap::new(egui::Context::default());
        let root = disk_root();
        tree.set_root(root.clone());
        tree.navigate_to("nested");
        tree.update_root(root.clone());
        assert_eq!(tree.path, ["directory", "nested"]);
        tree.set_root(root);
        assert!(tree.path.is_empty());
        tree.navigate_to("file");
        assert!(
            tree.path.is_empty(),
            "leaf files cannot become navigation roots"
        );
    }

    #[test]
    fn completed_layouts_survive_newer_pending_requests_and_reject_previous_roots() {
        let context = egui::Context::default();
        let mut tree = NestedTreemap::new(context.clone());
        let (requests, _) = mpsc::channel();
        tree.sender = requests;
        let (results, receiver) = mpsc::channel();
        tree.receiver = receiver;
        let root = disk_root();
        tree.set_root(root.clone());
        tree.generation = 3;
        let primitives = layout_request(&Request {
            generation: 2,
            root: root.clone(),
            subpath: None,
            bounds: Rect {
                width: 800.0,
                height: 600.0,
                ..Default::default()
            },
            depth: 3,
            filter: String::new(),
            min_area: 0.0,
        });
        assert!(!primitives.is_empty());
        results.send((1, Vec::new())).unwrap();
        results.send((2, primitives.clone())).unwrap();
        let _ = draw_tree(&mut tree, &context, Vec::new());
        assert!(
            !tree.primitives.is_empty(),
            "completed layout must not starve behind requests"
        );
        tree.set_root(root);
        tree.dirty = false;
        tree.refresh_if_empty();
        assert!(
            tree.dirty,
            "returning to an empty canvas schedules a new layout"
        );
        results.send((2, primitives)).unwrap();
        let _ = draw_tree(&mut tree, &context, Vec::new());
        assert!(
            tree.primitives.is_empty(),
            "old scan results cannot replace a new root"
        );
    }

    #[test]
    fn shared_root_layout_resolves_only_the_requested_subtree() {
        let root = Arc::new(Node {
            children: vec![
                Node { children: vec![leaf("nested", 50)], ..leaf("directory", 50) },
                leaf("sibling", 50),
            ],
            ..leaf("root", 100)
        });
        let request = Request {
            generation: 1,
            root: root.clone(),
            subpath: Some("directory".into()),
            bounds: Rect { width: 800.0, height: 600.0, ..Default::default() },
            depth: 3,
            filter: String::new(),
            min_area: 0.0,
        };
        let output = layout_request(&request);
        assert!(output.iter().any(|item| item.id.as_deref() == Some("nested")));
        assert!(!output.iter().any(|item| item.id.as_deref() == Some("sibling")));
        assert!(Arc::ptr_eq(&root, &request.root));
    }

    #[test]
    fn colors_survive_resize_aggregation_and_sibling_reordering() {
        let mut processes = vec![leaf("process:large", 500)];
        processes.extend((0..30).map(|index| leaf(&format!("process:{index}"), 1)));
        let root = Node {
            children: vec![
                Node {
                    children: processes,
                    ..leaf("processes", 530)
                },
                leaf("cache", 100),
                leaf("pools", 100),
                leaf("modified", 100),
                leaf("free", 100),
            ],
            ..leaf("physical", 930)
        };
        let options = LayoutOptions {
            min_area: MIN_BLOCK_AREA,
            ..Default::default()
        };
        let bounds = Rect {
            width: 1600.0,
            height: 900.0,
            ..Default::default()
        };
        let large = treemap::layout(&root, bounds, options);
        let small = treemap::layout(
            &root,
            Rect {
                width: 350.0,
                height: 240.0,
                ..Default::default()
            },
            options,
        );
        assert!(large.len() > small.len());
        let reference = colors(&large);
        for (id, color) in colors(&small) {
            assert_eq!(reference.get(&id), Some(&color), "{id}");
        }
        for (id, hue) in [
            ("processes", 0),
            ("cache", 1),
            ("pools", 2),
            ("modified", 3),
            ("free", 7),
        ] {
            assert_eq!(reference[id].0, hue);
        }
        let mut reordered = root;
        reordered.children.reverse();
        reordered.children.last_mut().unwrap().children.reverse();
        assert_eq!(
            reference,
            colors(&treemap::layout(&reordered, bounds, options))
        );
    }

    #[test]
    fn deep_nodes_inherit_hue_and_keep_their_identity_based_shade() {
        let root = Node {
            children: vec![Node {
                children: vec![Node {
                    children: vec![leaf("region", 100)],
                    ..leaf("process:42", 100)
                }],
                ..leaf("processes", 100)
            }],
            ..leaf("physical", 100)
        };
        let primitives = treemap::layout(
            &root,
            Rect {
                width: 800.0,
                height: 600.0,
                ..Default::default()
            },
            LayoutOptions::default(),
        );
        let reference = colors(&primitives);
        assert_eq!(reference["region"].0, reference["process:42"].0);
        assert_eq!(
            reference["region"].1,
            ((stable_hash("region") >> 32) % 4) as usize
        );
    }

    #[test]
    fn category_labels_and_missing_ids_use_deterministic_colors() {
        for (label, hue) in [
            ("standby", 1),
            ("kernel", 2),
            ("进程工作集", 0),
            ("系统缓存", 1),
            ("内核与驱动", 2),
            ("已修改内存", 3),
            ("可用内存", 7),
        ] {
            let primitive = Primitive {
                id: None,
                parent_id: Some("physical".into()),
                label: label.into(),
                bytes: 100,
                rect: Rect::default(),
                depth: 1,
                kind: PrimitiveKind::Leaf,
            };
            assert_eq!(assign_hues(&[primitive])[0].unwrap().0, hue);
        }
    }
}
