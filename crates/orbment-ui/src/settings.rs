use crate::{
    main_host::SettingsEvent,
    theme::{self, IconKind, Tone},
};
use egui::{Align, Align2, Context, FontId, Layout, Margin, RichText, Sense, Ui, vec2};
use orbment_core::{
    config::{
        AppSoundRule, AudioDeviceSelection, AudioProfile, MOUSE_GESTURE_ACTIONS, ProcessFamily,
        QuickAskConfig, QuickAskPrompt, QuickAskProvider, ThinkingEffort,
    },
    i18n::{quick_ask_provider_label, thinking_effort_label, tr},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, SystemTime},
};

/// Navigation groups in semantic order; modules missing from the config are skipped.
const NAV_GROUPS: &[(&str, &[&str])] = &[
    ("common", &["app", "clipboard", "vision", "file_search"]),
    ("devices", &["audio", "monitor", "utilities"]),
    ("system", &["memory", "disk_analyzer"]),
    ("ai", &["quick_ask", "mcp"]),
];
/// Always-present pages that are not config modules.
const INFO_PAGES: &[&str] = &["history", "diagnostics", "about"];
const RESERVED_MODULES: &[&str] = &["hotkey_engine"];

const MONITOR_DISPLAY_MODES: [(bool, bool, &str); 4] = [
    (true, false, "settings.monitor_display_taskbar"),
    (false, true, "settings.monitor_display_floating"),
    (true, true, "settings.monitor_display_both"),
    (false, false, "settings.monitor_display_hidden"),
];

type FormGroup = (&'static str, &'static [&'static str]);
type FormTab = (&'static str, &'static [FormGroup]);

/// Authored information architecture; field order follows the user's workflow, never JSON order.
fn form_tabs(module: &str) -> &'static [FormTab] {
    match module {
        "app" => &[
            (
                "开机自启与运行状态",
                &[(
                    "启动与后台运行",
                    &[
                        "autostart",
                        "language",
                        "idle_destroy_seconds",
                        "tray_middle_click_action",
                    ],
                )],
            ),
            (
                "OCR 与日志维护",
                &[
                    ("文字识别", &["ocr_engine_tier", "ocr_idle_unload_seconds"]),
                    ("日志维护", &["log_level", "log_retention_days"]),
                ],
            ),
        ],
        "audio" => &[
            (
                "设备与快捷键",
                &[
                    (
                        "切换快捷键",
                        &[
                            "playback_hotkey",
                            "recording_hotkey",
                            "mute_recording_hotkey",
                        ],
                    ),
                    (
                        "设备切换规则",
                        &[
                            "cycle_mode",
                            "change_communications",
                            "switch_foreground_program",
                            "keep_volume_enabled",
                            "auto_add_new_devices",
                        ],
                    ),
                    (
                        "可选：音量旋钮",
                        &["knob_remap.intercept_volume_mute", "knob_remap.action"],
                    ),
                ],
            ),
            (
                "通知横幅与托盘",
                &[
                    (
                        "托盘操作",
                        &[
                            "tray_icon.enabled",
                            "quick_menu_enabled",
                            "switch_icon_mode",
                            "icon_double_click",
                            "tooltip_info",
                        ],
                    ),
                    (
                        "通知触发",
                        &[
                            "switch_device_notification",
                            "switch_profile_notification",
                            "mic_mute_notification",
                            "mic_mute_banner_mode",
                            "mic_unmute_banner_mode",
                        ],
                    ),
                    (
                        "横幅外观与位置",
                        &[
                            "banner_position",
                            "custom_banner_position",
                            "banner_opacity_pct",
                            "banner_display_info",
                            "banner_on_screen_time_ms",
                            "max_number_notifications",
                            "notify_using_primary_screen",
                        ],
                    ),
                ],
            ),
            (
                "场景配置与进程路由",
                &[
                    ("场景配置", &["profiles"]),
                    ("应用音频路由", &["app_sound_rules"]),
                ],
            ),
        ],
        "clipboard" => &[
            (
                "快捷键与窗口行为",
                &[
                    (
                        "呼出与快速粘贴",
                        &[
                            "open_clipboard_hotkey",
                            "open_preference_hotkey",
                            "replace_win_v",
                            "quick_paste.enabled",
                            "quick_paste.prefix",
                        ],
                    ),
                    (
                        "窗口位置与打开行为",
                        &[
                            "window.position",
                            "window.pos_x",
                            "window.pos_y",
                            "window.dock_edge",
                            "window.scroll_to_top_on_open",
                            "window.select_range_on_open",
                            "window.select_category_on_open",
                            "window.select_group_on_open",
                        ],
                    ),
                    (
                        "搜索与后台资源",
                        &[
                            "search.default_focus",
                            "search.clear_on_hide",
                            "window.lightweight_mode",
                            "window.idle_destroy_seconds",
                        ],
                    ),
                    (
                        "复制与粘贴",
                        &[
                            "content.auto_paste",
                            "content.middle_click",
                            "content.copy_plain",
                            "content.paste_plain",
                            "content.copy_then_hide_window",
                            "content.paste_files_as_path",
                            "content.update_on_reuse",
                            "feedback.copy_sound",
                        ],
                    ),
                ],
            ),
            (
                "捕获规则与隐私脱敏",
                &[
                    (
                        "内容捕获",
                        &[
                            "capture.text",
                            "capture.html",
                            "capture.rtf",
                            "capture.image",
                            "capture.files",
                            "capture.max_text_mb",
                            "capture.max_image_mb",
                            "capture.image_ocr",
                            "capture.order",
                        ],
                    ),
                    (
                        "隐私保护",
                        &[
                            "sensitive.collect_secrets",
                            "sensitive.redact_secrets",
                            "filters.excluded_app_ids",
                        ],
                    ),
                ],
            ),
            (
                "卡片外观与历史清理",
                &[
                    (
                        "卡片内容与预览",
                        &[
                            "display.text_max_lines",
                            "display.file_max_count",
                            "display.image_max_height",
                            "content.show_original_preview",
                            "preview.space_enabled",
                            "content.sort",
                            "content.auto_favorite",
                        ],
                    ),
                    (
                        "历史保留与清理",
                        &[
                            "history.retention_value",
                            "history.unit",
                            "history.max_count",
                            "history.cleanup_interval_hours",
                        ],
                    ),
                    (
                        "删除保护",
                        &[
                            "content.delete_confirm",
                            "content.delete_favorite_items",
                            "content.delete_favorite_items_confirm",
                            "content.delete_favorite_items_only_in_fav_group",
                            "content.delete_pinned_items",
                            "content.delete_pinned_items_confirm",
                        ],
                    ),
                ],
            ),
        ],
        "vision" => &[
            (
                "快捷键与鼠标手势",
                &[
                    (
                        "截图、贴图与录屏快捷键",
                        &["allow_simple_hotkey", "hotkeys"],
                    ),
                    (
                        "鼠标手势",
                        &[
                            "global_mouse.bindings",
                            "global_mouse.show_mask_on_drag",
                            "global_mouse.drag_threshold",
                        ],
                    ),
                    (
                        "游戏与桌面集成",
                        &[
                            "disable_in_fullscreen_game",
                            "ignore_process_list",
                            "desktop_toolbar_mode",
                        ],
                    ),
                ],
            ),
            (
                "截图与贴图设置",
                &[
                    (
                        "截图选区",
                        &[
                            "screenshot.select_mode",
                            "screenshot.keep_mode",
                            "screenshot.confirm_on_close_by_key",
                            "screenshot.enable_round_rect",
                            "screenshot.ratio",
                            "screenshot.unselected_mask_color",
                            "screenshot.border_color",
                            "screenshot.action_bar_buttons",
                        ],
                    ),
                    (
                        "贴图与历史",
                        &[
                            "pin.confirmation_flags",
                            "pin.text_selectable_default",
                            "pin.double_click_text_select",
                            "pin.show_text_popup_menu",
                            "pin.default_opacity_pct",
                            "pin.stays_on_top",
                            "pin.show_taskbar_icon_when_unpinned",
                            "pin.restore_unclosed_on_startup",
                            "pin.max_history_pins",
                        ],
                    ),
                    (
                        "文字识别与滚动截图",
                        &[
                            "ocr.remove_spaces",
                            "ocr.punctuation_type",
                            "ocr.vertical_reading",
                            "ocr.copy_text_confirmation",
                            "long_shot.auto_crop_on_scroll_back",
                        ],
                    ),
                ],
            ),
            (
                "标注工具样式",
                &[
                    (
                        "工具与公共颜色",
                        &["mark.edit_item_order", "mark.common_color_argb"],
                    ),
                    (
                        "线条与形状",
                        &[
                            "mark.arrow",
                            "mark.broken_line",
                            "mark.geometry",
                            "mark.pencil",
                            "mark.marker",
                        ],
                    ),
                    (
                        "文字、序号与水印",
                        &["mark.text", "mark.serial", "mark.watermark"],
                    ),
                    (
                        "马赛克、放大镜与高亮",
                        &[
                            "mark.mosaic",
                            "mark.auto_mosaic",
                            "mark.magnifier",
                            "mark.high_light",
                        ],
                    ),
                ],
            ),
            (
                "录屏与自动保存",
                &[
                    (
                        "录屏编码",
                        &[
                            "movie.fps",
                            "movie.encode_format",
                            "movie.toolbar_display_type",
                            "movie.exit_record_after_encode",
                        ],
                    ),
                    ("鼠标轨迹与按键显示", &["movie.key_mouse"]),
                    (
                        "文件保存",
                        &[
                            "save.manual_save_pattern",
                            "save.auto_save_types_mask",
                            "save.save_quality",
                        ],
                    ),
                ],
            ),
        ],
        "file_search" => &[
            (
                "搜索后端与快捷键",
                &[
                    (
                        "快捷键与搜索引擎",
                        &[
                            "hotkey",
                            "backend",
                            "everything_exe_path",
                            "auto_spawn_headless",
                        ],
                    ),
                    ("后台资源", &["residency_mode", "idle_release_seconds"]),
                ],
            ),
            (
                "索引范围与排除目录",
                &[
                    ("索引范围", &["included_drives"]),
                    ("排除目录", &["exclude_paths"]),
                ],
            ),
        ],
        "monitor" => &[
            ("任务栏窗口设置", &[]),
            (
                "主窗口(悬浮窗)设置",
                &[
                    (
                        "悬浮窗外观",
                        &[
                            "floating.font_name",
                            "floating.size",
                            "floating.style",
                            "floating.specify_each_item_color",
                            "floating.text_colors",
                            "floating.opacity_pct",
                            "floating.skin_selected",
                            "floating.allow_skin_cover_font",
                            "floating.allow_skin_cover_text",
                        ],
                    ),
                    (
                        "窗口位置与交互",
                        &[
                            "floating.pos_x",
                            "floating.pos_y",
                            "floating.always_on_top",
                            "floating.lock_position",
                            "floating.mouse_penetrate",
                            "floating.hide_on_fullscreen",
                            "floating.allow_out_of_border",
                            "floating.snap",
                            "double_click_action",
                            "double_click_exe",
                        ],
                    ),
                    (
                        "显示内容与单位",
                        &[
                            "floating.show_cpu_memory",
                            "floating.swap_up_down",
                            "labels",
                            "memory_display_mode",
                            "unit_byte",
                            "speed_unit",
                            "speed_short_mode",
                            "separate_value_unit_with_space",
                            "hide_unit",
                            "hide_percent",
                        ],
                    ),
                ],
            ),
            (
                "常规与硬件告警",
                &[
                    (
                        "采样与硬件",
                        &[
                            "sample_interval_ms",
                            "cpu_usage_by_times",
                            "hardware_monitor_mask",
                            "hard_disk_name",
                            "cpu_core_name",
                            "pawnio_module_dir",
                        ],
                    ),
                    (
                        "网络连接",
                        &[
                            "adapter_auto_select",
                            "adapter_select_all",
                            "selected_adapter_name",
                            "hidden_adapters",
                        ],
                    ),
                    (
                        "流量与内存告警",
                        &[
                            "alerts.traffic_enabled",
                            "alerts.traffic_value",
                            "alerts.traffic_unit",
                            "alerts.memory_enabled",
                            "alerts.memory_pct",
                        ],
                    ),
                    (
                        "硬件温度告警",
                        &[
                            "alerts.cpu_temperature_enabled",
                            "alerts.cpu_temperature_c",
                            "alerts.gpu_temperature_enabled",
                            "alerts.gpu_temperature_c",
                            "alerts.disk_temperature_enabled",
                            "alerts.disk_temperature_c",
                        ],
                    ),
                ],
            ),
            ("历史流量统计", &[]),
        ],
        "memory" => &[(
            "内存清理设置",
            &[
                (
                    "自动清理规则与清理区域",
                    &[
                        "auto_clean_threshold_enabled",
                        "auto_clean_threshold_pct",
                        "auto_clean_interval_enabled",
                        "auto_clean_interval_mins",
                        "clean_regions_mask",
                        "warn_on_standby_modified",
                        "confirm_before_clean",
                    ],
                ),
                (
                    "托盘图标与快捷键",
                    &[
                        "clean_hotkey",
                        "show_clean_result_notification",
                        "notification_sound",
                        "tray_icon",
                    ],
                ),
                (
                    "进程家族与残留清理",
                    &[
                        "reaper.families",
                        "reaper.close_timeout_seconds",
                        "reaper.show_in_treemap",
                    ],
                ),
            ],
        )],
        "disk_analyzer" => &[(
            "磁盘分析设置",
            &[
                (
                    "树图渲染参数",
                    &[
                        "display_unit",
                        "show_allocated_size",
                        "default_detail_depth",
                        "min_block_pixel_size",
                        "show_free_space",
                        "palette_colors",
                    ],
                ),
                ("扫描与删除安全", &["monitor_fs_changes", "max_top_files"]),
            ],
        )],
        "utilities" => &[(
            "小工具快捷键设置",
            &[
                (
                    "窗口置顶",
                    &["topmost_hotkey", "topmost_target", "topmost_border_color"],
                ),
                (
                    "屏幕取色",
                    &["color_picker_hotkey", "color_format", "color_template"],
                ),
                (
                    "防休眠",
                    &[
                        "keep_awake_hotkey",
                        "keep_awake_mode",
                        "keep_awake_duration",
                        "keep_awake_process_id",
                    ],
                ),
                (
                    "屏幕亮度",
                    &[
                        "brightness_hotkey_up",
                        "brightness_hotkey_down",
                        "brightness_step_pct",
                        "brightness_debounce_ms",
                    ],
                ),
            ],
        )],
        "quick_ask" => &[
            (
                "模型接口与密钥",
                &[(
                    "请求与工具调用",
                    &["max_tokens", "request_timeout_seconds", "max_tool_rounds"],
                )],
            ),
            ("快捷键与预设指令", &[]),
        ],
        "mcp" => &[(
            "外部控制接口",
            &[
                ("命名管道与 MCP 开关", &["enabled"]),
                (
                    "细粒度权限控制",
                    &[
                        "allow_memory_clean",
                        "allow_process_kill",
                        "allow_process_reap",
                        "allow_file_unlock",
                    ],
                ),
            ],
        )],
        _ => &[],
    }
}

/// Maps a page (including functional sub-pages) to the navigation entry that owns it.
fn page_owner(page: &str) -> &str {
    match page {
        "memory_analyzer" | "memory_reaper" | "memory_ports" => "memory",
        "disk_analysis" | "disk_top_files" | "disk_extensions" | "file_unlock" => "disk_analyzer",
        "utilities_status" => "utilities",
        "vision_pins" => "vision",
        other => other,
    }
}

/// Functional disk pages share one `DiskPanel` for the lifetime of the settings window.
pub(crate) fn is_disk_page(page: &str) -> bool {
    matches!(
        page,
        "disk_analysis" | "disk_top_files" | "disk_extensions" | "file_unlock"
    )
}

/// Segmented tabs shown above a module page: (page, icon).
fn module_tabs(module: &str) -> &'static [(&'static str, IconKind)] {
    match module {
        "vision" => &[
            ("vision_pins", IconKind::Pin),
            ("vision", IconKind::Settings),
        ],
        "memory" => &[
            ("memory_analyzer", IconKind::Treemap),
            ("memory_reaper", IconKind::Recycle),
            ("memory_ports", IconKind::Plug),
            ("memory", IconKind::Settings),
        ],
        "disk_analyzer" => &[
            ("disk_analysis", IconKind::Treemap),
            ("disk_top_files", IconKind::Ranking),
            ("disk_extensions", IconKind::PieChart),
            ("file_unlock", IconKind::Unlock),
            ("disk_analyzer", IconKind::Settings),
        ],
        "utilities" => &[
            ("utilities_status", IconKind::Sliders),
            ("utilities", IconKind::Settings),
        ],
        _ => &[],
    }
}

fn tr_or(key: &str, fallback: impl FnOnce() -> String) -> String {
    let text = tr(key);
    if text == key { fallback() } else { text }
}

pub(crate) fn nav_label(entry: &str) -> String {
    tr_or(&format!("settings.nav.{entry}"), || {
        tr(&format!("settings.module.{entry}"))
    })
}

fn page_description(entry: &str) -> String {
    tr_or(&format!("settings.desc.{entry}"), String::new)
}

fn nav_icon(entry: &str) -> IconKind {
    match entry {
        "app" => IconKind::Settings,
        "clipboard" => IconKind::Clipboard,
        "vision" => IconKind::Viewfinder,
        "file_search" => IconKind::Search,
        "audio" => IconKind::Speaker,
        "monitor" => IconKind::LineChart,
        "utilities" => IconKind::Wrench,
        "memory" => IconKind::Chip,
        "disk_analyzer" => IconKind::Disk,
        "quick_ask" => IconKind::Chat,
        "mcp" => IconKind::Terminal,
        "history" => IconKind::BarChart,
        "diagnostics" => IconKind::Pulse,
        "about" => IconKind::Info,
        _ => IconKind::Puzzle,
    }
}

type PinPreviewKey = (PathBuf, Option<SystemTime>);
type PinPreviewResult = (PinPreviewKey, Option<Result<egui::ColorImage, String>>);

struct PinPreviewRequest {
    path: PathBuf,
    previous: Option<PinPreviewKey>,
    scale: f32,
}

struct PinPreviews {
    requests: mpsc::SyncSender<PinPreviewRequest>,
    results: mpsc::Receiver<PinPreviewResult>,
    pending: BTreeSet<PathBuf>,
    checked: BTreeSet<PathBuf>,
    current: BTreeMap<PathBuf, PinPreviewKey>,
    textures: BTreeMap<PinPreviewKey, Result<egui::TextureHandle, String>>,
    _context: Arc<Context>,
}

impl PinPreviews {
    fn new(context: Context) -> std::io::Result<Self> {
        let (requests, receiver) = mpsc::sync_channel::<PinPreviewRequest>(16);
        let (sender, results) = mpsc::sync_channel(2);
        let context = Arc::new(context);
        let weak = Arc::downgrade(&context);
        std::thread::Builder::new()
            .name("orbment-pin-previews".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    if weak.strong_count() == 0 {
                        break;
                    }
                    if sender.send(load_pin_preview(request)).is_err() {
                        break;
                    }
                    if let Some(context) = weak.upgrade() {
                        context.request_repaint();
                    }
                }
            })?;
        Ok(Self {
            requests,
            results,
            pending: BTreeSet::new(),
            checked: BTreeSet::new(),
            current: BTreeMap::new(),
            textures: BTreeMap::new(),
            _context: context,
        })
    }

    fn refresh(&mut self, history: &Value) {
        let paths: BTreeSet<_> = history
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["image_path"].as_str().map(PathBuf::from))
            .collect();
        self.current.retain(|path, _| paths.contains(path));
        self.textures.retain(|(path, _), _| paths.contains(path));
        self.pending.retain(|path| paths.contains(path));
        // Re-stat on the worker; unchanged files keep their GPU textures.
        self.checked.clear();
    }

    fn texture(
        &mut self,
        context: &Context,
        path: &Path,
    ) -> Option<Result<egui::TextureHandle, String>> {
        while let Ok((key, result)) = self.results.try_recv() {
            if !self.pending.remove(&key.0) {
                continue;
            }
            self.checked.insert(key.0.clone());
            if let Some(result) = result {
                self.textures.retain(|(path, _), _| *path != key.0);
                if self.textures.len() >= 48 {
                    self.textures.pop_first();
                }
                self.textures.insert(
                    key.clone(),
                    result.map(|image| {
                        context.load_texture(
                            "vision-pin-preview",
                            image,
                            egui::TextureOptions::LINEAR,
                        )
                    }),
                );
            }
            self.current.insert(key.0.clone(), key);
        }
        let key = self
            .current
            .get(path)
            .filter(|key| self.textures.contains_key(*key))
            .cloned();
        if (!self.checked.contains(path) || key.is_none()) && !self.pending.contains(path) {
            let request = PinPreviewRequest {
                path: path.to_owned(),
                previous: key.clone(),
                scale: context.pixels_per_point().max(2.0),
            };
            if self.requests.try_send(request).is_ok() {
                self.pending.insert(path.to_owned());
            }
        }
        key.and_then(|key| self.textures.get(&key).cloned())
    }
}

fn load_pin_preview(request: PinPreviewRequest) -> PinPreviewResult {
    let modified = std::fs::metadata(&request.path).and_then(|metadata| metadata.modified());
    let key = (request.path.clone(), modified.as_ref().ok().copied());
    if modified.is_ok() && request.previous.as_ref() == Some(&key) {
        return (key, None);
    }
    let result = (|| -> Result<egui::ColorImage, String> {
        modified.map_err(|error| error.to_string())?;
        let mut reader =
            image::ImageReader::open(&request.path).map_err(|error| error.to_string())?;
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|error| error.to_string())?
            .thumbnail(
                (160.0 * request.scale) as u32,
                (110.0 * request.scale) as u32,
            )
            .to_rgba8();
        Ok(egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        ))
    })();
    (key, Some(result))
}

#[derive(Default)]
struct QuickAskCredentials {
    identity: Option<(String, QuickAskProvider, String)>,
    secret: String,
    saved: Option<bool>,
    status: String,
    error: bool,
}

impl QuickAskCredentials {
    fn select(&mut self, config: &QuickAskConfig) {
        let identity = (
            config.active_channel.clone(),
            config.provider,
            config.credential_target.clone(),
        );
        if self.identity.as_ref() == Some(&identity) {
            return;
        }
        self.identity = Some(identity);
        self.secret.clear();
        self.status.clear();
        self.reload(config);
    }

    fn reload(&mut self, config: &QuickAskConfig) {
        match orbment_core::credentials::read_quick_ask_api_key(config) {
            Ok(key) => {
                self.saved = Some(key.is_some_and(|key| !key.trim().is_empty()));
                self.error = false;
            }
            Err(error) => {
                self.saved = None;
                self.status = error.to_string();
                self.error = true;
            }
        }
    }
}

struct QuickAskModelRequest {
    provider: QuickAskProvider,
    base_url: String,
    credential_target: String,
    result: mpsc::Receiver<Result<Vec<String>, String>>,
}

#[derive(Default)]
struct QuickAskModels {
    pending: Option<QuickAskModelRequest>,
    error: Option<String>,
    fetched: Option<usize>,
}

struct QuickAskPromptRow {
    key: usize,
    prompt: QuickAskPrompt,
}

enum QuickAskPromptAction {
    MoveUp(usize),
    MoveDown(usize),
    Delete(usize),
}

struct QuickAskPromptDraft {
    baseline: Value,
    rows: Vec<QuickAskPromptRow>,
    next_key: usize,
}

impl QuickAskPromptDraft {
    fn new(value: &Value) -> Self {
        let mut draft = Self {
            baseline: value.clone(),
            rows: Vec::new(),
            next_key: 0,
        };
        draft.reset();
        draft
    }

    fn sync(&mut self, value: &Value) {
        if self.baseline != *value {
            self.baseline = value.clone();
            self.reset();
        }
    }

    fn push(&mut self, prompt: QuickAskPrompt) {
        self.rows.push(QuickAskPromptRow {
            key: self.next_key,
            prompt,
        });
        self.next_key += 1;
    }

    fn replace(&mut self, prompts: Vec<QuickAskPrompt>) {
        self.rows.clear();
        for prompt in prompts {
            self.push(prompt);
        }
    }

    fn add(&mut self) {
        let mut number = 1;
        while self
            .rows
            .iter()
            .any(|row| row.prompt.id.trim() == format!("prompt-{number}"))
        {
            number += 1;
        }
        self.push(QuickAskPrompt {
            id: format!("prompt-{number}"),
            label: tr("quick_ask.new_prompt"),
            prompt: String::new(),
        });
    }

    fn edit_row(&mut self, action: QuickAskPromptAction) {
        match action {
            QuickAskPromptAction::MoveUp(index) => self.rows.swap(index, index - 1),
            QuickAskPromptAction::MoveDown(index) => self.rows.swap(index, index + 1),
            QuickAskPromptAction::Delete(index) => {
                self.rows.remove(index);
            }
        }
    }

    fn restore_defaults(&mut self) {
        self.replace(QuickAskConfig::default().prompts);
    }

    fn reset(&mut self) {
        self.replace(
            serde_json::from_value(self.baseline.clone()).expect("typed QuickAsk prompts"),
        );
    }

    fn value(&self) -> Value {
        serde_json::json!(self.rows.iter().map(|row| &row.prompt).collect::<Vec<_>>())
    }

    fn valid(&self) -> bool {
        let mut ids = BTreeSet::new();
        self.rows.iter().all(|row| {
            let prompt = &row.prompt;
            !prompt.id.trim().is_empty()
                && ids.insert(prompt.id.trim())
                && !prompt.label.trim().is_empty()
                && !prompt.prompt.trim().is_empty()
        })
    }

    fn apply(&mut self, value: &mut Value) -> bool {
        let proposed = self.value();
        if !self.valid() || proposed == *value {
            return false;
        }
        *value = proposed.clone();
        self.baseline = proposed;
        true
    }
}

pub(crate) struct SettingsView {
    pub config: Value,
    committed: Value,
    pub diagnostics: Vec<String>,
    pub monitor_notices: Vec<String>,
    pub selected: String,
    pub history: Value,
    pub audio_devices: Value,
    pub pin_history: Value,
    pin_previews: Option<Result<PinPreviews, String>>,
    pub analyzer: Option<crate::analyzer_panel::AnalyzerPanel>,
    pub disk: Option<crate::file_tools::DiskPanel>,
    pub file_tools_state: crate::SharedFileToolsState,
    pub pending_port: Option<u16>,
    pub utilities_status: Value,
    pub(crate) active_page: String,
    pending_clean_mask: Option<u64>,
    dismiss_clean_warning: bool,
    pub(crate) search: String,
    quick_ask_credentials: QuickAskCredentials,
    quick_ask_models: BTreeMap<String, QuickAskModels>,
    quick_ask_prompts: Option<QuickAskPromptDraft>,
    subpages: BTreeMap<String, usize>,
    pub(crate) elevated: bool,
}

impl SettingsView {
    pub fn new(config: Value) -> Self {
        Self {
            committed: config.clone(),
            config,
            diagnostics: Vec::new(),
            monitor_notices: Vec::new(),
            selected: "app".into(),
            search: String::new(),
            history: Value::Array(Vec::new()),
            audio_devices: Value::Null,
            pin_history: Value::Array(Vec::new()),
            pin_previews: None,
            analyzer: None,
            disk: None,
            file_tools_state: Default::default(),
            pending_port: None,
            utilities_status: Value::Null,
            active_page: String::new(),
            pending_clean_mask: None,
            dismiss_clean_warning: false,
            quick_ask_credentials: QuickAskCredentials::default(),
            quick_ask_models: BTreeMap::new(),
            quick_ask_prompts: None,
            subpages: BTreeMap::new(),
            elevated: orbment_core::single_instance::is_elevated().unwrap_or(false),
        }
    }

    pub(crate) fn select_form_tab(&mut self, module: &str, index: usize) {
        self.selected = module.into();
        self.subpages.insert(module.into(), index);
        self.search.clear();
    }

    pub fn set_pin_history(&mut self, history: Value) -> bool {
        if let Some(Ok(previews)) = &mut self.pin_previews {
            previews.refresh(&history);
        }
        if self.pin_history == history {
            return self.pin_previews.is_some();
        }
        self.pin_history = history;
        true
    }

    pub fn sync_config(&mut self, accepted: Value) -> bool {
        if let Some(panel) = &mut self.disk
            && let Ok(config) = serde_json::from_value(accepted["disk_analyzer"].clone())
        {
            panel.sync_config(config);
        }
        if accepted["memory"]["enabled"].as_bool() != Some(true) {
            self.analyzer = None;
        } else if let Some(panel) = &mut self.analyzer {
            panel.sync_config(&accepted);
        }
        let mut merged = accepted.clone();
        if let Some(modules) = self.config.as_object() {
            for (module, fields) in modules {
                if module == "quick_ask" && self.committed.get(module) != Some(fields) {
                    // Provider and channel snapshots are one transaction, including local drafts.
                    merged[module] = fields.clone();
                    continue;
                }
                if let Some(fields) = fields.as_object() {
                    for (field, value) in fields {
                        if self
                            .committed
                            .get(module)
                            .and_then(|values| values.get(field))
                            != Some(value)
                        {
                            merged[module][field] = value.clone();
                        }
                    }
                }
            }
        }
        self.committed = accepted;
        let changed = self.config != merged;
        self.config = merged;
        changed
    }

    pub fn flush_pending(&mut self, events: &mut Vec<SettingsEvent>) {
        if let Some(modules) = self.config.as_object() {
            for (module, fields) in modules {
                if module == "quick_ask" {
                    continue;
                }
                if let Some(fields) = fields.as_object() {
                    for (field, value) in fields {
                        if self
                            .committed
                            .get(module)
                            .and_then(|values| values.get(field))
                            != Some(value)
                        {
                            events.push(SettingsEvent::ConfigChanged {
                                path: format!("{module}.{field}"),
                                value: value.clone(),
                            });
                            self.committed[module][field] = value.clone();
                        }
                    }
                }
            }
        }
        if let Some(config) = self.config.get("quick_ask").cloned() {
            self.commit_quick_ask(config, events);
        }
    }

    fn module_enabled(&self, module: &str) -> Option<bool> {
        self.config
            .get(module)
            .and_then(|values| values.get("enabled"))
            .and_then(Value::as_bool)
    }

    /// The page opened by a navigation entry: the first functional tab while the module runs.
    fn entry_page(&self, entry: &str) -> String {
        match module_tabs(entry).first() {
            Some((page, _)) if self.module_enabled(entry) != Some(false) => (*page).into(),
            _ => entry.into(),
        }
    }

    fn nav_groups(&self) -> Vec<(String, Vec<String>)> {
        let present = |module: &str| self.config.get(module).is_some_and(Value::is_object);
        let mut groups: Vec<(String, Vec<String>)> = NAV_GROUPS
            .iter()
            .map(|(group, modules)| {
                (
                    (*group).to_owned(),
                    modules
                        .iter()
                        .filter(|module| present(module))
                        .map(|module| (*module).to_owned())
                        .collect(),
                )
            })
            .collect();
        let grouped: Vec<&str> = NAV_GROUPS
            .iter()
            .flat_map(|(_, modules)| modules.iter().copied())
            .collect();
        let mut more: Vec<String> = self
            .config
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(module, value)| {
                value.is_object()
                    && !grouped.contains(&module.as_str())
                    && !RESERVED_MODULES.contains(&module.as_str())
            })
            .map(|(module, _)| module.clone())
            .collect();
        more.extend(
            RESERVED_MODULES
                .iter()
                .filter(|module| present(module))
                .map(|module| (*module).to_owned()),
        );
        // Secondary modules only surface for a matching search (or while one is open),
        // keeping every primary entry on one screen.
        let search = self.search.trim().to_lowercase();
        let owner = page_owner(&self.selected);
        more.retain(|module| {
            module == owner
                || (!search.is_empty()
                    && format!("{} {module}", nav_label(module))
                        .to_lowercase()
                        .contains(&search))
        });
        groups.push(("more".into(), more));
        groups.push((
            "stats".into(),
            INFO_PAGES.iter().map(|page| (*page).to_owned()).collect(),
        ));
        groups.retain(|(_, entries)| !entries.is_empty());
        groups
    }

    fn draw_navigation(&mut self, context: &Context, events: &mut Vec<SettingsEvent>) {
        egui::SidePanel::left("settings_navigation")
            .resizable(false)
            .show_separator_line(true)
            .exact_width(242.0)
            .frame(egui::Frame::new().fill(theme::NAV_BG).inner_margin(Margin {
                left: 12,
                right: 4,
                top: 18,
                bottom: 12,
            }))
            .show(context, |ui| {
                egui::Frame::new()
                    .inner_margin(Margin {
                        right: 8,
                        ..Default::default()
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(6.0);
                            ui.add(
                                egui::Label::new(theme::strong(ui, tr("settings.brand"), 17.0))
                                    .selectable(false),
                            );
                            theme::pill(
                                ui,
                                format!("v{}", env!("CARGO_PKG_VERSION")),
                                theme::ACCENT_LIGHT,
                                theme::ACCENT_SOFT,
                            );
                        });
                        ui.add_space(12.0);
                        theme::search_field(
                            ui,
                            &mut self.search,
                            &tr("settings.search"),
                            ui.available_width(),
                            32.0,
                        );
                        if self.elevated {
                            ui.add_space(8.0);
                            theme::icon_pill(
                                ui,
                                IconKind::Shield,
                                "管理员权限",
                                theme::GREEN,
                                theme::GREEN_BG,
                            );
                        }
                    });
                ui.add_space(12.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin {
                                right: 8,
                                ..Default::default()
                            })
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                for (group, entries) in self.nav_groups() {
                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        ui.add_space(8.0);
                                        ui.add(
                                            egui::Label::new(theme::faint(tr(&format!(
                                                "settings.group.{group}"
                                            ))))
                                            .selectable(false),
                                        );
                                    });
                                    for entry in entries {
                                        self.nav_item(ui, &entry, events);
                                    }
                                }
                                ui.add_space(12.0);
                            });
                    });
            });
    }

    fn nav_item(&mut self, ui: &mut Ui, entry: &str, events: &mut Vec<SettingsEvent>) {
        let selected = page_owner(&self.selected) == entry;
        let enabled = self.module_enabled(entry);
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), 33.0), Sense::click());
        let painter = ui.painter().clone();
        if selected {
            painter.rect_filled(rect, 8, theme::ACCENT_SOFT);
        } else if response.hovered() {
            painter.rect_filled(rect, 8, theme::NAV_HOVER);
        }
        let text_color = if selected {
            theme::TEXT
        } else if enabled == Some(false) {
            theme::TEXT_FAINT
        } else {
            egui::Color32::from_rgb(75, 74, 70)
        };
        theme::draw_icon(
            &painter,
            egui::Rect::from_center_size(
                egui::pos2(rect.left() + 18.0, rect.center().y),
                vec2(16.0, 16.0),
            ),
            nav_icon(entry),
            text_color,
        );
        let switch_rect = enabled.map(|_| {
            egui::Rect::from_center_size(
                egui::pos2(rect.right() - 22.0, rect.center().y),
                vec2(28.0, 16.0),
            )
        });
        let text_left = rect.left() + 34.0;
        let text_right = switch_rect.map_or(rect.right() - 8.0, |rect| rect.left() - 8.0);
        let font = if selected {
            theme::bold(ui.ctx(), 14.0)
        } else {
            FontId::proportional(14.0)
        };
        let galley = egui::WidgetText::from(RichText::new(nav_label(entry)).color(text_color))
            .into_galley(
                ui,
                Some(egui::TextWrapMode::Truncate),
                text_right - text_left,
                font,
            );
        painter.galley(
            egui::pos2(text_left, rect.center().y - galley.size().y * 0.5),
            galley,
            text_color,
        );
        let mut switch_hovered = false;
        if let (Some(mut on), Some(switch_rect)) = (enabled, switch_rect) {
            let mut overlay = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("module_switch", entry))
                    .max_rect(switch_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            let switch = theme::toggle_switch_sized(&mut overlay, &mut on, vec2(28.0, 16.0))
                .on_hover_text(nav_label(entry));
            switch_hovered = switch.contains_pointer();
            if switch.changed() {
                self.set_module_enabled(entry, on, events);
            }
        }
        if response.clicked() && !switch_hovered && !selected {
            self.selected = self.entry_page(entry);
        }
    }

    fn set_module_enabled(&mut self, module: &str, on: bool, events: &mut Vec<SettingsEvent>) {
        self.config[module]["enabled"] = Value::Bool(on);
        events.push(SettingsEvent::ConfigChanged {
            path: format!("{module}.enabled"),
            value: Value::Bool(on),
        });
    }

    fn set_monitor_display(
        &mut self,
        taskbar: bool,
        floating: bool,
        events: &mut Vec<SettingsEvent>,
    ) {
        self.flush_pending(events);
        for (field, enabled) in [("taskbar", taskbar), ("floating", floating)] {
            self.config["monitor"][field]["enabled"] = enabled.into();
            events.push(SettingsEvent::ConfigChanged {
                path: format!("monitor.{field}"),
                value: self.config["monitor"][field].clone(),
            });
        }
        if taskbar != floating {
            self.subpages
                .insert("monitor".into(), usize::from(floating));
            self.search.clear();
        }
    }

    fn draw_monitor_display(&mut self, ui: &mut Ui, events: &mut Vec<SettingsEvent>) {
        let mut selected = (
            self.config["monitor"]["taskbar"]["enabled"]
                .as_bool()
                .unwrap_or(false),
            self.config["monitor"]["floating"]["enabled"]
                .as_bool()
                .unwrap_or(false),
        );
        let title = MONITOR_DISPLAY_MODES
            .iter()
            .find(|&&(taskbar, floating, _)| selected == (taskbar, floating))
            .map(|&(_, _, key)| tr(key))
            .unwrap_or_default();
        let mut changed = false;
        theme::section_card(ui, None, |ui| {
            theme::setting_row(
                ui,
                theme::row_label(tr("settings.monitor_display_location")),
                |ui| {
                    changed = combo(ui, "monitor-display-location", title, |ui| {
                        let mut changed = false;
                        for (taskbar, floating, key) in MONITOR_DISPLAY_MODES {
                            changed |= ui
                                .selectable_value(&mut selected, (taskbar, floating), tr(key))
                                .changed();
                        }
                        changed
                    });
                },
            );
        });
        if changed {
            crate::hotkeys::cancel_recorder(ui.ctx());
            self.set_monitor_display(selected.0, selected.1, events);
        }
    }

    fn is_form_page(&self) -> bool {
        self.config
            .get(&self.selected)
            .is_some_and(Value::is_object)
    }

    fn draw_header(&mut self, context: &Context, events: &mut Vec<SettingsEvent>) {
        egui::TopBottomPanel::top("settings_header")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin {
                left: 36,
                right: 36,
                top: 20,
                bottom: 10,
            }))
            .show(context, |ui| {
                let owner = page_owner(&self.selected).to_owned();
                ui.horizontal(|ui| {
                    theme::page_title(ui, &nav_label(&owner), &page_description(&owner));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(enabled) = self.module_enabled(&owner) {
                            let mut on = enabled;
                            if theme::toggle_switch(ui, &mut on)
                                .on_hover_text(tr("settings.enable_module"))
                                .changed()
                            {
                                self.set_module_enabled(&owner, on, events);
                            }
                            ui.add(
                                egui::Label::new(theme::muted(tr(if enabled {
                                    "settings.module_on"
                                } else {
                                    "settings.module_off"
                                })))
                                .selectable(false),
                            );
                        }
                    });
                });
                if self.selected == "monitor" {
                    ui.add_space(10.0);
                    self.draw_monitor_display(ui, events);
                }
                let tabs = module_tabs(&owner);
                if !tabs.is_empty() {
                    ui.add_space(10.0);
                    let items: Vec<(&str, Option<IconKind>, String)> = tabs
                        .iter()
                        .map(|(page, icon)| {
                            (*page, Some(*icon), tr(&format!("settings.tab.{page}")))
                        })
                        .collect();
                    if let Some(page) = theme::segmented_tabs(ui, &self.selected, &items) {
                        self.selected = page;
                    }
                }
                if self.is_form_page() {
                    let tabs = form_tabs(&owner);
                    if !tabs.is_empty() {
                        ui.add_space(10.0);
                        let index = *self.subpages.get(&owner).unwrap_or(&0);
                        let current = tabs.get(index).unwrap_or(&tabs[0]).0;
                        let items: Vec<_> = tabs
                            .iter()
                            .map(|(title, _)| (*title, None, (*title).into()))
                            .collect();
                        if let Some(title) = theme::segmented_tabs(ui, current, &items) {
                            self.flush_pending(events);
                            crate::hotkeys::cancel_recorder(context);
                            self.subpages.insert(
                                owner,
                                tabs.iter()
                                    .position(|(name, _)| *name == title)
                                    .unwrap_or(0),
                            );
                        }
                    }
                }
            });
    }

    pub fn draw(&mut self, context: &Context, events: &mut Vec<SettingsEvent>) {
        self.poll_quick_ask_models(context, events);
        let previous_page = self.selected.clone();
        self.draw_navigation(context, events);
        self.draw_header(context, events);
        if previous_page != self.selected {
            crate::hotkeys::cancel_recorder(context);
            self.flush_pending(events);
        }
        if self.active_page != self.selected {
            self.analyzer = None;
            if let Some(panel) = &mut self.disk {
                panel.set_page(&self.selected);
            }
            if self.selected == "utilities_status" {
                events.push(SettingsEvent::Action {
                    action: "utilities.display_brightness".into(),
                    params: serde_json::json!({"refresh": true}),
                });
            }
            if self.selected == "vision_pins" {
                if self.module_enabled("vision") == Some(true) {
                    events.push(SettingsEvent::Action {
                        action: "vision.pin_history".into(),
                        params: Value::Null,
                    });
                }
            } else {
                self.pin_previews = None;
            }
            self.active_page = self.selected.clone();
        }
        let analyzer_page = matches!(
            self.selected.as_str(),
            "memory_analyzer" | "memory_reaper" | "memory_ports"
        );
        let memory_enabled = self.config["memory"]["enabled"].as_bool() == Some(true);
        if !analyzer_page || !memory_enabled {
            self.analyzer = None;
        } else if self.analyzer.is_none() {
            self.analyzer = Some(crate::analyzer_panel::AnalyzerPanel::new(
                &self.selected,
                self.config.clone(),
                context.clone(),
            ));
        }
        if let Some(port) = self.pending_port.take()
            && let Some(panel) = &mut self.analyzer
        {
            panel.set_port_filter(port);
        }
        let disk_page = is_disk_page(&self.selected);
        let disk_enabled = self.config["disk_analyzer"]["enabled"].as_bool() == Some(true);
        if !disk_enabled {
            if self.disk.take().is_some() {
                events.push(SettingsEvent::FileTools(crate::FileToolsAction::DiskClosed));
            }
        } else if disk_page && self.disk.is_none() {
            match serde_json::from_value(self.config["disk_analyzer"].clone()) {
                Ok(config) => {
                    self.disk = Some(crate::file_tools::DiskPanel::new(
                        &self.selected,
                        self.file_tools_state.clone(),
                        config,
                        context.clone(),
                    ));
                    events.push(SettingsEvent::FileTools(crate::FileToolsAction::DiskOpened));
                }
                Err(error) => self.diagnostics.push(error.to_string()),
            }
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin {
                left: 36,
                right: 8,
                top: 6,
                bottom: 16,
            }))
            .show(context, |ui| {
                if disk_page && !disk_enabled {
                    theme::page_content_frame().show(ui, |ui| {
                        theme::callout(ui, Tone::Warning, tr("file_tools.disabled"));
                    });
                    return;
                }
                if disk_page && let Some(panel) = &mut self.disk {
                    panel.draw(ui, events);
                    // The panel redirects to the unlock tab after a "query locks" action.
                    if panel.page != self.selected {
                        self.selected = panel.page.clone();
                        self.active_page = self.selected.clone();
                    }
                    return;
                }
                if analyzer_page && !memory_enabled {
                    theme::page_content_frame().show(ui, |ui| {
                        theme::callout(ui, Tone::Warning, tr("analyzer.disabled"));
                    });
                    return;
                }
                if let Some(panel) = &mut self.analyzer {
                    panel.draw(ui, events);
                    return;
                }
                match self.selected.as_str() {
                    "utilities_status" => {
                        draw_utilities_status(ui, &mut self.utilities_status, events)
                    }
                    "vision_pins" => self.draw_pin_history(ui, events),
                    "history" => self.draw_history(ui),
                    "about" => draw_about(ui, events),
                    "diagnostics" => self.draw_diagnostics(ui),
                    _ => self.draw_module_form(ui, events),
                }
            });
        if let Some(mask) = self.pending_clean_mask {
            egui::Window::new(tr("settings.clean_warning_title"))
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .show(context, |ui| {
                    theme::callout(ui, Tone::Warning, tr("settings.clean_warning"));
                    theme::setting_row(
                        ui,
                        theme::row_label(tr("settings.do_not_warn_again")),
                        |ui| {
                            theme::toggle_switch(ui, &mut self.dismiss_clean_warning);
                        },
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add(theme::danger_button(tr("settings.accept_risk")))
                            .clicked()
                        {
                            self.config["memory"]["clean_regions_mask"] = mask.into();
                            events.push(SettingsEvent::ConfigChanged {
                                path: "memory.clean_regions_mask".into(),
                                value: mask.into(),
                            });
                            if self.dismiss_clean_warning {
                                self.config["memory"]["warn_on_standby_modified"] = false.into();
                                events.push(SettingsEvent::ConfigChanged {
                                    path: "memory.warn_on_standby_modified".into(),
                                    value: false.into(),
                                });
                            }
                            self.pending_clean_mask = None;
                        }
                        if ui
                            .add(theme::secondary_button(tr("settings.cancel")))
                            .clicked()
                        {
                            self.pending_clean_mask = None;
                            self.dismiss_clean_warning = false;
                        }
                    });
                });
        }
        for event in events.iter() {
            if let SettingsEvent::ConfigChanged { path, value } = event {
                if let Some((module, field)) = path.split_once('.') {
                    self.config[module][field] = value.clone();
                    self.committed[module][field] = value.clone();
                } else if path == "quick_ask" {
                    self.committed[path] = value.clone();
                }
            }
        }
    }

    fn draw_pin_history(&mut self, ui: &mut Ui, events: &mut Vec<SettingsEvent>) {
        if self.module_enabled("vision") != Some(true) {
            theme::page_content_frame().show(ui, |ui| {
                theme::callout(ui, Tone::Warning, tr("vision.pins.disabled"));
            });
            return;
        }
        let rows = self.pin_history.as_array().cloned().unwrap_or_default();
        egui::ScrollArea::vertical()
            .id_salt("vision_pin_history")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::page_content_frame().show(ui, |ui| {
                    theme::section_card(ui, Some(&tr("vision.pins.actions")), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for (key, action, hotkey, icon) in [
                                (
                                    "capture",
                                    "vision.capture",
                                    Some("capture"),
                                    Some(IconKind::Plus),
                                ),
                                (
                                    "paste",
                                    "vision.pin_last",
                                    Some("pin_last"),
                                    Some(IconKind::Pin),
                                ),
                                (
                                    "restore_last",
                                    "vision.restore_last",
                                    None,
                                    Some(IconKind::Refresh),
                                ),
                                ("close_all", "vision.pin_close_all", None, None),
                            ] {
                                let mut title = tr(&format!("vision.pins.{key}"));
                                if let Some(hotkey) = hotkey
                                    && let Some(chord) = self.config["vision"]["hotkeys"][hotkey]
                                        .as_str()
                                        .or_else(|| {
                                            (hotkey == "pin_last")
                                                .then(|| {
                                                    self.config["vision"]["hotkeys"]["pin"].as_str()
                                                })
                                                .flatten()
                                        })
                                    && !chord.is_empty()
                                {
                                    title.push_str(&format!(" ({chord})"));
                                }
                                let mut button = theme::secondary_button(title);
                                if let Some(icon) = icon {
                                    button = button.icon(icon);
                                }
                                if ui.add(button).clicked() {
                                    events.push(SettingsEvent::Action {
                                        action: action.into(),
                                        params: Value::Null,
                                    });
                                }
                            }
                        });
                        ui.add_space(8.0);
                        ui.add(egui::Label::new(theme::faint(tr("vision.pins.hint"))).wrap());
                    });
                    for (active, title, empty) in [
                        (true, "vision.pins.active", "vision.pins.empty_active"),
                        (false, "vision.pins.closed", "vision.pins.empty_closed"),
                    ] {
                        let entries: Vec<_> = rows
                            .iter()
                            .filter(|entry| entry["is_active"].as_bool() == Some(active))
                            .collect();
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new(format!("{} ({})", tr(title), entries.len()))
                                .size(16.0)
                                .strong(),
                        );
                        ui.add_space(8.0);
                        if entries.is_empty() {
                            theme::section_card(ui, None, |ui| {
                                ui.add(egui::Label::new(theme::faint(tr(empty))).wrap());
                            });
                        }
                        for entry in entries {
                            self.draw_pin_card(ui, entry, active, events);
                        }
                    }
                });
            });
    }

    fn draw_pin_card(
        &mut self,
        ui: &mut Ui,
        entry: &Value,
        active: bool,
        events: &mut Vec<SettingsEvent>,
    ) {
        let Some(id) = entry["id"].as_str() else {
            return;
        };
        ui.push_id(id, |ui| {
            theme::section_card(ui, None, |ui| {
                let title = entry["title"]
                    .as_str()
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| tr("vision.pins.untitled"));
                ui.add(egui::Label::new(RichText::new(title).size(15.0).strong()).wrap());
                ui.add_space(8.0);
                ui.horizontal_top(|ui| {
                    self.draw_pin_preview(ui, entry["image_path"].as_str());
                    ui.vertical(|ui| {
                        let bounds = &entry["bounds"];
                        ui.label(format!(
                            "{:.0} × {:.0}  ·  ({:.0}, {:.0})",
                            bounds["width"].as_f64().unwrap_or(0.0),
                            bounds["height"].as_f64().unwrap_or(0.0),
                            bounds["x"].as_f64().unwrap_or(0.0),
                            bounds["y"].as_f64().unwrap_or(0.0),
                        ));
                        let (label, time) = if active {
                            ("vision.pins.created", &entry["created_at"])
                        } else {
                            ("vision.pins.closed_at", &entry["closed_at"])
                        };
                        ui.add(
                            egui::Label::new(theme::faint(format!(
                                "{} {}",
                                tr(label),
                                time.as_str().unwrap_or("—"),
                            )))
                            .wrap(),
                        );
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            for (label, action, icon) in [
                                ("vision.pins.copy", "copy_image", IconKind::Copy),
                                ("vision.pins.save", "save_image", IconKind::Folder),
                            ] {
                                if ui
                                    .add(theme::secondary_button(tr(label)).icon(icon))
                                    .clicked()
                                {
                                    events.push(SettingsEvent::Action {
                                        action: "vision.pin_action".into(),
                                        params: serde_json::json!({"id":id,"action":action}),
                                    });
                                }
                            }
                            if active {
                                if ui
                                    .add(theme::secondary_button(tr("vision.pins.close")))
                                    .clicked()
                                {
                                    events.push(SettingsEvent::Action {
                                        action: "vision.pin_close".into(),
                                        params: serde_json::json!({"id":id}),
                                    });
                                }
                            } else {
                                for (label, action, icon) in [
                                    ("vision.pins.restore", "vision.pin_restore", IconKind::Pin),
                                    ("vision.pins.delete", "vision.pin_delete", IconKind::Trash),
                                ] {
                                    if ui
                                        .add(theme::secondary_button(tr(label)).icon(icon))
                                        .clicked()
                                    {
                                        events.push(SettingsEvent::Action {
                                            action: action.into(),
                                            params: serde_json::json!({"id":id}),
                                        });
                                    }
                                }
                            }
                        });
                    });
                });
            });
        });
    }

    fn draw_pin_preview(&mut self, ui: &mut Ui, path: Option<&str>) {
        let (rect, response) = ui.allocate_exact_size(vec2(160.0, 110.0), Sense::hover());
        ui.painter().rect_filled(rect, 8.0, theme::CONTROL);
        let Some(path) = path else {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                tr("vision.pins.text_preview"),
                FontId::proportional(13.0),
                theme::TEXT_MUTED,
            );
            return;
        };
        if !ui.is_rect_visible(rect) {
            return;
        }
        let previews = self.pin_previews.get_or_insert_with(|| {
            PinPreviews::new(ui.ctx().clone()).map_err(|error| error.to_string())
        });
        let preview = match previews {
            Ok(previews) => previews.texture(ui.ctx(), Path::new(path)),
            Err(error) => Some(Err(error.clone())),
        };
        match preview {
            Some(Ok(texture)) => {
                let size = texture.size_vec2();
                let scale = (rect.width() / size.x).min(rect.height() / size.y).min(1.0);
                let target = egui::Rect::from_center_size(rect.center(), size * scale);
                ui.painter().image(
                    texture.id(),
                    target,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            Some(Err(error)) => {
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    tr("vision.pins.preview_unavailable"),
                    FontId::proportional(13.0),
                    theme::TEXT_MUTED,
                );
                response.on_hover_text(error.as_str());
            }
            None => {
                ui.put(rect, egui::Spinner::new());
            }
        }
    }

    fn draw_history(&self, ui: &mut Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::page_content_frame().show(ui, |ui| {
                    theme::section_card(ui, Some(&tr("settings.history")), |ui| {
                        let rows = self
                            .history
                            .as_array()
                            .or_else(|| self.history.get("days").and_then(Value::as_array))
                            .cloned()
                            .unwrap_or_default();
                        if rows.is_empty() {
                            ui.label(theme::faint(tr("settings.history_empty")));
                            return;
                        }
                        let column = (ui.available_width() / 3.0).max(80.0);
                        egui::Grid::new("traffic_history")
                            .num_columns(3)
                            .min_col_width(column - 12.0)
                            .spacing([12.0, 10.0])
                            .show(ui, |ui| {
                                for key in ["day_date", "upload_bytes", "download_bytes"] {
                                    ui.label(theme::muted(label(key)));
                                }
                                ui.end_row();
                                for row in rows {
                                    ui.label(
                                        row.get("day_date")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default(),
                                    );
                                    for key in ["upload_bytes", "download_bytes"] {
                                        ui.label(
                                            RichText::new(crate::treemap::bytes(
                                                row.get(key)
                                                    .and_then(Value::as_u64)
                                                    .unwrap_or_default(),
                                            ))
                                            .color(theme::TEXT_MUTED),
                                        );
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                });
            });
    }

    fn draw_diagnostics(&self, ui: &mut Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::page_content_frame().show(ui, |ui| {
                    theme::section_card(ui, Some(&tr("settings.hotkey_card")), |ui| {
                        let conflicts = hotkey_diagnostics(&self.config);
                        if conflicts.is_empty() {
                            ui.horizontal(|ui| {
                                theme::icon(ui, IconKind::Check, theme::SUCCESS);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(tr("settings.no_hotkey_conflicts"))
                                            .size(13.0)
                                            .color(theme::SUCCESS),
                                    )
                                    .wrap(),
                                );
                            });
                        }
                        for conflict in conflicts {
                            theme::callout(ui, Tone::Warning, conflict);
                        }
                    });
                    theme::section_card(ui, Some(&tr("settings.diagnostics_card")), |ui| {
                        for diagnostic in &self.diagnostics {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(diagnostic)
                                        .monospace()
                                        .color(theme::TEXT_MUTED),
                                )
                                .wrap(),
                            );
                        }
                    });
                });
            });
    }

    fn draw_module_form(&mut self, ui: &mut Ui, events: &mut Vec<SettingsEvent>) {
        let module = self.selected.clone();
        let tabs = form_tabs(&module);
        if tabs.is_empty() {
            theme::page_content_frame().show(ui, |ui| {
                theme::callout(ui, Tone::Info, tr("settings.future_module"));
            });
            return;
        }
        let index = *self.subpages.get(&module).unwrap_or(&0);
        if module == "monitor" && index == 3 && self.search.is_empty() {
            self.draw_history(ui);
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt((&module, index))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::page_content_frame().show(ui, |ui| {
                    if module == "monitor" {
                        for notice in &self.monitor_notices {
                            theme::callout(ui, Tone::Warning, notice);
                        }
                    }
                    if index == 0 {
                        self.draw_module_extras(ui, &module, events);
                    }
                    if module == "quick_ask" && index == 1 {
                        self.draw_quick_ask_prompt_settings(ui, events);
                        return;
                    }
                    if module == "app" && index == 0 {
                        theme::section_card(ui, Some("运行权限"), |ui| {
                            theme::setting_row(
                                ui,
                                theme::row_label(if self.elevated {
                                    "系统管理功能已可用"
                                } else {
                                    "部分系统管理功能需要管理员权限"
                                }),
                                |ui| {
                                    if !self.elevated
                                        && ui
                                            .add(
                                                theme::secondary_button("以管理员身份重启")
                                                    .icon(IconKind::Shield),
                                            )
                                            .clicked()
                                    {
                                        events.push(SettingsEvent::Action {
                                            action: "memory.elevate".into(),
                                            params: serde_json::json!({}),
                                        });
                                    }
                                    theme::pill(
                                        ui,
                                        if self.elevated {
                                            "已获管理员权限"
                                        } else {
                                            "普通权限"
                                        },
                                        if self.elevated {
                                            theme::GREEN
                                        } else {
                                            theme::TEXT_MUTED
                                        },
                                        if self.elevated {
                                            theme::GREEN_BG
                                        } else {
                                            theme::CONTROL
                                        },
                                    );
                                },
                            );
                        });
                    }
                    if module == "monitor" && (index == 0 || !self.search.is_empty()) {
                        self.draw_taskbar_settings(ui, events);
                    }
                    for (tab_index, (_, groups)) in tabs.iter().enumerate() {
                        if tab_index != index && self.search.is_empty() {
                            continue;
                        }
                        for (group_index, &(title, fields)) in groups.iter().enumerate() {
                            self.draw_form_group(ui, &module, title, fields, events);
                            if module == "audio"
                                && tab_index == 0
                                && group_index == 0
                                && self.search.is_empty()
                            {
                                self.draw_audio_devices(ui, events);
                            }
                        }
                    }
                    if module == "disk_analyzer" {
                        theme::callout(
                            ui,
                            Tone::Info,
                            "删除与强制解锁均需确认；请先在扫描结果中核对路径和文件大小。",
                        );
                    }
                    if module == "mcp" {
                        theme::callout(
                            ui,
                            Tone::Info,
                            "仅允许本机命名管道访问。高风险操作必须逐项授权，建议保持关闭。",
                        );
                    }
                });
            });
    }

    fn draw_form_group(
        &mut self,
        ui: &mut Ui,
        module: &str,
        title: &str,
        fields: &[&str],
        events: &mut Vec<SettingsEvent>,
    ) {
        let search = self.search.to_lowercase();
        let fields: Vec<_> = fields
            .iter()
            .copied()
            .filter(|field| {
                let pointer = format!("/{module}/{}", field.replace('.', "/"));
                self.config.pointer(&pointer).is_some()
                    && (search.is_empty()
                        || format!(
                            "{title} {} {module}.{field}",
                            label(field.rsplit('.').next().unwrap_or(field))
                        )
                        .to_lowercase()
                        .contains(&search))
            })
            .collect();
        if fields.is_empty() {
            return;
        }
        theme::section_card(ui, Some(title), |ui| {
            for field in fields {
                self.draw_form_field(ui, module, field, events);
            }
        });
    }

    fn draw_form_field(
        &mut self,
        ui: &mut Ui,
        module: &str,
        field: &str,
        events: &mut Vec<SettingsEvent>,
    ) {
        let path = format!("{module}.{field}");
        let pointer = format!("/{module}/{}", field.replace('.', "/"));
        let warn = self.config["memory"]["warn_on_standby_modified"]
            .as_bool()
            .unwrap_or(true);
        let Some(value) = self.config.pointer_mut(&pointer) else {
            return;
        };
        let key = field.rsplit('.').next().unwrap_or(field);
        let mut edits = Vec::new();
        ui.push_id(&path, |ui| {
            edit_field(
                ui,
                key,
                &path,
                value,
                warn,
                &mut self.pending_clean_mask,
                &mut edits,
            );
        });
        if !edits.is_empty() {
            // Config sync and draft merging share the same top-level-field transaction boundary.
            let top = field.split('.').next().unwrap_or(field);
            events.push(SettingsEvent::ConfigChanged {
                path: format!("{module}.{top}"),
                value: self.config[module][top].clone(),
            });
        }
    }

    fn draw_taskbar_settings(&mut self, ui: &mut Ui, events: &mut Vec<SettingsEvent>) {
        let mut taskbar = self.config["monitor"]["taskbar"].clone();
        if taskbar.is_null() {
            return;
        }
        let mut changed = false;
        theme::section_card(ui, Some("颜色和字体"), |ui| {
            ui.columns(2, |columns| {
                changed |= taskbar_fields(&mut columns[0], &mut taskbar, &["font_name"]);
                changed |= taskbar_fields(&mut columns[1], &mut taskbar, &["size"]);
            });
            changed |= taskbar_fields(ui, &mut taskbar, &["style"]);
            let each = taskbar["specify_each_item_color"]
                .as_bool()
                .unwrap_or(false);
            if !each && let Some(colors) = taskbar["text_colors"].as_array_mut() {
                let mut first = colors.first().cloned().unwrap_or(Value::from(0xffffffu32));
                if edit_value(
                    ui,
                    "text_color",
                    "monitor.taskbar.text_colors.0",
                    &mut first,
                ) {
                    colors.fill(first);
                    changed = true;
                }
            }
            changed |= taskbar_fields(ui, &mut taskbar, &["specify_each_item_color"]);
            theme::setting_row(ui, theme::row_label("配色预设"), |ui| {
                ui.menu_button("预设方案…", |ui| {
                    for (title, foreground, background) in [
                        ("浅色任务栏 · 深色文字", 0x202020u32, 0xf3f3f3u32),
                        ("深色任务栏 · 浅色文字", 0xf3f3f3, 0x202020),
                        ("经典双色", 0xffffff, 0x202020),
                    ] {
                        if ui.button(title).clicked() {
                            if let Some(colors) = taskbar["text_colors"].as_array_mut() {
                                for (index, color) in colors.iter_mut().enumerate() {
                                    *color = if title == "经典双色" && index % 2 == 0 {
                                        Value::from(0x60d9fau32)
                                    } else {
                                        Value::from(foreground)
                                    };
                                }
                            }
                            taskbar["back_color"] = background.into();
                            changed = true;
                            ui.close_menu();
                        }
                    }
                });
            });
            if each {
                changed |= taskbar_fields(ui, &mut taskbar, &["text_colors"]);
            }
            changed |= taskbar_fields(
                ui,
                &mut taskbar,
                &[
                    "back_color",
                    "transparent_background",
                    "auto_set_background_color",
                    "auto_adapt_light_theme",
                ],
            );
        });
        ui.columns(2, |columns| {
            theme::section_card(&mut columns[0], Some("显示设置"), |ui| {
                let order_open = expandable_setting_row(ui, "taskbar-order", "显示项目与顺序设置");
                if order_open {
                    changed |= taskbar_order(ui, &mut taskbar);
                }
                let labels_open = expandable_setting_row(ui, "taskbar-labels", "显示文本前缀设置");
                if labels_open && let Some(labels) = taskbar["labels"].as_object_mut() {
                    for key in [
                        "up",
                        "down",
                        "total_speed",
                        "cpu",
                        "cpu_temperature",
                        "cpu_frequency",
                        "gpu",
                        "gpu_temperature",
                        "memory",
                        "disk",
                        "disk_temperature",
                        "today_traffic",
                        "disk_read",
                        "disk_write",
                    ] {
                        if let Some(value) = labels.get_mut(key) {
                            changed |= edit_value(
                                ui,
                                key,
                                &format!("monitor.taskbar.labels.{key}"),
                                value,
                            );
                        }
                    }
                }
                changed |= taskbar_fields(
                    ui,
                    &mut taskbar,
                    &[
                        "speed_short_mode",
                        "value_right_align",
                        "digits_number",
                        "horizontal_arrange",
                    ],
                );
                ui.label(theme::faint("不勾选水平排列时，指标按上下两行成对显示。"));
                changed |= taskbar_fields(
                    ui,
                    &mut taskbar,
                    &[
                        "separate_value_unit_with_space",
                        "show_tool_tip",
                        "memory_display_mode",
                        "item_space",
                        "vertical_margin",
                    ],
                );
            });
            theme::section_card(&mut columns[1], Some("单位设置"), |ui| {
                let mut bytes = taskbar["unit_byte"].as_bool().unwrap_or(true);
                theme::setting_row(ui, theme::row_label("网速单位"), |ui| {
                    changed |= combo(
                        ui,
                        "taskbar-speed-unit-kind",
                        if bytes {
                            "B（字节）"
                        } else {
                            "b（比特）"
                        }
                        .into(),
                        |ui| {
                            let mut changed =
                                ui.selectable_value(&mut bytes, true, "B（字节）").changed();
                            changed |= ui
                                .selectable_value(&mut bytes, false, "b（比特）")
                                .changed();
                            changed
                        },
                    );
                });
                taskbar["unit_byte"] = bytes.into();
                changed |= taskbar_fields(
                    ui,
                    &mut taskbar,
                    &["speed_unit", "hide_unit", "hide_percent"],
                );
            });
        });
        theme::section_card(ui, Some("任务栏窗口位置与双击动作"), |ui| {
            changed |= taskbar_fields(
                ui,
                &mut taskbar,
                &[
                    "on_left",
                    "snap",
                    "taskbar_left_space_win11",
                    "taskbar_right_space_win11",
                    "double_click_action",
                ],
            );
            ui.label(theme::faint(tr("settings.monitor_taskbar_reserve_hint")));
            if taskbar["double_click_action"].as_u64() == Some(5) {
                changed |= taskbar_fields(ui, &mut taskbar, &["double_click_exe"]);
            }
        });
        theme::section_card(ui, Some("资源占用图"), |ui| {
            changed |= taskbar_fields(
                ui,
                &mut taskbar,
                &[
                    "show_status_bar",
                    "show_netspeed_figure",
                    "show_graph_dashed_box",
                    "netspeed_figure_max_value",
                    "netspeed_figure_max_value_unit",
                    "status_bar_color",
                    "graph_color_following_system",
                ],
            );
            let mut bars = taskbar["cm_graph_type"].as_bool().unwrap_or(true);
            theme::setting_row(ui, theme::row_label("占用图类型"), |ui| {
                changed |= combo(
                    ui,
                    "taskbar-graph-type",
                    if bars { "柱状图" } else { "横向滚动图" }.into(),
                    |ui| {
                        let mut changed = ui.selectable_value(&mut bars, true, "柱状图").changed();
                        changed |= ui
                            .selectable_value(&mut bars, false, "横向滚动图")
                            .changed();
                        changed
                    },
                );
            });
            taskbar["cm_graph_type"] = bars.into();
        });
        self.config["monitor"]["taskbar"] = taskbar.clone();
        if changed {
            events.push(SettingsEvent::ConfigChanged {
                path: "monitor.taskbar".into(),
                value: taskbar,
            });
        }
    }

    /// Module-specific hints and cards rendered above the generic form.
    fn draw_module_extras(&mut self, ui: &mut Ui, selected: &str, events: &mut Vec<SettingsEvent>) {
        if !matches!(
            selected,
            "app"
                | "mcp"
                | "audio"
                | "monitor"
                | "memory"
                | "utilities"
                | "clipboard"
                | "file_search"
                | "disk_analyzer"
                | "vision"
                | "quick_ask"
        ) {
            theme::callout(ui, Tone::Info, tr("settings.future_module"));
        }
        match selected {
            "app" => theme::callout(ui, Tone::Info, tr("settings.instant_apply")),
            "clipboard" => {
                theme::callout(ui, Tone::Info, tr("clipboard.privacy_notice"));
                theme::callout(ui, Tone::Info, tr("clipboard.settings_hint"));
            }
            "vision" => {
                theme::callout(ui, Tone::Info, tr("vision.settings_hint"));
                if let Some(tier) = self
                    .config
                    .get_mut("app")
                    .and_then(|app| app.get_mut("ocr_engine_tier"))
                {
                    let changed =
                        theme::section_card(ui, Some(&tr("settings.section_ocr")), |ui| {
                            edit_value(ui, "ocr_engine_tier", "app.ocr_engine_tier", tier)
                        });
                    if changed {
                        events.push(SettingsEvent::ConfigChanged {
                            path: "app.ocr_engine_tier".into(),
                            value: tier.clone(),
                        });
                    }
                }
            }
            "quick_ask" => self.draw_quick_ask_workbench(ui, events),
            "file_search" => {
                theme::callout(ui, Tone::Info, tr("file_tools.search_settings_hint"));
                if self.config["file_search"]["hotkey"]
                    .as_str()
                    .is_some_and(|key| key.eq_ignore_ascii_case("Alt+F"))
                {
                    theme::callout(ui, Tone::Warning, tr("file_tools.alt_f_warning"));
                    if ui
                        .add(theme::primary_button(tr("file_tools.use_double_ctrl")))
                        .clicked()
                    {
                        self.config["file_search"]["hotkey"] = "DoubleCtrl".into();
                        events.push(SettingsEvent::ConfigChanged {
                            path: "file_search.hotkey".into(),
                            value: "DoubleCtrl".into(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    fn draw_quick_ask_prompt_settings(&mut self, ui: &mut Ui, events: &mut Vec<SettingsEvent>) {
        theme::section_card(ui, Some("呼出与交互快捷键"), |ui| {
            ui.add(egui::Label::new(theme::muted(
                "选中任意窗口文字后按快捷键直接带入上下文提问，或在截图/贴图工具栏点击提问按钮带入图像。",
            )).wrap());
            let value = &mut self.config["quick_ask"]["hotkey"];
            if edit_value(ui, "hotkey", "quick_ask.hotkey", value) {
                events.push(SettingsEvent::ConfigChanged {
                    path: "quick_ask.hotkey".into(),
                    value: value.clone(),
                });
            }
        });
        let mut system_prompt = self.config["quick_ask"]["system_prompt"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut system_changed = false;
        theme::section_card(ui, Some("全局系统提示词 (System Prompt)"), |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::muted(
                    "设定 AI 助手的角色、回答风格与默认语言；留空则使用模型默认行为。",
                ));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !system_prompt.is_empty()
                        && ui
                            .add(theme::small_button("清空").icon(IconKind::Trash))
                            .clicked()
                    {
                        system_prompt.clear();
                        system_changed = true;
                    }
                });
            });
            ui.add_space(6.0);
            ui.push_id("quick-ask-system-prompt", |ui| {
                let response = prompt_text_input(
                    ui,
                    &mut system_prompt,
                    4,
                    "例如：你是一位严谨的技术助手，请用简体中文给出简洁、可操作的回答。",
                );
                let committed = commit_edit(ui, &response);
                system_changed |= response.changed() || committed;
            });
        });
        if system_changed && self.config["quick_ask"]["system_prompt"] != system_prompt {
            self.config["quick_ask"]["system_prompt"] = system_prompt.into();
            events.push(SettingsEvent::ConfigChanged {
                path: "quick_ask.system_prompt".into(),
                value: self.config["quick_ask"]["system_prompt"].clone(),
            });
        }

        let value = &mut self.config["quick_ask"]["prompts"];
        let draft = self
            .quick_ask_prompts
            .get_or_insert_with(|| QuickAskPromptDraft::new(value));
        draft.sync(value);
        let mut applied = false;
        theme::section_card(ui, Some("预设快捷指令 (Preset Prompts)"), |ui| {
            ui.horizontal(|ui| {
                let description_width = (ui.available_width() - 330.0).max(120.0);
                ui.allocate_ui_with_layout(
                    vec2(description_width, 0.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.add(
                            egui::Label::new(theme::muted(
                                "显示在提问小窗输入框上方的快捷指令胶囊，点击即可一键填入。",
                            ))
                            .wrap(),
                        );
                    },
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(theme::primary_button("新增预设").icon(IconKind::Plus))
                        .clicked()
                    {
                        draft.add();
                    }
                    if ui
                        .add(theme::secondary_button("恢复默认预设").icon(IconKind::Refresh))
                        .clicked()
                    {
                        draft.restore_defaults();
                    }
                    theme::pill(
                        ui,
                        format!("{} 项", draft.rows.len()),
                        theme::TEXT_MUTED,
                        theme::CONTROL,
                    );
                });
            });
            ui.add_space(8.0);
            let mut action = None;
            let count = draft.rows.len();
            if count == 0 {
                ui.label(theme::faint(
                    "暂无预设指令。可以添加新指令，或恢复默认预设。",
                ));
            }
            for (index, row) in draft.rows.iter_mut().enumerate() {
                ui.push_id(("quick-ask-prompt", row.key), |ui| {
                    egui::Frame::new()
                        .fill(theme::CONTROL)
                        .stroke(theme::stroke(1.0, theme::CARD_BORDER))
                        .corner_radius(8)
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let input_width = (ui.available_width() - 390.0).max(140.0);
                            let name_width = (input_width * 0.6).clamp(80.0, 180.0);
                            let id_width = (input_width - name_width).clamp(60.0, 120.0);
                            ui.horizontal(|ui| {
                                theme::icon(ui, IconKind::Bulb, theme::TEXT_MUTED);
                                theme::pill(ui, format!("#{}", index + 1), theme::ACCENT_LIGHT, theme::ACCENT_SOFT);
                                ui.label(theme::muted("按钮名称"));
                                ui.add(theme::text_input(&mut row.prompt.label)
                                    .id_salt("name").desired_width(name_width).hint_text("显示名称"));
                                ui.label(theme::muted("标识 ID"));
                                ui.add(theme::text_input(&mut row.prompt.id)
                                    .id_salt("id").desired_width(id_width).hint_text("唯一 ID"));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.add(theme::small_button("删除").icon(IconKind::Trash)).clicked() {
                                        action = Some(QuickAskPromptAction::Delete(index));
                                    }
                                    if ui.add_enabled(index + 1 < count, theme::small_button("↓")).on_hover_text("下移").clicked() {
                                        action = Some(QuickAskPromptAction::MoveDown(index));
                                    }
                                    if ui.add_enabled(index > 0, theme::small_button("↑")).on_hover_text("上移").clicked() {
                                        action = Some(QuickAskPromptAction::MoveUp(index));
                                    }
                                });
                            });
                            ui.add_space(8.0);
                            prompt_text_input(ui, &mut row.prompt.prompt, 3,
                                "输入执行该指令时发送给助手的提示词，例如：概括当前内容并列出关键结论。");
                        });
                });
                ui.add_space(10.0);
            }
            if let Some(action) = action {
                draft.edit_row(action);
            }
            let dirty = draft.value() != *value;
            let valid = draft.valid();
            if !valid {
                warning_text(ui, tr("quick_ask.prompt_invalid"));
            }
            ui.horizontal(|ui| {
                ui.label(theme::faint("ID 须唯一，名称与提示词不能为空。"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(dirty, theme::secondary_button(tr("settings.reset")))
                        .clicked()
                    {
                        draft.reset();
                    }
                    if ui
                        .add_enabled(
                            dirty && valid,
                            theme::primary_button(tr("settings.apply")).icon(IconKind::Check),
                        )
                        .clicked()
                    {
                        applied = draft.apply(value);
                    }
                });
            });
        });
        if applied {
            events.push(SettingsEvent::ConfigChanged {
                path: "quick_ask.prompts".into(),
                value: value.clone(),
            });
        }
    }

    fn commit_quick_ask(&mut self, value: Value, events: &mut Vec<SettingsEvent>) {
        let mut config: QuickAskConfig =
            serde_json::from_value(value).expect("typed QuickAsk settings");
        let previous: QuickAskConfig = serde_json::from_value(self.committed["quick_ask"].clone())
            .expect("committed QuickAsk settings");
        // Blank names/models remain editable drafts; disk snapshots retain valid values.
        for (id, channel) in &mut config.channels {
            if channel.name.trim().is_empty() {
                channel.name = previous
                    .channels
                    .get(id)
                    .map_or_else(|| channel.default_name(), |previous| previous.name.clone());
            }
            if channel.model.trim().is_empty() {
                channel.model = previous.channels.get(id).map_or_else(
                    || channel.provider.default_model().to_owned(),
                    |previous| previous.model.clone(),
                );
            }
        }
        if config.model.trim().is_empty() {
            config
                .model
                .clone_from(&config.channels[&config.active_channel].model);
        }
        config.sync_active_channel();
        let value = serde_json::json!(config);
        if self.committed["quick_ask"] != value {
            self.committed["quick_ask"] = value.clone();
            events.push(SettingsEvent::ConfigChanged {
                path: "quick_ask".into(),
                value,
            });
        }
    }

    fn request_quick_ask_models(
        &mut self,
        config: QuickAskConfig,
        events: &mut Vec<SettingsEvent>,
    ) {
        let (sender, result) = mpsc::channel();
        let state = self
            .quick_ask_models
            .entry(config.active_channel.clone())
            .or_default();
        state.error = None;
        state.fetched = None;
        state.pending = Some(QuickAskModelRequest {
            provider: config.provider,
            base_url: config.base_url.clone(),
            credential_target: config.credential_target.clone(),
            result,
        });
        let secret = &self.quick_ask_credentials.secret;
        events.push(SettingsEvent::QuickAskModels {
            config: Box::new(config),
            api_key_override: (!secret.trim().is_empty()).then(|| secret.clone()),
            result: sender,
        });
    }

    fn poll_quick_ask_models(&mut self, context: &Context, events: &mut Vec<SettingsEvent>) {
        let channels = &self.config["quick_ask"]["channels"];
        self.quick_ask_models
            .retain(|id, _| channels.get(id).is_some());
        let mut completed = Vec::new();
        for (id, state) in &mut self.quick_ask_models {
            let Some(request) = &state.pending else {
                continue;
            };
            let result = match request.result.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => Err("模型列表请求已中断，请重试。".into()),
            };
            if let Some(request) = state.pending.take() {
                completed.push((id.clone(), request, result));
            }
        }
        for (id, request, result) in completed {
            let result = (|| {
                let mut config: QuickAskConfig =
                    serde_json::from_value(self.config["quick_ask"].clone())
                        .map_err(|error| error.to_string())?;
                let mut committed: QuickAskConfig =
                    serde_json::from_value(self.committed["quick_ask"].clone())
                        .map_err(|error| error.to_string())?;
                config.sync_active_channel();
                committed.sync_active_channel();
                let select_first = config
                    .channels
                    .get(&id)
                    .is_some_and(|channel| channel.model.trim().is_empty());
                for candidate in [&mut config, &mut committed] {
                    let channel = candidate
                        .channels
                        .get_mut(&id)
                        .filter(|channel| {
                            channel.provider == request.provider
                                && channel.base_url == request.base_url
                                && channel.credential_target == request.credential_target
                        })
                        .ok_or_else(|| "连接已更改，请重新刷新模型列表。".to_owned())?;
                    let models = result.as_ref().map_err(Clone::clone)?;
                    channel.cached_models = models.clone();
                    if select_first && let Some(model) = models.first() {
                        channel.model.clone_from(model);
                        if candidate.active_channel == id {
                            candidate.model.clone_from(model);
                        }
                    }
                }
                let count = config.channels[&id].cached_models.len();
                self.config["quick_ask"] = serde_json::json!(config);
                // Persist the cache and any automatic selection, preserving unrelated editor drafts.
                self.commit_quick_ask(serde_json::json!(committed), events);
                Ok(count)
            })();
            let state = self.quick_ask_models.entry(id).or_default();
            match result {
                Ok(count) => {
                    state.fetched = Some(count);
                    state.error = None;
                }
                Err(error) => {
                    state.fetched = None;
                    state.error = Some(error);
                }
            }
        }
        if self
            .quick_ask_models
            .values()
            .any(|state| state.pending.is_some())
        {
            context.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn remove_quick_ask_channel(
        &mut self,
        config: &mut QuickAskConfig,
        delete_key: impl FnOnce(&str) -> Result<bool, String>,
    ) -> bool {
        if config.channels.len() <= 1 {
            self.quick_ask_credentials.status = "至少保留一个模型渠道。".into();
            self.quick_ask_credentials.error = true;
            return false;
        }
        config.sync_active_channel();
        let id = config.active_channel.clone();
        let shared = config.channels.iter().any(|(other_id, channel)| {
            other_id != &id
                && (channel.credential_target.eq_ignore_ascii_case(&config.credential_target)
                    // Canonical OpenAI channels can also consume the pre-channel credential.
                    || (config.credential_target.eq_ignore_ascii_case("Orbment/QuickAsk/default")
                        && channel.provider == QuickAskProvider::OpenAI
                        && channel.credential_target == channel.provider.default_credential_target()))
        });
        if !shared && let Err(error) = delete_key(&config.credential_target) {
            self.quick_ask_credentials.status = format!("删除渠道失败，密钥未清理：{error}");
            self.quick_ask_credentials.error = true;
            return false;
        }
        let removed = config
            .remove_channel(&id)
            .expect("known channel with another remaining");
        self.quick_ask_models.remove(&id);
        self.quick_ask_credentials.select(config);
        if !self.quick_ask_credentials.error {
            self.quick_ask_credentials.status = if shared {
                format!(
                    "已删除渠道「{}」；其他渠道共用此目标，已保留密钥。",
                    removed.name
                )
            } else {
                format!("已删除渠道「{}」并清理其凭据目标。", removed.name)
            };
        }
        true
    }

    fn draw_quick_ask_workbench(&mut self, ui: &mut Ui, events: &mut Vec<SettingsEvent>) {
        let mut config: QuickAskConfig =
            match serde_json::from_value(self.config["quick_ask"].clone()) {
                Ok(config) => config,
                Err(error) => {
                    theme::callout(ui, Tone::Warning, error.to_string());
                    return;
                }
            };
        config.sync_active_channel();
        self.quick_ask_credentials.select(&config);
        let mut selected = None;
        let mut add_channel = false;
        let mut delete_channel = false;
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                delete_channel = ui
                    .add_enabled(
                        config.channels.len() > 1,
                        theme::small_button("删除渠道").icon(IconKind::Trash),
                    )
                    .clicked();
                add_channel = ui.add(theme::small_button("+ 新增渠道")).clicked();
                let width = ui.available_width().max(0.0);
                ui.allocate_ui_with_layout(
                    vec2(width, 28.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("quick-ask-channel-selector")
                            .max_width(width)
                            .max_height(28.0)
                            .auto_shrink([false, true])
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            )
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    for id in config.ordered_channel_ids() {
                                        let channel = &config.channels[id];
                                        let name = if channel.name.trim().is_empty() {
                                            "未命名渠道"
                                        } else {
                                            &channel.name
                                        };
                                        if theme::chip(ui, name, config.active_channel == *id)
                                            .on_hover_text(format!(
                                                "{id} · {}",
                                                quick_ask_provider_label(channel.provider)
                                            ))
                                            .clicked()
                                        {
                                            selected = Some(id.clone());
                                        }
                                    }
                                });
                            });
                    },
                );
            });
        });
        let mut commit = false;
        if let Some(id) = selected {
            config.switch_channel(&id).expect("listed QuickAsk channel");
            commit = true;
        }
        if add_channel {
            config.add_channel();
            commit = true;
        } else if delete_channel {
            commit |= self.remove_quick_ask_channel(&mut config, |target| {
                orbment_core::credentials::delete_api_key(target).map_err(|error| error.to_string())
            });
        }
        config.sync_active_channel();
        self.quick_ask_credentials.select(&config);
        let channel_key = config.active_channel.clone();
        let old_provider = config.provider;
        let old_base_url = config.base_url.clone();
        let old_target = config.credential_target.clone();
        let mut refresh = false;
        let mut key_changed = false;
        ui.push_id(("quick-ask-channel", &channel_key), |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            theme::section_card(ui, Some("模型渠道与密钥配置"), |ui| {
                let mut provider = config.provider;
                theme::setting_row_sub(ui, "渠道名称与协议", format!("独立渠道 · {channel_key}"), |ui| {
                    egui::ComboBox::from_id_salt("quick-ask-protocol")
                        .icon(theme::combo_icon)
                        .width(235.0)
                        .truncate()
                        .selected_text(quick_ask_provider_label(provider))
                        .show_ui(ui, |ui| {
                            for option in QuickAskProvider::ALL {
                                ui.selectable_value(&mut provider, option, quick_ask_provider_label(option));
                            }
                        });
                    let response = ui.add(theme::text_input(
                        &mut config.channels.get_mut(&channel_key).expect("active channel").name,
                    ).id_salt("quick-ask-channel-name").desired_width(175.0).hint_text("渠道名称"));
                    commit |= commit_edit(ui, &response);
                });
                if provider != config.provider {
                    config.switch_provider(provider);
                    self.quick_ask_credentials.select(&config);
                    commit = true;
                }
                let endpoint = match config.chat_endpoint_url() {
                    Ok(url) => format!("端点预览: {url}"),
                    Err(error) => format!("端点预览: {error}"),
                };
                theme::setting_block(ui, |ui| {
                    ui.horizontal(|ui| {
                        let summary = format!("{} · {endpoint}", quick_ask_provider_label(config.provider));
                        ui.allocate_ui_with_layout(
                            vec2((ui.available_width() - 150.0).max(120.0), 22.0),
                            Layout::left_to_right(Align::Center),
                            |ui| {
                                ui.add(egui::Label::new(theme::muted(&summary)).truncate())
                                    .on_hover_text(&summary);
                            },
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let (text, foreground, background) = match self.quick_ask_credentials.saved {
                                Some(true) => ("已配置密钥", theme::GREEN, theme::GREEN_BG),
                                Some(false) => ("尚未保存密钥", theme::WARNING, theme::WARNING_BG),
                                None => ("密钥状态读取失败", theme::DANGER, theme::DANGER_BG),
                            };
                            theme::pill(ui, text, foreground, background);
                        });
                    });
                });
                let key_hint = if self.quick_ask_credentials.status.is_empty() {
                    "存入 Windows 凭据管理器，各渠道独立保存".into()
                } else if self.quick_ask_credentials.error {
                    format!("Windows 独立保存 · 错误：{}", self.quick_ask_credentials.status)
                } else {
                    format!("Windows 独立保存 · {}", self.quick_ask_credentials.status)
                };
                theme::setting_row_sub(ui, "API 密钥", key_hint, |ui| {
                    let target_valid = !config.credential_target.trim().is_empty();
                    if ui.add_enabled(target_valid, theme::secondary_button(tr("quick_ask.delete_api_key"))).clicked() {
                        match orbment_core::credentials::delete_api_key(&config.credential_target) {
                            Ok(_) => {
                                self.quick_ask_credentials.secret.clear();
                                self.quick_ask_credentials.status = "已删除当前目标的密钥。".into();
                                self.quick_ask_credentials.reload(&config);
                                if config.provider == QuickAskProvider::OpenAI
                                    && config.credential_target == config.provider.default_credential_target()
                                    && self.quick_ask_credentials.saved == Some(true)
                                {
                                    self.quick_ask_credentials.status =
                                        "当前目标密钥已删除；旧版兼容密钥仍可用，可将凭据目标名称改为 Orbment/QuickAsk/default 管理。".into();
                                }
                                key_changed = true;
                            }
                            Err(error) => {
                                self.quick_ask_credentials.status = error.to_string();
                                self.quick_ask_credentials.error = true;
                            }
                        }
                    }
                    if ui.add_enabled(
                        target_valid && !self.quick_ask_credentials.secret.trim().is_empty(),
                        theme::primary_button(tr("quick_ask.save_api_key")),
                    ).clicked() {
                        match orbment_core::credentials::write_api_key(
                            &config.credential_target, &self.quick_ask_credentials.secret,
                        ) {
                            Ok(()) => {
                                self.quick_ask_credentials.secret.clear();
                                self.quick_ask_credentials.status = tr("quick_ask.api_key_saved");
                                self.quick_ask_credentials.reload(&config);
                                key_changed = true;
                            }
                            Err(error) => {
                                self.quick_ask_credentials.status = error.to_string();
                                self.quick_ask_credentials.error = true;
                            }
                        }
                    }
                    ui.add(theme::text_input(&mut self.quick_ask_credentials.secret)
                        .id_salt(("quick-ask-key", &channel_key, config.provider.as_key(), &config.credential_target))
                        .password(true)
                        .hint_text("当前渠道的 API Key")
                        .desired_width(220.0))
                        .on_hover_text("密钥仅保存在 Windows 凭据管理器中，不写入配置文件。");
                });
                let protocol_hint = match config.provider {
                    QuickAskProvider::OpenAI => "支持 .../v1 基础地址或完整 /responses 端点",
                    QuickAskProvider::OpenAiCompatible =>
                        "支持 .../v1 基础地址或完整 /chat/completions 端点",
                    QuickAskProvider::Anthropic => "支持 .../v1 基础地址或完整 /messages 端点",
                    QuickAskProvider::Google => "支持 .../v1beta 基础地址或完整 :streamGenerateContent 端点",
                };
                theme::setting_row_sub(ui, "接口地址", protocol_hint, |ui| {
                    if ui.add(theme::small_button("恢复默认")).clicked() {
                        config.base_url = config.provider.default_base_url().into();
                        commit = true;
                    }
                    let response = ui.add(theme::text_input(&mut config.base_url)
                        .id_salt("quick-ask-base-url")
                        .desired_width(280.0));
                    commit |= commit_edit(ui, &response);
                });
                let state = self.quick_ask_models.get(&channel_key);
                let pending = state.is_some_and(|state| state.pending.is_some());
                let cached = &config.channels[&channel_key].cached_models;
                let model_hint = if pending {
                    "正在获取模型列表…".into()
                } else if let Some(error) = state.and_then(|state| state.error.as_ref()) {
                    format!("获取失败：{error}")
                } else if let Some(count) = state.and_then(|state| state.fetched) {
                    format!("已获取 {count} 个模型 · 也可手动输入")
                } else if cached.is_empty() {
                    "可直接输入模型 ID，或拉取渠道列表后下拉点选".into()
                } else {
                    format!("已缓存 {} 个模型 · 也可手动输入", cached.len())
                };
                theme::setting_row_sub(ui, "模型名称", model_hint, |ui| {
                    refresh = ui.add_enabled(!pending,
                        theme::secondary_button(if pending { "获取中" } else { "获取列表" })
                            .icon(IconKind::Refresh))
                        .on_hover_text("优先使用未保存的密钥获取列表，不会保存密钥；模型为空时自动选择首项。")
                        .clicked();
                    egui::ComboBox::from_id_salt("quick-ask-cached-models")
                        .icon(theme::combo_icon)
                        .width(150.0)
                        .selected_text(format!("缓存模型（{}）", cached.len()))
                        .show_ui(ui, |ui| {
                            if cached.is_empty() {
                                ui.label(theme::muted("尚无模型缓存，请获取列表。"));
                            }
                            for model in cached {
                                commit |= ui.selectable_value(
                                    &mut config.model, model.clone(), model,
                                ).changed();
                            }
                        });
                    let response = ui.add(theme::text_input(&mut config.model)
                        .id_salt("quick-ask-model")
                        .hint_text(config.provider.default_model())
                        .desired_width(180.0));
                    commit |= commit_edit(ui, &response);
                });
                let thinking_hint =
                    "映射为 Responses reasoning / Chat reasoning_effort / Anthropic thinking / Gemini thinkingConfig";
                theme::setting_row_sub(ui, "思考强度", thinking_hint, |ui| {
                    commit |= combo(ui, "quick-ask-thinking",
                        thinking_effort_label(config.thinking_effort), |ui| {
                            let mut changed = false;
                            for effort in [
                                ThinkingEffort::Off, ThinkingEffort::Low, ThinkingEffort::Medium,
                                ThinkingEffort::High, ThinkingEffort::Max,
                            ] {
                                changed |= ui.selectable_value(
                                    &mut config.thinking_effort, effort, thinking_effort_label(effort),
                                ).changed();
                            }
                            changed
                        });
                });
                theme::setting_row_sub(ui, "凭据目标名称",
                    "Windows 凭据管理器存储槽位，各渠道默认独立", |ui| {
                        if ui.add(theme::small_button("恢复默认")).clicked() {
                            config.credential_target = config.active_channel_config().default_credential_target();
                            commit = true;
                        }
                        let response = ui.add(theme::text_input(&mut config.credential_target)
                            .id_salt("quick-ask-target")
                            .desired_width(240.0))
                            .on_hover_text("修改后重新读取密钥，并清空未保存的密钥输入；保存和删除仅影响当前目标。");
                        commit |= commit_edit(ui, &response);
                    });
                if old_provider != config.provider || old_base_url != config.base_url
                    || old_target != config.credential_target || key_changed
                {
                    self.quick_ask_models.remove(&channel_key);
                    if let Some(channel) = config.channels.get_mut(&channel_key) {
                        channel.cached_models.clear();
                    }
                    self.quick_ask_credentials.select(&config);
                    commit |= key_changed;
                }
            });
        });
        config.sync_active_channel();
        self.config["quick_ask"] = serde_json::json!(config);
        if commit || refresh {
            self.commit_quick_ask(self.config["quick_ask"].clone(), events);
        }
        if refresh {
            self.request_quick_ask_models(config, events);
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    fn draw_audio_devices(&mut self, ui: &mut Ui, events: &mut Vec<SettingsEvent>) {
        for (kind, field, icon) in [
            ("playback", "selected_playback_devices", IconKind::Speaker),
            (
                "recording",
                "selected_recording_devices",
                IconKind::Microphone,
            ),
        ] {
            let available: Vec<Value> = self
                .audio_devices
                .get(kind)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let default_id = self
                .audio_devices
                .get(format!("default_{kind}"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let Some(selected) = self
                .config
                .get_mut("audio")
                .and_then(|audio| audio.get_mut(field))
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            let mut devices: Vec<(Value, bool)> = available
                .iter()
                .map(|device| (device.clone(), false))
                .collect();
            for item in selected.iter() {
                if !available.iter().any(|device| device["id"] == item["id"]) {
                    devices.push((item.clone(), true));
                }
            }
            let mut changed = false;
            theme::section_card(ui, Some(&label(field)), |ui| {
                ui.label(theme::muted(tr("settings.devices_hint")));
                if devices.is_empty() {
                    ui.label(theme::faint(tr("settings.no_devices")));
                }
                for (device, offline) in &devices {
                    let id = device["id"].as_str().unwrap_or_default();
                    let name = device["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .unwrap_or(id);
                    let mut checked = selected.iter().any(|item| item["id"].as_str() == Some(id));
                    ui.push_id(id, |ui| {
                        theme::icon_setting_row(ui, Some(icon), theme::row_label(name), |ui| {
                            if theme::toggle_switch(ui, &mut checked)
                                .on_hover_text(id)
                                .changed()
                            {
                                selected.retain(|item| item["id"].as_str() != Some(id));
                                if checked {
                                    selected.push(device.clone());
                                }
                                changed = true;
                            }
                            if *offline {
                                theme::pill(
                                    ui,
                                    tr("settings.device_offline"),
                                    theme::TEXT_FAINT,
                                    theme::CONTROL,
                                );
                            } else if default_id.as_deref() == Some(id) {
                                theme::pill(
                                    ui,
                                    tr("settings.device_default"),
                                    theme::GREEN,
                                    theme::GREEN_BG,
                                );
                            }
                        });
                    });
                }
            });
            if changed {
                events.push(SettingsEvent::ConfigChanged {
                    path: format!("audio.{field}"),
                    value: Value::Array(selected.clone()),
                });
            }
        }
    }
}

fn draw_about(ui: &mut Ui, events: &mut Vec<SettingsEvent>) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            theme::page_content_frame().show(ui, |ui| {
                theme::section_card(ui, None, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(theme::strong(ui, tr("settings.brand"), 18.0));
                        theme::pill(
                            ui,
                            format!("v{}", env!("CARGO_PKG_VERSION")),
                            theme::ACCENT_LIGHT,
                            theme::ACCENT_SOFT,
                        );
                    });
                    ui.add(egui::Label::new(theme::row_label(tr("settings.about_body"))).wrap());
                    ui.label(theme::faint(tr("settings.licenses")));
                });
                theme::section_card(ui, Some(&tr("settings.danger_zone")), |ui| {
                    theme::setting_row(ui, theme::muted(tr("settings.clear_traces_hint")), |ui| {
                        if ui
                            .add(theme::danger_button(tr("settings.clear_traces")))
                            .clicked()
                        {
                            events.push(SettingsEvent::Action {
                                action: "app.clear_traces".into(),
                                params: serde_json::json!({}),
                            });
                        }
                    });
                });
            });
        });
}

/// Edits one top-level field, holding back risky memory-clean masks behind a confirmation.
fn edit_field(
    ui: &mut Ui,
    key: &str,
    path: &str,
    value: &mut Value,
    warn_on_clean: bool,
    pending_clean_mask: &mut Option<u64>,
    events: &mut Vec<SettingsEvent>,
) {
    let previous = value.clone();
    if !edit_value(ui, key, path, value) {
        return;
    }
    if path == "memory.clean_regions_mask"
        && warn_on_clean
        && value.as_u64().unwrap_or(0)
            & !previous.as_u64().unwrap_or(0)
            & u64::from(orbment_analyzer::memory_clean::STALL_RISK_MASK)
            != 0
    {
        *pending_clean_mask = value.as_u64();
        *value = previous;
        return;
    }
    events.push(SettingsEvent::ConfigChanged {
        path: path.into(),
        value: value.clone(),
    });
}

fn object_title(path: &str, key: &str) -> String {
    if path == "memory.tray_icon" {
        tr("settings.field.memory_tray_icon")
    } else {
        label(key)
    }
}

fn label(key: &str) -> String {
    let key = if key == "family_drafts" {
        "families"
    } else {
        key
    };
    tr(&format!("settings.field.{key}"))
}

fn commit_edit(ui: &mut Ui, response: &egui::Response) -> bool {
    let dirty = response.id.with("dirty");
    if response.changed() {
        ui.data_mut(|data| data.insert_temp(dirty, true));
    }
    let ended = response.drag_stopped()
        || response.lost_focus()
        || (response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)))
        || (!response.has_focus() && ui.input(|input| !input.pointer.any_down()));
    if ended && ui.data(|data| data.get_temp::<bool>(dirty).unwrap_or(false)) {
        ui.data_mut(|data| data.insert_temp(dirty, false));
        true
    } else {
        false
    }
}

fn is_optional(key: &str, path: &str) -> bool {
    (matches!(key, "playback_device_id" | "recording_device_id")
        && (path.contains("profiles.") || path.contains("app_sound_rules.")))
        || (matches!(key, "process_path" | "hotkey") && path.contains("profiles."))
        || (path.starts_with("memory.reaper.families.") && key == "path_prefix")
        || path == "memory.clean_hotkey"
        || path == "quick_ask.hotkey"
        || (path.starts_with("utilities.")
            && (key.contains("hotkey") || key == "keep_awake_process_id"))
}

fn warning_text(ui: &mut Ui, text: String) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        theme::icon(ui, IconKind::Warning, theme::WARNING);
        ui.add(egui::Label::new(RichText::new(text).size(12.0).color(theme::WARNING)).wrap());
    });
}

/// Fixed outline so swatches close to the surface colour stay visible.
fn outline_swatch(ui: &Ui, response: &egui::Response) {
    ui.painter().rect_stroke(
        response.rect,
        ui.visuals().widgets.inactive.corner_radius,
        theme::stroke(1.0, theme::SWATCH_BORDER),
        egui::StrokeKind::Inside,
    );
}

/// Monitor metrics in `text_colors` order (shared by the floating window and the taskbar).
const MONITOR_METRICS: [&str; 13] = [
    "up",
    "down",
    "cpu",
    "memory",
    "gpu",
    "cpu_temperature",
    "gpu_temperature",
    "disk_temperature",
    "board_temperature",
    "board_fan",
    "gpu_memory",
    "disk_activity",
    "today_traffic",
];

/// Display name of one colour slot: floating colours map 1:1 to metrics, taskbar colours come
/// in (label, value) pairs per metric; other palettes are numbered.
fn color_slot_label(path: &str, index: usize) -> String {
    let metric = |index: usize| {
        MONITOR_METRICS
            .get(index)
            .map(|metric| tr(&format!("settings.metric.{metric}")))
    };
    let name = match path {
        "monitor.floating.text_colors" => metric(index),
        "monitor.taskbar.text_colors" => metric(index / 2).map(|name| {
            let part = if index.is_multiple_of(2) {
                "settings.color_part_label"
            } else {
                "settings.color_part_value"
            };
            format!("{name} · {}", tr(part))
        }),
        _ => None,
    };
    name.unwrap_or_else(|| format!("{:02}", index + 1))
}

fn color_row(ui: &mut Ui, key: &str, color: &mut [u8; 4], alpha: bool) -> (bool, bool) {
    theme::setting_row(ui, theme::row_label(label(key)), |ui| {
        ui.spacing_mut().interact_size = vec2(40.0, 24.0);
        let response = if alpha {
            ui.color_edit_button_srgba_unmultiplied(color)
        } else {
            let mut rgb = [color[0], color[1], color[2]];
            let response = ui.color_edit_button_srgb(&mut rgb);
            color[..3].copy_from_slice(&rgb);
            response
        };
        outline_swatch(ui, &response);
        ui.label(theme::faint(format!(
            "#{:02X}{:02X}{:02X}",
            color[0], color[1], color[2]
        )));
        (response.changed(), commit_edit(ui, &response))
    })
}

/// Compact four-column swatch grid for fixed-length colour lists.
fn color_grid(ui: &mut Ui, key: &str, path: &str, values: &mut [Value]) -> bool {
    let mut changed = false;
    theme::setting_block(ui, |ui| {
        ui.label(theme::row_label(format!(
            "{} · {}",
            label(key),
            values.len()
        )));
        let column = ((ui.available_width() - 3.0 * 16.0) / 4.0).max(72.0);
        egui::Grid::new(path)
            .num_columns(4)
            .min_col_width(column)
            .max_col_width(column)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                for (index, value) in values.iter_mut().enumerate() {
                    ui.push_id(index, |ui| {
                        ui.horizontal(|ui| {
                            let mut rgb = match value {
                                Value::Number(number) => {
                                    let number = number.as_u64().unwrap_or_default();
                                    [number as u8, (number >> 8) as u8, (number >> 16) as u8]
                                }
                                Value::String(text) => {
                                    let number =
                                        u32::from_str_radix(text.trim_start_matches('#'), 16)
                                            .unwrap_or_default();
                                    [(number >> 16) as u8, (number >> 8) as u8, number as u8]
                                }
                                _ => [0, 0, 0],
                            };
                            ui.spacing_mut().interact_size = vec2(30.0, 20.0);
                            let response = ui.color_edit_button_srgb(&mut rgb);
                            outline_swatch(ui, &response);
                            if response.changed() {
                                *value = if value.is_string() {
                                    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]).into()
                                } else {
                                    (rgb[0] as u32 | (rgb[1] as u32) << 8 | (rgb[2] as u32) << 16)
                                        .into()
                                };
                            }
                            changed |= commit_edit(ui, &response);
                            let hex = format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(color_slot_label(path, index))
                                        .size(12.5)
                                        .color(theme::TEXT),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&hex);
                            if !path.starts_with("monitor.taskbar.") {
                                ui.label(theme::faint(hex));
                            }
                        });
                    });
                    if index % 4 == 3 {
                        ui.end_row();
                    }
                }
            });
    });
    changed
}

fn number_editor(ui: &mut Ui, key: &str, path: &str, value: &mut serde_json::Number) -> bool {
    let size = [theme::NUMBER_WIDTH, 28.0];
    if matches!(key, "pos_x" | "pos_y" | "vertical_margin") {
        let mut number = value.as_i64().unwrap_or_default();
        let response = ui.add_sized(size, egui::DragValue::new(&mut number).speed(1));
        if response.changed() {
            *value = number.into();
        }
        commit_edit(ui, &response)
    } else if let Some(mut number) = value.as_u64() {
        let range = match path {
            "monitor.taskbar.size" | "monitor.floating.size" => 6..=24,
            "monitor.taskbar.digits_number" => 1..=12,
            "monitor.taskbar.item_space" => 0..=64,
            "monitor.taskbar.taskbar_left_space_win11"
            | "monitor.taskbar.taskbar_right_space_win11" => 0..=4096,
            "clipboard.display.image_max_height" => 96..=110,
            "disk_analyzer.default_detail_depth" => 1..=8,
            "disk_analyzer.min_block_pixel_size" | "disk_analyzer.max_top_files" => {
                1..=u32::MAX as u64
            }
            "vision.global_mouse.drag_threshold" | "vision.pin.max_history_pins" => {
                1..=u32::MAX as u64
            }
            "vision.pin.default_opacity_pct" => 1..=100,
            "vision.movie.fps" => 1..=240,
            "vision.movie.key_mouse.key_keep_time_ms" => 1..=60000,
            "vision.movie.key_mouse.max_history_count" => 1..=32,
            "vision.movie.key_mouse.key_font_size" => 8..=128,
            "vision.movie.key_mouse.character_merge_limit" => 0..=256,
            "vision.save.save_quality" | "vision.mark.watermark.opacity_pct" => 0..=100,
            "quick_ask.max_tokens"
            | "quick_ask.request_timeout_seconds"
            | "quick_ask.max_tool_rounds" => 1..=u32::MAX as u64,
            _ => 0..=u64::MAX,
        };
        let response = ui.add_sized(
            size,
            egui::DragValue::new(&mut number).speed(1).range(range),
        );
        if response.changed() {
            *value = number.into();
        }
        if matches!(
            path,
            "monitor.taskbar.taskbar_left_space_win11"
                | "monitor.taskbar.taskbar_right_space_win11"
        ) {
            response.changed()
        } else {
            commit_edit(ui, &response)
        }
    } else if let Some(mut number) = value.as_i64() {
        let response = ui.add_sized(size, egui::DragValue::new(&mut number).speed(1));
        if response.changed() {
            *value = number.into();
        }
        commit_edit(ui, &response)
    } else if let Some(mut number) = value.as_f64() {
        let range = match path {
            "monitor.taskbar.size" | "monitor.floating.size" => 6.0..=24.0,
            "vision.movie.key_mouse.pointer_scale" | "vision.movie.key_mouse.animation_speed" => {
                0.1..=10.0
            }
            _ if path.starts_with("vision.") && key != "angle" => 0.0..=f64::MAX,
            _ => f64::MIN..=f64::MAX,
        };
        let response = ui.add_sized(
            size,
            egui::DragValue::new(&mut number).speed(0.1).range(range),
        );
        if response.changed()
            && let Some(number) = serde_json::Number::from_f64(number)
        {
            *value = number;
        }
        commit_edit(ui, &response)
    } else {
        false
    }
}

fn combo(
    ui: &mut Ui,
    path: &str,
    selected_text: String,
    add: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    egui::ComboBox::from_id_salt(path)
        .width(theme::COMBO_WIDTH)
        .selected_text(selected_text)
        .show_ui(ui, add)
        .inner
        .unwrap_or(false)
}

pub(crate) fn edit_value(ui: &mut Ui, key: &str, path: &str, value: &mut Value) -> bool {
    let mut changed = false;
    let optional = is_optional(key, path);
    if key.contains("hotkey") && (value.is_string() || value.is_null()) {
        let mut chord = value.as_str().unwrap_or_default().to_owned();
        let title = if path == "quick_ask.hotkey" {
            "全局快速提问热键".into()
        } else {
            label(key)
        };
        theme::setting_row(ui, theme::row_label(title), |ui| {
            changed = crate::hotkeys::hotkey_recorder(ui, path, &mut chord);
        });
        if changed {
            *value = if optional && chord.is_empty() {
                Value::Null
            } else {
                chord.into()
            };
        }
        return changed;
    }
    if path == "monitor.taskbar.double_click_action" {
        let mut selected = value.as_u64().unwrap_or(0);
        let actions = [
            (4, "打开任务管理器"),
            (0, "打开连接详情"),
            (5, "打开指定程序"),
            (7, "无动作"),
        ];
        let title = actions
            .iter()
            .find(|(id, _)| *id == selected)
            .map(|(_, title)| *title)
            .unwrap_or("其他动作");
        theme::setting_row(ui, theme::row_label("双击动作"), |ui| {
            changed = combo(ui, path, title.into(), |ui| {
                let mut changed = false;
                for (id, title) in actions {
                    changed |= ui.selectable_value(&mut selected, id, title).changed();
                }
                changed
            });
        });
        if changed {
            *value = selected.into();
        }
        return changed;
    }
    if let Some(folder) = path_picker_kind(key, path)
        && (value.is_string() || value.is_null())
    {
        let mut text = value.as_str().unwrap_or_default().to_owned();
        theme::setting_block(ui, |ui| {
            ui.label(theme::row_label(label(key)));
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 100.0).max(100.0);
                let response = ui.add(theme::text_input(&mut text).desired_width(width));
                changed |= commit_edit(ui, &response);
                if ui
                    .add(
                        theme::secondary_button(if key == "manual_save_pattern" {
                            "选择目录…"
                        } else {
                            "浏览…"
                        })
                        .icon(IconKind::Folder),
                    )
                    .clicked()
                {
                    let selected = if folder {
                        theme::pick_folder_dialog(Some(&text))
                    } else {
                        theme::pick_file_dialog(
                            Some(&text),
                            matches!(
                                key,
                                "process_path" | "double_click_exe" | "everything_exe_path"
                            ),
                        )
                    };
                    if let Some(selected) = selected {
                        text = if key == "manual_save_pattern" {
                            save_pattern_in_directory(&text, &selected)
                        } else {
                            selected
                        };
                        changed = true;
                    }
                }
            });
        });
        // Preserve drafts, just as a regular text edit does; flush_pending commits on page changes.
        *value = if optional && text.is_empty() {
            Value::Null
        } else {
            text.into()
        };
        return changed;
    }
    if matches!(key, "exclude_paths" | "excluded_paths")
        && let Some(paths) = value.as_array_mut()
    {
        theme::setting_block(ui, |ui| {
            ui.label(theme::muted("这些目录不会进入文件搜索索引。"));
            let mut remove = None;
            for (index, item) in paths.iter_mut().enumerate() {
                ui.push_id(index, |ui| {
                    changed |= edit_value(ui, "path_prefix", &format!("{path}.{index}"), item);
                    if ui.add(theme::small_button("移除此目录")).clicked() {
                        remove = Some(index);
                    }
                });
            }
            if let Some(index) = remove {
                paths.remove(index);
                changed = true;
            }
            if ui
                .add(theme::secondary_button("添加文件夹…").icon(IconKind::Folder))
                .clicked()
                && let Some(path) = theme::pick_folder_dialog(None)
                && !paths
                    .iter()
                    .any(|existing| existing.as_str() == Some(path.as_str()))
            {
                paths.push(path.into());
                changed = true;
            }
        });
        return changed;
    }
    if key == "font_name"
        && let Some(name) = value.as_str()
    {
        let mut name = name.to_owned();
        theme::setting_row(ui, theme::row_label(label(key)), |ui| {
            ui.menu_button("选择…", |ui| {
                for font in [
                    "Microsoft YaHei UI",
                    "微软雅黑",
                    "Segoe UI",
                    "Consolas",
                    "DengXian",
                ] {
                    if ui.selectable_label(name == font, font).clicked() {
                        name = font.into();
                        changed = true;
                        ui.close_menu();
                    }
                }
            });
            let response = ui.add(theme::text_input(&mut name).desired_width(130.0));
            changed |= commit_edit(ui, &response);
        });
        *value = name.into();
        return changed;
    }
    if path.starts_with("vision.movie.key_mouse.")
        && key.ends_with("_color")
        && let Some(values) = value.as_array()
        && values.len() == 4
    {
        let mut color = std::array::from_fn::<_, 4, _>(|index| {
            values[index].as_u64().unwrap_or_default() as u8
        });
        let (edited, committed) = color_row(ui, key, &mut color, true);
        if edited {
            *value = serde_json::json!(color);
        }
        return committed;
    }
    if path == "vision.global_mouse.bindings" {
        return edit_mouse_bindings(ui, path, value);
    }
    if path == "vision.hotkeys" {
        return edit_vision_hotkeys(ui, value);
    }
    if path.starts_with("vision.")
        && (key.ends_with("_argb") || key.ends_with("_color"))
        && let Some(text) = value.as_str()
        && let Ok(number) = u32::from_str_radix(text.trim_start_matches('#'), 16)
    {
        let mut color = [
            (number >> 16) as u8,
            (number >> 8) as u8,
            number as u8,
            (number >> 24) as u8,
        ];
        let (edited, committed) = color_row(ui, key, &mut color, true);
        if edited {
            *value = format!(
                "{:02x}{:02x}{:02x}{:02x}",
                color[3], color[0], color[1], color[2]
            )
            .into();
        }
        return committed;
    }
    if path == "vision.mark.watermark.text"
        && let Some(text) = value.as_str()
    {
        let mut draft = text.to_owned();
        let response = theme::setting_block(ui, |ui| {
            ui.label(theme::row_label(label(key)));
            ui.add(
                egui::TextEdit::multiline(&mut draft)
                    .desired_width(f32::INFINITY)
                    .desired_rows(4),
            )
        });
        if response.changed() {
            *value = draft.into();
        }
        return commit_edit(ui, &response);
    }
    if matches!(
        path,
        "clipboard.capture.order"
            | "clipboard.content.item_actions"
            | "clipboard.content.item_action_order"
    ) && let Some(values) = value.as_array_mut()
    {
        return edit_clipboard_order(ui, key, path, values);
    }
    if key == "families" && value.is_array() {
        // Families have required owner/companion names, so edit the whole list atomically.
        let id = ui.make_persistent_id((path, "family_draft"));
        let (mut baseline, mut draft) = ui
            .data(|data| data.get_temp::<(Value, Value)>(id))
            .unwrap_or_else(|| (value.clone(), value.clone()));
        if baseline != *value && draft == baseline {
            draft = value.clone();
            baseline = value.clone();
        }
        edit_value(ui, "family_drafts", path, &mut draft);
        ui.label(theme::muted(tr("settings.family_draft_hint")));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    draft != *value,
                    theme::primary_button(tr("settings.apply_families")),
                )
                .clicked()
            {
                *value = draft.clone();
                baseline = draft.clone();
                changed = true;
            }
            if ui
                .add(theme::secondary_button(tr("settings.reset_families")))
                .clicked()
            {
                draft = value.clone();
                baseline = value.clone();
            }
        });
        ui.data_mut(|data| data.insert_temp(id, (baseline, draft)));
        return changed;
    }
    if path.starts_with("memory.tray_icon.") && key.ends_with("_color") {
        if let Some(number) = value.as_u64() {
            let mut color = [
                (number >> 16) as u8,
                (number >> 8) as u8,
                number as u8,
                (number >> 24) as u8,
            ];
            let (edited, committed) = color_row(ui, key, &mut color, true);
            if edited {
                *value = ((color[3] as u32) << 24
                    | (color[0] as u32) << 16
                    | (color[1] as u32) << 8
                    | color[2] as u32)
                    .into();
            }
            changed = committed;
        }
        return changed;
    }
    if let Some(number) = value.as_u64() {
        if let Some(items) = flag_options(key) {
            let mut bits = number;
            if key == "style" {
                theme::setting_row(ui, theme::row_label(label(key)), |ui| {
                    for (bit, name) in items.iter().rev() {
                        if theme::chip(ui, label(name), bits & bit != 0).clicked() {
                            bits ^= bit;
                            changed = true;
                        }
                    }
                });
            } else {
                theme::setting_block(ui, |ui| {
                    ui.label(theme::row_label(label(key)));
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                        for (bit, name) in items {
                            if theme::chip(ui, label(name), bits & bit != 0).clicked() {
                                bits ^= bit;
                                changed = true;
                            }
                        }
                    });
                });
            }
            if changed {
                *value = bits.into();
            }
            return changed;
        }
        if let Some(options) = numeric_options(key) {
            let mut selected = number;
            theme::setting_row(ui, theme::row_label(label(key)), |ui| {
                let name = options.get(number as usize).copied().unwrap_or("unknown");
                changed = combo(ui, path, tr(&format!("settings.option.{name}")), |ui| {
                    let mut changed = false;
                    for (index, option) in options.iter().enumerate() {
                        changed |= ui
                            .selectable_value(
                                &mut selected,
                                index as u64,
                                tr(&format!("settings.option.{option}")),
                            )
                            .changed();
                    }
                    changed
                });
            });
            if changed {
                *value = selected.into();
            }
            return changed;
        }
        if key.ends_with("_color") || path.contains(".text_colors.") {
            let mut color = [number as u8, (number >> 8) as u8, (number >> 16) as u8, 255];
            let (edited, committed) = color_row(ui, key, &mut color, false);
            if edited {
                *value =
                    (color[0] as u32 | (color[1] as u32) << 8 | (color[2] as u32) << 16).into();
            }
            return committed;
        }
    }
    let mut clear = false;
    match value {
        Value::Bool(value) => {
            theme::setting_row(ui, theme::row_label(label(key)), |ui| {
                changed = theme::toggle_switch(ui, value).changed();
            });
        }
        Value::Number(number) => {
            theme::setting_row(ui, theme::row_label(label(key)), |ui| {
                changed = number_editor(ui, key, path, number);
                if optional
                    && ui
                        .add(theme::small_button(tr("settings.clear_optional")))
                        .clicked()
                {
                    clear = true;
                }
            });
        }
        Value::String(text) => {
            theme::setting_row(ui, theme::row_label(label(key)), |ui| {
                if let Some(options) = stage6_options(path)
                    .or_else(|| clipboard_options(path))
                    .or_else(|| options(key))
                {
                    changed = combo(ui, path, tr(&format!("settings.option.{text}")), |ui| {
                        let mut changed = false;
                        for option in options {
                            changed |= ui
                                .selectable_value(
                                    text,
                                    (*option).into(),
                                    tr(&format!("settings.option.{option}")),
                                )
                                .changed();
                        }
                        changed
                    });
                } else {
                    let response = ui.add(theme::text_input(text).desired_width(theme::TEXT_WIDTH));
                    changed = commit_edit(ui, &response);
                }
                if optional
                    && ui
                        .add(theme::small_button(tr("settings.clear_optional")))
                        .clicked()
                {
                    clear = true;
                }
            });
            if key.contains("hotkey") && !text.is_empty() {
                if !(path == "file_search.hotkey" && text.eq_ignore_ascii_case("DoubleCtrl"))
                    && crate::hotkeys::parse_chord(text).is_err()
                {
                    warning_text(ui, tr("settings.invalid_hotkey"));
                }
                if text.eq_ignore_ascii_case("Alt+F") {
                    warning_text(ui, tr("settings.alt_f_warning"));
                }
            }
            if path == "app.ocr_engine_tier" {
                ui.label(theme::faint(tr("clipboard.ocr_tier_hint")));
            }
        }
        Value::Object(values) => {
            let title = object_title(path, key);
            theme::setting_block(ui, |ui| {
                egui::CollapsingHeader::new(theme::row_label(title))
                    .id_salt(path)
                    .default_open(path.ends_with(".draft"))
                    .show(ui, |ui| {
                        for &child in authored_object_fields(path) {
                            let Some(value) = values.get_mut(child) else {
                                continue;
                            };
                            ui.push_id(child, |ui| {
                                changed |= edit_value(ui, child, &format!("{path}.{child}"), value);
                            });
                        }
                    });
            });
        }
        Value::Array(values) if matches!(key, "text_colors" | "palette_colors") => {
            changed = color_grid(ui, key, path, values);
        }
        Value::Array(values) if key == "custom_banner_position" && values.len() == 2 => {
            theme::setting_row(ui, theme::row_label(label(key)), |ui| {
                for (index, axis) in [(1, "Y"), (0, "X")] {
                    let mut number = values[index].as_i64().unwrap_or_default();
                    let response = ui.add_sized(
                        [theme::NUMBER_WIDTH, 28.0],
                        egui::DragValue::new(&mut number)
                            .speed(1)
                            .prefix(format!("{axis}  ")),
                    );
                    if response.changed() {
                        values[index] = number.into();
                    }
                    changed |= commit_edit(ui, &response);
                }
            });
        }
        Value::Array(values) => {
            let open_id = ui.make_persistent_id((path, "list_open"));
            let mut open = ui
                .data(|data| data.get_temp::<bool>(open_id))
                .unwrap_or(false);
            let mut title = egui::text::LayoutJob::default();
            title.append(
                &label(key),
                0.0,
                egui::TextFormat::simple(FontId::proportional(14.0), theme::TEXT),
            );
            title.append(
                &format!("{} {}", values.len(), tr("settings.items_unit")),
                10.0,
                egui::TextFormat {
                    valign: Align::Center,
                    ..egui::TextFormat::simple(FontId::proportional(12.0), theme::TEXT_FAINT)
                },
            );
            theme::setting_row(ui, title, |ui| {
                if ui
                    .add(
                        theme::secondary_button(tr("settings.add"))
                            .icon(IconKind::Plus)
                            .min_size(vec2(0.0, 28.0)),
                    )
                    .clicked()
                {
                    values.push(array_item(key, values));
                    changed = true;
                    open = true;
                }
                if !values.is_empty()
                    && ui
                        .add(
                            theme::small_button(tr(if open {
                                "settings.collapse"
                            } else {
                                "settings.expand"
                            }))
                            .icon(if open {
                                IconKind::ChevronDown
                            } else {
                                IconKind::ChevronRight
                            })
                            .min_size(vec2(0.0, 28.0)),
                        )
                        .clicked()
                {
                    open = !open;
                }
            });
            if open && !values.is_empty() {
                ui.indent(path, |ui| {
                    let mut remove = None;
                    let mut move_item = None;
                    for (index, value) in values.iter_mut().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(theme::muted(format!(
                                    "{} {}",
                                    tr("settings.item"),
                                    index + 1
                                )));
                                if ui.add(theme::small_button(tr("settings.remove"))).clicked() {
                                    remove = Some(index);
                                }
                                if index > 0
                                    && ui
                                        .add(theme::small_button(tr("settings.move_up")))
                                        .clicked()
                                {
                                    move_item = Some(index);
                                }
                            });
                            changed |= edit_value(ui, "value", &format!("{path}.{index}"), value);
                        });
                    }
                    if let Some(index) = remove {
                        values.remove(index);
                        changed = true;
                    }
                    if let Some(index) = move_item {
                        values.swap(index, index - 1);
                        changed = true;
                    }
                });
            }
            ui.data_mut(|data| data.insert_temp(open_id, open));
        }
        Value::Null => {
            theme::setting_row(ui, theme::row_label(label(key)), |ui| {
                if ui
                    .add(theme::secondary_button(tr("settings.set_optional")))
                    .clicked()
                {
                    *value = if key == "keep_awake_process_id" {
                        Value::from(std::process::id())
                    } else {
                        Value::String(String::new())
                    };
                    changed = true;
                }
            });
        }
    }
    if clear {
        *value = Value::Null;
        changed = true;
    }
    changed
}

fn draw_utilities_status(ui: &mut Ui, status: &mut Value, events: &mut Vec<SettingsEvent>) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            theme::page_content_frame().show(ui, |ui| {
                theme::section_card(ui, Some(&tr("settings.quick_actions")), |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (icon, key, action) in [
                            (
                                IconKind::Monitor,
                                "settings.refresh_displays",
                                "utilities.display_brightness",
                            ),
                            (
                                IconKind::Palette,
                                "settings.pick_color",
                                "utilities.pick_color",
                            ),
                            (
                                IconKind::Pin,
                                "settings.toggle_topmost",
                                "utilities.set_topmost",
                            ),
                        ] {
                            if ui
                                .add(
                                    theme::secondary_button(tr(key))
                                        .icon(icon)
                                        .min_size(vec2(150.0, 40.0)),
                                )
                                .clicked()
                            {
                                events.push(SettingsEvent::Action {
                                    action: action.into(),
                                    params: if action == "utilities.display_brightness" {
                                        serde_json::json!({"refresh": true})
                                    } else {
                                        serde_json::json!({})
                                    },
                                });
                            }
                        }
                    });
                });
                if status.is_null() {
                    theme::callout(ui, Tone::Info, tr("settings.utilities_loading"));
                    return;
                }
                theme::section_card(ui, Some(&tr("settings.keep_awake_card")), |ui| {
                    let mut active = status["keep_awake_active"].as_bool().unwrap_or(false);
                    theme::setting_row(
                        ui,
                        theme::row_label(tr("settings.keep_awake_active")),
                        |ui| {
                            if theme::toggle_switch(ui, &mut active).changed() {
                                events.push(SettingsEvent::Action {
                                    action: "utilities.keep_awake".into(),
                                    params: serde_json::json!({"enabled": active}),
                                });
                            }
                        },
                    );
                });
                theme::section_card(ui, Some(&tr("settings.display_brightness")), |ui| {
                    let Some(monitors) = status.get_mut("monitors").and_then(Value::as_array_mut)
                    else {
                        return;
                    };
                    if monitors.is_empty() {
                        ui.label(theme::faint(tr("settings.no_displays")));
                    }
                    for monitor in monitors {
                        let Some(id) = monitor["id"].as_u64() else {
                            continue;
                        };
                        let name = monitor["name"].as_str().unwrap_or_default().to_owned();
                        ui.push_id(id, |ui| {
                            theme::icon_setting_row(
                                ui,
                                Some(IconKind::Monitor),
                                theme::row_label(name),
                                |ui| {
                                    if let Some(percent) = monitor["percent"].as_u64() {
                                        let mut percent = percent as u8;
                                        ui.spacing_mut().slider_width = 220.0;
                                        let response = ui.add(
                                            egui::Slider::new(&mut percent, 0..=100).suffix("%"),
                                        );
                                        if response.changed() {
                                            monitor["percent"] = percent.into();
                                        }
                                        if commit_edit(ui, &response) {
                                            events.push(SettingsEvent::Action {
                                        action: "utilities.display_brightness".into(),
                                        params: serde_json::json!({"id": id, "percent": percent}),
                                    });
                                        }
                                    } else {
                                        ui.label(theme::faint(tr(
                                            "settings.brightness_unavailable",
                                        )));
                                    }
                                },
                            );
                            if let Some(error) = monitor["error"].as_str() {
                                warning_text(ui, error.to_owned());
                            }
                        });
                    }
                });
            });
        });
}

fn array_item(key: &str, values: &[Value]) -> Value {
    match key {
        "palette_colors" => Value::String("#5B8FF9".into()),
        "families" | "family_drafts" => {
            let title = tr("settings.new_family");
            let mut index = 1;
            let name = loop {
                let name = format!("{title} {index}");
                if !values
                    .iter()
                    .any(|family| family["name"].as_str() == Some(name.as_str()))
                {
                    break name;
                }
                index += 1;
            };
            serde_json::to_value(ProcessFamily {
                name,
                ..Default::default()
            })
            .expect("ProcessFamily serialization")
        }
        "owners" | "companions" | "exemptions" => Value::String(String::new()),
        "profiles" => {
            let mut index = 1;
            let id = loop {
                let id = format!("profile-{index}");
                if !values
                    .iter()
                    .any(|profile| profile["id"].as_str() == Some(id.as_str()))
                {
                    break id;
                }
                index += 1;
            };
            serde_json::to_value(AudioProfile {
                id,
                name: tr("settings.new_profile"),
                ..Default::default()
            })
            .expect("AudioProfile serialization")
        }
        "app_sound_rules" => {
            serde_json::to_value(AppSoundRule::default()).expect("AppSoundRule serialization")
        }
        "selected_playback_devices" | "selected_recording_devices" => {
            serde_json::to_value(AudioDeviceSelection::default())
                .expect("AudioDeviceSelection serialization")
        }
        "text_colors" => Value::from(0xffffff_u32),
        _ => values
            .first()
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
    }
}

fn edit_clipboard_order(ui: &mut Ui, key: &str, path: &str, values: &mut Vec<Value>) -> bool {
    let capture = path == "clipboard.capture.order";
    let options: &[&str] = if capture {
        &["files", "image", "html", "rtf", "text"]
    } else {
        &[
            "paste",
            "pastePlain",
            "pastePath",
            "copy",
            "copyPlain",
            "openLink",
            "reveal",
            "note",
            "star",
            "pinItem",
            "delete",
        ]
    };
    let mut changed = false;
    egui::CollapsingHeader::new(label(key))
        .id_salt(path)
        .show(ui, |ui| {
            let mut remove = None;
            let mut move_up = None;
            for (index, value) in values.iter().enumerate() {
                ui.push_id(index, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(tr(&format!(
                            "settings.option.{}",
                            value.as_str().unwrap_or_default()
                        )));
                        if index > 0 && ui.small_button(tr("settings.move_up")).clicked() {
                            move_up = Some(index);
                        }
                        if !capture && ui.small_button(tr("settings.remove")).clicked() {
                            remove = Some(index);
                        }
                    });
                });
            }
            if let Some(index) = remove {
                values.remove(index);
                changed = true;
            }
            if let Some(index) = move_up {
                values.swap(index, index - 1);
                changed = true;
            }
            if !capture {
                ui.menu_button(tr("settings.add"), |ui| {
                    for option in options {
                        if !values.iter().any(|value| value.as_str() == Some(*option))
                            && ui
                                .button(tr(&format!("settings.option.{option}")))
                                .clicked()
                        {
                            values.push(Value::String((*option).into()));
                            changed = true;
                            ui.close_menu();
                        }
                    }
                });
            }
        });
    changed
}

fn edit_vision_hotkeys(ui: &mut Ui, value: &mut Value) -> bool {
    ui.add(egui::Label::new(theme::faint(tr("vision.hotkey_hint"))).wrap());
    let mut changed = false;
    let Some(values) = value.as_object_mut() else {
        return false;
    };
    for action in [
        "capture",
        "pin_last",
        "restore_last",
        "ocr",
        "longshot",
        "record_start",
        "record_stop",
        "quick_ask",
    ] {
        let mut chord = values
            .get(action)
            .or_else(|| {
                if action == "pin_last" {
                    values.get("pin")
                } else {
                    None
                }
            })
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        ui.push_id(action, |ui| {
            theme::setting_row(
                ui,
                theme::row_label(tr(&format!("vision.hotkey.{action}"))),
                |ui| {
                    if crate::hotkeys::hotkey_recorder(
                        ui,
                        &format!("vision.hotkeys.{action}"),
                        &mut chord,
                    ) {
                        if action == "pin_last" {
                            values.remove("pin");
                        }
                        if chord.is_empty() {
                            values.remove(action);
                        } else {
                            values.insert(action.into(), chord.clone().into());
                        }
                        changed = true;
                    }
                },
            );
            if !chord.is_empty() && crate::hotkeys::parse_chord(&chord).is_err() {
                warning_text(ui, tr("settings.invalid_hotkey"));
            }
        });
    }
    changed
}

fn prompt_text_input(ui: &mut Ui, text: &mut String, rows: usize, hint: &str) -> egui::Response {
    egui::Frame::new()
        .fill(theme::CONTROL)
        .stroke(theme::stroke(1.0, theme::CARD_BORDER))
        .corner_radius(8)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(text)
                    .frame(false)
                    .margin(Margin::same(0))
                    .desired_rows(rows)
                    .desired_width(f32::INFINITY)
                    .hint_text(hint),
            )
        })
        .inner
}

fn edit_mouse_bindings(ui: &mut Ui, path: &str, value: &mut Value) -> bool {
    let id = ui.make_persistent_id((path, "draft"));
    let (mut baseline, mut draft) = ui
        .data_mut(|data| data.get_temp::<(Value, Value)>(id))
        .unwrap_or_else(|| (value.clone(), value.clone()));
    if baseline != *value {
        baseline = value.clone();
        draft = value.clone();
    }
    let mut changed = false;
    egui::CollapsingHeader::new(label("bindings"))
        .default_open(true).show(ui, |ui| {
            let Some(rows) = draft.as_array_mut() else { return; };
            let mut remove = None;
            for (index, row) in rows.iter_mut().enumerate() {
                ui.push_id(index, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} {}", tr("settings.item"), index + 1));
                        if ui.add(theme::small_button(tr("settings.remove")).icon(IconKind::Trash)).clicked() {
                            remove = Some(index);
                        }
                    });
                    ui.horizontal(|ui| {
                        for modifier in ["Ctrl", "Shift", "Alt", "Win"] {
                            let mut enabled = row["modifiers"].as_array().is_some_and(|items| items.iter().any(|item| item.as_str() == Some(modifier)));
                            if theme::chip(ui, modifier, enabled).clicked() {
                                enabled = !enabled;
                                let modifiers = row["modifiers"].as_array_mut().expect("Typed gesture modifier array");
                                modifiers.retain(|item| item.as_str() != Some(modifier));
                                if enabled { modifiers.push(modifier.into()); }
                            }
                        }
                    });
                    for (field, options) in [
                        ("button", &["Left", "Right", "Middle", "X1", "X2"][..]),
                        ("gesture", &["click", "doubleClick", "drag", "wheelUp", "wheelDown", "wheelLeft", "wheelRight"][..]),
                        ("action", MOUSE_GESTURE_ACTIONS),
                    ] {
                        if field == "button" && row["gesture"].as_str().is_some_and(|gesture| gesture.starts_with("wheel")) {
                            continue;
                        }
                        ui.horizontal(|ui| {
                            ui.label(label(field));
                            let mut selected = row[field].as_str().unwrap_or_default().to_owned();
                            egui::ComboBox::from_id_salt(field).icon(theme::combo_icon)
                                .selected_text(tr(&format!("settings.option.{selected}"))).show_ui(ui, |ui| {
                                    for option in options {
                                        ui.selectable_value(&mut selected, (*option).into(), tr(&format!("settings.option.{option}")));
                                    }
                                });
                            row[field] = selected.into();
                        });
                    }
                    ui.separator();
                });
            }
            if let Some(index) = remove { rows.remove(index); }
            if ui.add(theme::secondary_button(tr("settings.add")).icon(IconKind::Plus)).clicked() {
                rows.push(serde_json::json!({"modifiers":["Ctrl"],"button":"Left","gesture":"drag","action":"vision.pin_last"}));
            }
            let mut seen = BTreeSet::new();
            let valid = rows.iter().all(|row| {
                let mut modifiers: Vec<_> = row["modifiers"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                modifiers.sort_unstable();
                let wheel = row["gesture"].as_str().is_some_and(|gesture| gesture.starts_with("wheel"));
                let button = if wheel { "" } else { row["button"].as_str().unwrap_or_default() };
                (wheel || button != "Left" || !modifiers.is_empty())
                    && row["action"].as_str().is_some_and(|action| MOUSE_GESTURE_ACTIONS.contains(&action))
                    && seen.insert(format!("{modifiers:?}:{button}:{}", row["gesture"]))
            });
            if !valid { warning_text(ui, tr("vision.gesture_invalid")); }
            ui.horizontal(|ui| {
                if ui.add_enabled(valid && *value != draft, theme::primary_button(tr("settings.apply"))).clicked() {
                    *value = draft.clone(); baseline = value.clone(); changed = true;
                }
                if ui.add(theme::secondary_button(tr("settings.reset"))).clicked() { draft = value.clone(); }
            });
        });
    ui.data_mut(|data| data.insert_temp(id, (baseline, draft)));
    changed
}

fn stage6_options(path: &str) -> Option<&'static [&'static str]> {
    Some(match path {
        "vision.screenshot.select_mode" => {
            &["Smart", "Rectangle", "RoundRect", "Polygon", "Freehand"]
        }
        "vision.ocr.punctuation_type" => &["Auto", "Chinese", "English"],
        _ if path.starts_with("vision.mark.") && path.ends_with(".mode") => &["Pixel", "Blur"],
        _ if path.starts_with("vision.mark.") && path.ends_with(".text_direction") => {
            &["Auto", "Follow", "Flat"]
        }
        _ if path.starts_with("vision.mark.")
            && (path.ends_with(".start_style") || path.ends_with(".end_style")) =>
        {
            &["None", "Arrow", "Circle", "Square", "Diamond"]
        }
        _ if path == "vision.mark.serial.shape" => &["Circle", "Square"],
        _ => return None,
    })
}

fn clipboard_options(path: &str) -> Option<&'static [&'static str]> {
    Some(match path {
        "clipboard.content.auto_paste" => &[
            "singleClickPaste",
            "doubleClickPaste",
            "singleClickCopy",
            "doubleClickCopy",
            "disabled",
        ],
        "clipboard.content.middle_click" => &[
            "disabled",
            "singleClickPaste",
            "singleClickPastePlain",
            "singleClickCopy",
            "singleClickCopyPlain",
            "delete",
        ],
        "clipboard.content.sort" => &["updatedAtDesc", "createdAtDesc", "useCountDesc"],
        "clipboard.history.unit" => &["forever", "hours", "days", "weeks", "months"],
        "clipboard.window.position" => &["followCursor", "center", "remember", "fixed", "dock"],
        "clipboard.window.dock_edge" => &["left", "right", "top", "bottom"],
        "clipboard.window.select_category_on_open" => {
            &["preserve", "all", "text", "image", "files"]
        }
        "clipboard.window.select_range_on_open" => &["preserve", "all", "favorite"],
        _ if path.starts_with("clipboard.capture.order.") => {
            &["files", "image", "html", "rtf", "text"]
        }
        _ if path.starts_with("clipboard.content.item_actions.")
            || path.starts_with("clipboard.content.item_action_order.") =>
        {
            &[
                "paste",
                "pastePlain",
                "pastePath",
                "copy",
                "copyPlain",
                "openLink",
                "reveal",
                "note",
                "star",
                "pinItem",
                "delete",
            ]
        }
        _ => return None,
    })
}

fn options(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "language" => &["zh-CN"],
        "backend" => &["BuiltinMft", "EverythingBridge"],
        "residency_mode" => &["Resident", "SnapshotIncrement", "OnDemandOnly"],
        "display_unit" => &["Dynamic", "Bytes", "KB", "MB", "GB", "TB"],
        "ocr_engine_tier" => &["Ask", "WindowsNative", "OfflineOnnx"],
        "cycle_mode" => &["Available", "All"],
        "switch_icon_mode" => &["Playback", "Recording", "Both", "Never"],
        "tooltip_info" => &["Playback", "Recording", "Both", "None"],
        "icon_double_click" => &["SwitchDevice", "OpenSettings"],
        "switch_device_notification" => &["Banner", "SystemToast", "Sound", "None"],
        "switch_profile_notification" | "mic_mute_notification" => {
            &["Banner", "SystemToast", "None"]
        }
        "banner_position" => &[
            "TopLeft",
            "TopCenter",
            "TopRight",
            "MiddleLeft",
            "MiddleCenter",
            "MiddleRight",
            "BottomLeft",
            "BottomCenter",
            "BottomRight",
            "Custom",
        ],
        "banner_display_info" => &["Full", "IconOnly", "TextOnly"],
        "mic_mute_banner_mode" | "mic_unmute_banner_mode" => &["None", "Temporary", "Persistent"],
        "log_level" => &["trace", "debug", "info", "warn", "error"],
        "traffic_unit" => &["MB", "GB"],
        "middle_click_action" => &["CleanMemory", "ToggleWindow", "OpenTaskMgr"],
        "tray_middle_click_action" => &["audio.switch_playback", "memory.clean", "none"],
        "topmost_target" => &["Foreground", "UnderCursor"],
        "color_format" => &[
            "Hex", "Rgb", "Hsl", "Hsb", "Cmyk", "Oklab", "Oklch", "Lab", "Bgr", "Custom",
        ],
        "keep_awake_mode" => &["System", "Display"],
        "keep_awake_duration" => &["Indefinite", "Minutes30", "Hour1", "Hours2", "Process"],
        _ => return None,
    })
}

fn flag_options(key: &str) -> Option<&'static [(u64, &'static str)]> {
    Some(match key {
        "clean_regions_mask" => &[
            (1, "clean_working_set"),
            (2, "clean_system_working_set"),
            (4, "clean_standby_priority0"),
            (8, "clean_standby"),
            (16, "clean_modified"),
            (32, "clean_combine"),
        ],
        "hardware_monitor_mask" => &[(1, "cpu"), (2, "gpu"), (4, "disk")],
        "style" => &[
            (1, "bold"),
            (2, "italic"),
            (4, "underline"),
            (8, "strikethrough"),
        ],
        "display_mask" => &[
            (1, "up"),
            (2, "down"),
            (4, "cpu"),
            (8, "memory"),
            (16, "gpu"),
            (32, "cpu_temperature"),
            (64, "gpu_temperature"),
            (128, "disk_temperature"),
            (512, "disk"),
            (1024, "total_speed"),
            (2048, "cpu_frequency"),
            (4096, "today_traffic"),
            (8192, "disk_read"),
            (16384, "disk_write"),
        ],
        _ => return None,
    })
}

fn numeric_options(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "desktop_toolbar_mode" => &["toolbar_show", "toolbar_edge", "toolbar_hidden"],
        "encode_format" => &["MP4", "GIF", "WebP"],
        "netspeed_figure_max_value_unit" => &["KB", "MB"],
        "toolbar_display_type" => &["toolbar_show", "toolbar_hidden", "toolbar_auto"],
        "memory_display_mode" => &["percent", "used", "available"],
        "speed_unit" => &["auto", "KB", "MB"],
        "double_click_action" => &[
            "connection",
            "history",
            "show_more",
            "settings",
            "task_manager",
            "launch_exe",
            "change_skin",
            "None",
        ],
        "allow_out_of_border" => &["confined", "outside", "edge_hide"],
        _ => return None,
    })
}

fn hotkey_diagnostics(config: &Value) -> Vec<String> {
    fn collect(value: &Value, path: &str, out: &mut Vec<(String, String)>) {
        match value {
            Value::Object(values) => {
                if values.get("enabled") == Some(&Value::Bool(false)) {
                    return;
                }
                for (key, value) in values {
                    let path = format!("{path}.{key}");
                    if key.contains("hotkey")
                        && let Some(chord) = value.as_str().filter(|v| !v.is_empty())
                    {
                        out.push((path.clone(), chord.into()));
                    }
                    collect(value, &path, out);
                }
            }
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    collect(value, &format!("{path}.{index}"), out);
                }
            }
            _ => {}
        }
    }
    let mut hotkeys = Vec::new();
    collect(config, "", &mut hotkeys);
    let mut seen = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for (path, chord) in hotkeys {
        if path == ".file_search.hotkey" && chord.eq_ignore_ascii_case("DoubleCtrl") {
            continue;
        }
        match crate::hotkeys::parse_chord(&chord) {
            Ok((modifiers, key)) => {
                if let Some(previous) = seen.insert((modifiers.0, key), path.clone()) {
                    diagnostics.push(format!(
                        "{}: {chord} ({previous}, {path})",
                        tr("settings.hotkey_conflict")
                    ));
                }
            }
            Err(_) => diagnostics.push(format!(
                "{}: {chord} ({path})",
                tr("settings.invalid_hotkey")
            )),
        }
    }
    diagnostics
}

fn path_picker_kind(key: &str, path: &str) -> Option<bool> {
    match key {
        "pawnio_module_dir" | "path_prefix" | "skin_selected" | "manual_save_pattern" => Some(true),
        "double_click_exe" | "everything_exe_path" | "process_path" => Some(false),
        _ if path.starts_with("file_search.exclude_paths.") => Some(true),
        _ => None,
    }
}

fn expandable_setting_row(ui: &mut Ui, key: &str, title: &str) -> bool {
    let id = ui.make_persistent_id(key);
    let mut open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    theme::setting_row(ui, theme::row_label(title), |ui| {
        if ui
            .add(
                theme::small_button(if open { "收起" } else { "展开…" }).icon(if open {
                    IconKind::ChevronDown
                } else {
                    IconKind::ChevronRight
                }),
            )
            .clicked()
        {
            open = !open;
        }
    });
    ui.data_mut(|data| data.insert_temp(id, open));
    open
}

fn taskbar_fields(ui: &mut Ui, taskbar: &mut Value, keys: &[&str]) -> bool {
    let mut changed = false;
    for key in keys {
        if let Some(value) = taskbar.get_mut(*key) {
            ui.push_id(*key, |ui| {
                changed |= edit_value(ui, key, &format!("monitor.taskbar.{key}"), value);
            });
        }
    }
    changed
}

fn taskbar_order(ui: &mut Ui, taskbar: &mut Value) -> bool {
    const ITEMS: &[(u8, &str)] = &[
        (0, "上传"),
        (1, "下载"),
        (10, "总网速"),
        (4, "显卡利用率"),
        (2, "CPU 利用率"),
        (6, "显卡温度"),
        (5, "CPU 温度"),
        (3, "内存"),
        (11, "CPU 频率"),
        (7, "硬盘温度"),
        (9, "硬盘利用率"),
        (12, "今日流量"),
        (13, "硬盘读取"),
        (14, "硬盘写入"),
    ];
    let mut order: Vec<u8> = taskbar["item_order"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_u64().and_then(|value| u8::try_from(value).ok()))
        .filter(|id| ITEMS.iter().any(|(known, _)| known == id))
        .collect();
    for &(id, _) in ITEMS {
        if !order.contains(&id) {
            order.push(id);
        }
    }
    let mut mask = taskbar["display_mask"].as_u64().unwrap_or(0);
    let mut changed = false;
    let mut movement = None;
    for (index, id) in order.iter().copied().enumerate() {
        let Some((_, title)) = ITEMS.iter().find(|(candidate, _)| *candidate == id) else {
            continue;
        };
        ui.push_id(id, |ui| {
            theme::setting_row(ui, theme::row_label(*title), |ui| {
                let mut on = mask & (1u64 << id) != 0;
                if theme::toggle_switch(ui, &mut on).changed() {
                    if on {
                        mask |= 1u64 << id;
                    } else {
                        mask &= !(1u64 << id);
                    }
                    changed = true;
                }
                if ui
                    .add_enabled(index + 1 < order.len(), theme::small_button("↓"))
                    .clicked()
                {
                    movement = Some((index, index + 1));
                }
                if ui
                    .add_enabled(index > 0, theme::small_button("↑"))
                    .clicked()
                {
                    movement = Some((index, index - 1));
                }
            });
        });
    }
    if let Some((from, to)) = movement {
        order.swap(from, to);
        changed = true;
    }
    if changed {
        taskbar["display_mask"] = mask.into();
        taskbar["item_order"] = serde_json::json!(order);
    }
    changed
}

fn save_pattern_in_directory(pattern: &str, directory: &str) -> String {
    let filename = pattern.rsplit(['\\', '/']).next().unwrap_or(pattern);
    std::path::Path::new(directory)
        .join(filename)
        .to_string_lossy()
        .into_owned()
}

fn authored_object_fields(path: &str) -> &'static [&'static str] {
    if path.starts_with("audio.profiles.") {
        return &[
            "name",
            "enabled",
            "playback_device_id",
            "recording_device_id",
            "hotkey",
            "process_path",
            "on_process_start",
            "on_window_activate",
            "change_communications",
            "id",
        ];
    }
    if path.starts_with("audio.app_sound_rules.") {
        return &[
            "enabled",
            "process_path",
            "playback_device_id",
            "recording_device_id",
        ];
    }
    if path.starts_with("memory.reaper.families.") {
        return &[
            "name",
            "owners",
            "companions",
            "exemptions",
            "path_prefix",
            "include_descendants",
        ];
    }
    if path == "memory.tray_icon" {
        return &[
            "enabled",
            "font",
            "text_color",
            "bg_color",
            "transparent_bg",
            "show_border",
            "round_corners",
            "antialias",
            "warn_level_pct",
            "warn_color",
            "danger_level_pct",
            "danger_color",
            "middle_click_action",
        ];
    }
    if path == "vision.movie.key_mouse" {
        return &[
            "record_mouse",
            "record_keyboard",
            "smooth_track",
            "simplify_animation",
            "pointer_scale",
            "animation_speed",
            "left_color",
            "right_color",
            "middle_color",
            "show_characters",
            "show_combinations",
            "show_modifiers",
            "show_function_keys",
            "key_keep_time_ms",
            "max_history_count",
            "key_font_size",
            "key_text_color",
            "key_background_color",
            "character_merge_limit",
        ];
    }
    if path.ends_with(".labels") {
        return &[
            "up",
            "down",
            "total_speed",
            "cpu",
            "cpu_temperature",
            "cpu_frequency",
            "gpu",
            "gpu_temperature",
            "memory",
            "disk",
            "disk_temperature",
            "today_traffic",
            "disk_read",
            "disk_write",
        ];
    }
    match path.rsplit('.').next().unwrap_or(path) {
        "arrow" | "broken_line" | "geometry" | "pencil" | "marker" => &[
            "width",
            "filling",
            "path_mode",
            "start_style",
            "end_style",
            "double_click_comment",
            "text_direction",
            "text",
        ],
        "text" | "style" => &[
            "font",
            "size",
            "bold",
            "italic",
            "outline",
            "outline_width",
            "outline_color_argb",
            "background",
            "background_color_argb",
        ],
        "serial" => &["global_count", "start", "shape", "text"],
        "watermark" => &["text", "tiled", "spacing", "angle", "opacity_pct", "style"],
        "mosaic" | "auto_mosaic" => &["mode", "strength"],
        "magnifier" => &["scale", "radius"],
        "high_light" => &["mask_color_argb", "border_width"],
        _ => &[],
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prompt_edits_additions_order_and_deletions_apply_as_one_transaction() {
        let original = json!([
            {"id":"prompt-1", "label":"One", "prompt":"First task"},
            {"id":"prompt-3", "label":"Three", "prompt":"Third task"}
        ]);
        let mut value = original.clone();
        let mut draft = QuickAskPromptDraft::new(&value);
        draft.rows[0].prompt.label = "Edited name".into();
        draft.rows[0].prompt.prompt = "Edited task".into();
        draft.add();
        assert_eq!(draft.rows[2].prompt.id, "prompt-2");
        draft.rows[2].prompt.prompt = "New task".into();
        let new_row_key = draft.rows[2].key;
        draft.edit_row(QuickAskPromptAction::MoveUp(2));
        draft.edit_row(QuickAskPromptAction::MoveUp(1));
        draft.edit_row(QuickAskPromptAction::MoveDown(0));
        draft.edit_row(QuickAskPromptAction::Delete(2));
        draft.rows[0].prompt.id = "renamed".into();
        assert_eq!(draft.rows[1].key, new_row_key);
        assert_eq!(value, original);
        assert!(draft.apply(&mut value));
        assert_eq!(
            value,
            json!([
                {"id":"renamed", "label":"Edited name", "prompt":"Edited task"},
                {"id":"prompt-2", "label":tr("quick_ask.new_prompt"), "prompt":"New task"}
            ])
        );
        assert!(!draft.apply(&mut value));
    }

    #[test]
    fn prompt_validation_and_reset_preserve_the_last_applied_list() {
        let original = json!([
            {"id":"custom", "label":"Custom", "prompt":"Custom task"}
        ]);
        let mut value = original.clone();
        let mut draft = QuickAskPromptDraft::new(&value);
        for field in ["id", "label", "prompt"] {
            draft.reset();
            let prompt = &mut draft.rows[0].prompt;
            match field {
                "id" => prompt.id.clear(),
                "label" => prompt.label.clear(),
                _ => prompt.prompt.clear(),
            }
            assert!(!draft.apply(&mut value));
            assert_eq!(value, original);
        }
        draft.reset();
        draft.add();
        draft.rows[1].prompt.id = " custom ".into();
        draft.rows[1].prompt.prompt = "Another task".into();
        assert!(!draft.apply(&mut value));
        draft.reset();
        assert_eq!(draft.value(), original);
        draft.edit_row(QuickAskPromptAction::Delete(0));
        assert!(draft.apply(&mut value));
        assert_eq!(value, json!([]));
    }

    #[test]
    fn prompt_defaults_and_external_updates_rebase_without_leaking_drafts() {
        let original = json!([{"id":"custom", "label":"Custom", "prompt":"Custom task"}]);
        let mut value = original.clone();
        let mut draft = QuickAskPromptDraft::new(&value);
        draft.rows[0].prompt.label = "Unsaved edit".into();
        draft.sync(&value);
        assert_eq!(draft.rows[0].prompt.label, "Unsaved edit");
        draft.restore_defaults();
        let defaults = json!(QuickAskConfig::default().prompts);
        assert_eq!(draft.value(), defaults);
        assert_eq!(value, original);
        draft.reset();
        assert_eq!(draft.value(), original);
        draft.restore_defaults();
        assert!(draft.apply(&mut value));
        draft.rows[0].prompt.prompt = "Unsaved default edit".into();
        draft.reset();
        assert_eq!(draft.value(), defaults);
        let external = json!([{"id":"external", "label":"External", "prompt":"External task"}]);
        draft.sync(&external);
        assert_eq!(draft.value(), external);
        assert!(!draft.apply(&mut external.clone()));
    }

    fn draw_prompt_tab(
        context: &Context,
        view: &mut SettingsView,
        input: Vec<egui::Event>,
        events: &mut Vec<SettingsEvent>,
    ) -> egui::FullOutput {
        context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    vec2(1000.0, 1600.0),
                )),
                events: input,
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| view.draw_module_form(ui, events));
            },
        )
    }

    fn click_prompt_control(
        context: &Context,
        view: &mut SettingsView,
        output: &egui::FullOutput,
        label: &str,
        events: &mut Vec<SettingsEvent>,
    ) {
        let pointer = visible_text(output, label)
            .unwrap_or_else(|| panic!("prompt control {label:?} must be visible"))
            .center();
        for pressed in [true, false] {
            draw_prompt_tab(
                context,
                view,
                vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::PointerButton {
                        pos: pointer,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                events,
            );
        }
    }

    #[test]
    fn prompt_tab_clear_and_apply_dispatch_only_the_edited_settings() {
        let context = Context::default();
        let config = QuickAskConfig {
            system_prompt: "Global role and language instructions".into(),
            ..Default::default()
        };
        let mut view = SettingsView::new(json!({"quick_ask":config}));
        view.select_form_tab("quick_ask", 1);
        let mut events = Vec::new();
        draw_prompt_tab(&context, &mut view, Vec::new(), &mut events);
        let output = draw_prompt_tab(&context, &mut view, Vec::new(), &mut events);
        click_prompt_control(&context, &mut view, &output, "清空", &mut events);
        assert_eq!(view.config["quick_ask"]["system_prompt"], "");
        assert_eq!(view.config["quick_ask"]["hotkey"], json!(config.hotkey));
        assert_eq!(view.config["quick_ask"]["prompts"], json!(config.prompts));
        assert!(matches!(events.as_slice(),
            [SettingsEvent::ConfigChanged { path, value }]
                if path == "quick_ask.system_prompt" && value == ""));
        events.clear();
        view.quick_ask_prompts.as_mut().unwrap().rows[0]
            .prompt
            .label = "Edited preset".into();
        let output = draw_prompt_tab(&context, &mut view, Vec::new(), &mut events);
        assert!(events.is_empty());
        assert_ne!(
            view.config["quick_ask"]["prompts"][0]["label"],
            "Edited preset"
        );
        click_prompt_control(
            &context,
            &mut view,
            &output,
            &tr("settings.apply"),
            &mut events,
        );
        assert!(matches!(events.as_slice(),
            [SettingsEvent::ConfigChanged { path, value }]
                if path == "quick_ask.prompts" && value[0]["label"] == "Edited preset"));
    }

    fn request_model_list(
        view: &mut SettingsView,
        config: QuickAskConfig,
    ) -> mpsc::Sender<Result<Vec<String>, String>> {
        let mut events = Vec::new();
        view.request_quick_ask_models(config, &mut events);
        let SettingsEvent::QuickAskModels { result, .. } = events.pop().unwrap() else {
            panic!("model refresh must dispatch a background request");
        };
        result
    }

    #[test]
    fn model_refresh_preserves_origin_after_switching_channels() {
        let mut initial = QuickAskConfig::default();
        let custom = initial.add_channel();
        for origin in initial.ordered_channel_ids() {
            let mut config = initial.clone();
            config.switch_channel(origin).unwrap();
            let mut view = SettingsView::new(json!({"quick_ask": config}));
            let sender = request_model_list(&mut view, config.clone());
            let destination = if origin == &custom {
                QuickAskProvider::OpenAiCompatible.default_channel_id()
            } else {
                &custom
            };
            config.switch_channel(destination).unwrap();
            config.model = "destination-model".into();
            config.sync_active_channel();
            view.config["quick_ask"] = json!(config);
            view.committed = view.config.clone();
            sender.send(Ok(vec!["origin-model".into()])).unwrap();
            let mut events = Vec::new();
            view.poll_quick_ask_models(&Context::default(), &mut events);
            assert_eq!(
                view.config["quick_ask"]["channels"][origin]["cached_models"],
                json!(["origin-model"]),
            );
            assert_eq!(
                view.config["quick_ask"]["channels"][destination]["cached_models"],
                json!([]),
            );
            assert_eq!(view.config["quick_ask"]["model"], "destination-model");
            assert_eq!(view.config["quick_ask"]["active_channel"], destination);
            assert_eq!(
                view.config["quick_ask"]["provider"],
                json!(QuickAskProvider::OpenAiCompatible)
            );
            assert!(matches!(events.as_slice(),
                [SettingsEvent::ConfigChanged { path, .. }] if path == "quick_ask"));
            assert!(view.quick_ask_models[origin].pending.is_none());
        }
    }

    #[test]
    fn model_refresh_rejects_results_for_changed_connections() {
        let channel = QuickAskProvider::OpenAI.default_channel_id();
        for field in ["base_url", "credential_target", "provider"] {
            let mut config = QuickAskConfig::default();
            config.sync_active_channel();
            config.channels.get_mut(channel).unwrap().cached_models = vec!["previous".into()];
            let mut view = SettingsView::new(json!({"quick_ask": config}));
            let sender = request_model_list(&mut view, config.clone());
            match field {
                "base_url" => config.base_url = "https://other.example/v1".into(),
                "credential_target" => config.credential_target = "Orbment/Test/Other".into(),
                _ => config.switch_provider(QuickAskProvider::OpenAiCompatible),
            }
            config.sync_active_channel();
            view.config["quick_ask"] = json!(config);
            view.committed = view.config.clone();
            sender.send(Ok(vec!["stale-model".into()])).unwrap();
            let mut events = Vec::new();
            view.poll_quick_ask_models(&Context::default(), &mut events);
            assert!(events.is_empty());
            assert_eq!(
                view.config["quick_ask"]["channels"][channel]["cached_models"],
                json!(["previous"])
            );
            assert!(view.quick_ask_models[channel].error.is_some());
        }
    }

    #[test]
    fn model_refresh_does_not_commit_an_unfinished_model_edit() {
        let channel = QuickAskProvider::OpenAI.default_channel_id();
        let mut config = QuickAskConfig::default();
        config.sync_active_channel();
        let original_model = config.model.clone();
        let mut view = SettingsView::new(json!({"quick_ask": config}));
        let sender = request_model_list(&mut view, config.clone());
        config.model = "unfinished-model-draft".into();
        config.sync_active_channel();
        view.config["quick_ask"] = json!(config);
        sender.send(Ok(vec!["listed-model".into()])).unwrap();
        let mut events = Vec::new();
        view.poll_quick_ask_models(&Context::default(), &mut events);
        assert_eq!(view.config["quick_ask"]["model"], "unfinished-model-draft");
        assert_eq!(view.committed["quick_ask"]["model"], original_model);
        let SettingsEvent::ConfigChanged { value, .. } = &events[0] else {
            panic!("the model cache must be persisted");
        };
        assert_eq!(value["model"], original_model);
        assert_eq!(
            value["channels"][channel]["cached_models"],
            json!(["listed-model"])
        );
        events.clear();
        view.flush_pending(&mut events);
        assert!(matches!(events.as_slice(),
            [SettingsEvent::ConfigChanged { path, value }]
                if path == "quick_ask" && value["model"] == "unfinished-model-draft"));
    }

    #[test]
    fn model_discovery_fills_empty_drafts_without_persisting_an_invalid_model() {
        let origin = QuickAskProvider::OpenAI.default_channel_id();
        for switch_while_pending in [false, true] {
            let mut config = QuickAskConfig::default();
            config.sync_active_channel();
            let previous_model = config.model.clone();
            let mut view = SettingsView::new(json!({"quick_ask": config}));
            config.model = "  ".into();
            config.base_url = "https://discovery.example/v1".into();
            config.sync_active_channel();
            view.config["quick_ask"] = json!(config);
            let mut events = Vec::new();
            view.commit_quick_ask(view.config["quick_ask"].clone(), &mut events);
            assert_eq!(view.config["quick_ask"]["model"], "  ");
            assert_eq!(view.committed["quick_ask"]["model"], previous_model);
            let sender = request_model_list(&mut view, config.clone());
            if switch_while_pending {
                config
                    .switch_channel(QuickAskProvider::Google.default_channel_id())
                    .unwrap();
                view.config["quick_ask"] = json!(config);
            }
            view.flush_pending(&mut events);
            for event in &events {
                if let SettingsEvent::ConfigChanged { value, .. } = event {
                    serde_json::from_value::<QuickAskConfig>(value.clone())
                        .unwrap()
                        .validate()
                        .expect("model discovery must never commit an empty model");
                }
            }
            view.sync_config(view.committed.clone());
            assert_eq!(view.config["quick_ask"]["channels"][origin]["model"], "  ");
            events.clear();
            sender
                .send(Ok(vec!["first-model".into(), "second-model".into()]))
                .unwrap();
            view.poll_quick_ask_models(&Context::default(), &mut events);
            for snapshot in [&view.config, &view.committed] {
                assert_eq!(
                    snapshot["quick_ask"]["channels"][origin]["model"],
                    "first-model"
                );
                assert_eq!(
                    snapshot["quick_ask"]["model"],
                    if switch_while_pending {
                        QuickAskProvider::Google.default_model()
                    } else {
                        "first-model"
                    }
                );
            }
            assert_eq!(view.quick_ask_models[origin].fetched, Some(2));
            let [SettingsEvent::ConfigChanged { path, value }] = events.as_slice() else {
                panic!("automatic model selection must persist with the discovered cache");
            };
            assert_eq!(path, "quick_ask");
            assert_eq!(value["channels"][origin]["model"], "first-model");
            serde_json::from_value::<QuickAskConfig>(value.clone())
                .unwrap()
                .validate()
                .unwrap();
        }
    }

    #[test]
    fn model_refresh_uses_unsaved_key_and_preserves_cache_on_failure() {
        let channel = QuickAskProvider::OpenAI.default_channel_id();
        let mut config = QuickAskConfig::default();
        config.sync_active_channel();
        config.channels.get_mut(channel).unwrap().cached_models = vec!["cached-model".into()];
        let mut view = SettingsView::new(json!({"quick_ask": config}));
        view.quick_ask_credentials.secret = "unsaved-test-key".into();
        let mut events = Vec::new();
        view.request_quick_ask_models(config, &mut events);
        let SettingsEvent::QuickAskModels {
            api_key_override,
            result,
            ..
        } = events.pop().unwrap()
        else {
            panic!("model refresh must dispatch a background request");
        };
        assert_eq!(api_key_override.as_deref(), Some("unsaved-test-key"));
        assert!(!view.config.to_string().contains("unsaved-test-key"));
        result.send(Err("service unavailable".into())).unwrap();
        view.poll_quick_ask_models(&Context::default(), &mut events);
        assert!(events.is_empty());
        assert_eq!(
            view.config["quick_ask"]["channels"][channel]["cached_models"],
            json!(["cached-model"])
        );
        assert_eq!(
            view.quick_ask_models[channel].error.as_deref(),
            Some("service unavailable")
        );
        assert!(view.quick_ask_models[channel].pending.is_none());
    }

    #[test]
    fn newer_model_request_cannot_be_overwritten_by_an_older_completion() {
        let channel = QuickAskProvider::OpenAI.default_channel_id();
        let mut config = QuickAskConfig::default();
        config.sync_active_channel();
        let mut view = SettingsView::new(json!({"quick_ask": config}));
        let old = request_model_list(&mut view, config.clone());
        let new = request_model_list(&mut view, config);
        assert!(old.send(Ok(vec!["old-model".into()])).is_err());
        new.send(Ok(vec!["new-model".into()])).unwrap();
        let mut events = Vec::new();
        view.poll_quick_ask_models(&Context::default(), &mut events);
        assert_eq!(
            view.config["quick_ask"]["channels"][channel]["cached_models"],
            json!(["new-model"])
        );
        assert!(view.quick_ask_models[channel].error.is_none());
    }

    #[test]
    fn blank_channel_names_and_models_stay_editable_without_invalid_commits() {
        for newly_added in [false, true] {
            let mut config = QuickAskConfig::default();
            let mut view = SettingsView::new(json!({"quick_ask": config}));
            if newly_added {
                config.add_channel();
            }
            let id = config.active_channel.clone();
            let expected_name = config.active_channel_config().name.clone();
            let expected_model = config.model.clone();
            config.channels.get_mut(&id).unwrap().name = "  ".into();
            config.model.clear();
            config.base_url = "https://draft.example/v1".into();
            config.sync_active_channel();
            view.config["quick_ask"] = json!(config);
            let mut events = Vec::new();
            view.commit_quick_ask(view.config["quick_ask"].clone(), &mut events);
            assert_eq!(view.config["quick_ask"]["channels"][&id]["name"], "  ");
            assert_eq!(view.config["quick_ask"]["model"], "");
            assert_eq!(
                view.committed["quick_ask"]["channels"][&id]["name"],
                expected_name
            );
            assert_eq!(view.committed["quick_ask"]["model"], expected_model);
            let sender = request_model_list(&mut view, config);
            view.sync_config(view.committed.clone());
            assert_eq!(view.config["quick_ask"]["channels"][&id]["name"], "  ");
            sender.send(Ok(vec!["discovered-model".into()])).unwrap();
            view.poll_quick_ask_models(&Context::default(), &mut events);
            assert_eq!(view.config["quick_ask"]["channels"][&id]["name"], "  ");
            assert_eq!(view.config["quick_ask"]["model"], "discovered-model");
            assert_eq!(
                view.committed["quick_ask"]["channels"][&id]["name"],
                expected_name
            );
            assert_eq!(view.committed["quick_ask"]["model"], "discovered-model");
            view.config["quick_ask"]["channels"][&id]["name"] = json!("Finished name");
            view.flush_pending(&mut events);
            assert_eq!(view.committed["quick_ask"]["active_channel"], id);
            assert_eq!(
                view.committed["quick_ask"]["channels"][&id]["name"],
                "Finished name"
            );
            for event in events {
                if let SettingsEvent::ConfigChanged { value, .. } = event {
                    serde_json::from_value::<QuickAskConfig>(value)
                        .unwrap()
                        .validate()
                        .unwrap();
                }
            }
        }
    }

    #[test]
    fn channel_rename_keeps_pending_models_and_uncommitted_name() {
        let mut config = QuickAskConfig::default();
        let id = config.add_channel();
        let previous_name = config.active_channel_config().name.clone();
        let mut view = SettingsView::new(json!({"quick_ask": config}));
        let sender = request_model_list(&mut view, config.clone());
        config.channels.get_mut(&id).unwrap().name = "Renaming draft".into();
        view.config["quick_ask"] = json!(config);
        sender.send(Ok(vec!["custom-model".into()])).unwrap();
        let mut events = Vec::new();
        view.poll_quick_ask_models(&Context::default(), &mut events);
        assert_eq!(
            view.config["quick_ask"]["channels"][&id]["name"],
            "Renaming draft"
        );
        assert_eq!(
            view.committed["quick_ask"]["channels"][&id]["name"],
            previous_name
        );
        assert_eq!(
            view.committed["quick_ask"]["channels"][&id]["cached_models"],
            json!(["custom-model"])
        );
        assert_eq!(view.quick_ask_models[&id].fetched, Some(1));
    }

    #[test]
    fn channel_deletion_failure_preserves_channel_key_draft_and_pending_request() {
        let mut config = QuickAskConfig::default();
        let id = config.add_channel();
        let target = config.credential_target.clone();
        let mut view = SettingsView::new(json!({"quick_ask": config}));
        let sender = request_model_list(&mut view, config.clone());
        view.quick_ask_credentials.secret = "unsaved-key".into();
        let before = json!(config);
        assert!(!view.remove_quick_ask_channel(&mut config, |requested| {
            assert_eq!(requested, target);
            Err("credential access denied".into())
        }));
        assert_eq!(json!(config), before);
        assert_eq!(view.quick_ask_credentials.secret, "unsaved-key");
        assert!(view.quick_ask_credentials.error);
        assert!(
            view.quick_ask_credentials
                .status
                .contains("credential access denied")
        );
        assert!(view.quick_ask_models[&id].pending.is_some());
        sender.send(Ok(vec!["still-current".into()])).unwrap();
    }

    #[test]
    fn deleting_channel_cleans_only_its_target_and_drops_pending_results() {
        let mut config = QuickAskConfig::default();
        let id = config.add_channel();
        let target = config.credential_target.clone();
        let mut view = SettingsView::new(json!({"quick_ask": config}));
        let sender = request_model_list(&mut view, config.clone());
        view.quick_ask_credentials.secret = "removed-channel-draft".into();
        let mut deleted = None;
        assert!(view.remove_quick_ask_channel(&mut config, |requested| {
            deleted = Some(requested.to_owned());
            Ok(true)
        }));
        assert_eq!(deleted.as_deref(), Some(target.as_str()));
        assert!(!config.channels.contains_key(&id));
        assert!(!config.ordered_channel_ids().contains(&id));
        assert!(!view.quick_ask_models.contains_key(&id));
        assert!(view.quick_ask_credentials.secret.is_empty());
        assert!(sender.send(Ok(vec!["removed-model".into()])).is_err());
        assert_eq!(config.active_channel, config.ordered_channel_ids()[0]);
        view.config["quick_ask"] = json!(config);
        let mut events = Vec::new();
        view.commit_quick_ask(view.config["quick_ask"].clone(), &mut events);
        view.poll_quick_ask_models(&Context::default(), &mut events);
        let [SettingsEvent::ConfigChanged { path, value }] = events.as_slice() else {
            panic!("channel removal must persist exactly one config snapshot");
        };
        assert_eq!(path, "quick_ask");
        assert!(value["channels"].get(&id).is_none());
        serde_json::from_value::<QuickAskConfig>(value.clone())
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn channel_deletion_preserves_shared_and_legacy_fallback_credentials() {
        for legacy_fallback in [false, true] {
            let mut config = QuickAskConfig::default();
            let retained_id = config.active_channel.clone();
            let retained_target = config.credential_target.clone();
            let removed_id = config.add_channel();
            config.credential_target = if legacy_fallback {
                "Orbment/QuickAsk/default".into()
            } else {
                retained_target.to_uppercase()
            };
            config.sync_active_channel();
            let mut view = SettingsView::new(json!({"quick_ask": config}));
            assert!(view.remove_quick_ask_channel(&mut config, |_| {
                panic!("a remaining channel still consumes this credential target")
            }));
            assert!(!config.channels.contains_key(&removed_id));
            assert_eq!(
                config.channels[&retained_id].credential_target,
                retained_target
            );
            if !view.quick_ask_credentials.error {
                assert!(view.quick_ask_credentials.status.contains("已保留密钥"));
            }
        }
    }

    #[test]
    fn last_channel_cannot_be_removed_or_have_its_key_deleted() {
        let mut config = QuickAskConfig::default();
        let retained = config.active_channel.clone();
        for id in config.ordered_channel_ids().to_vec() {
            if id != retained {
                config.remove_channel(&id).unwrap();
            }
        }
        let before = json!(config);
        let mut view = SettingsView::new(json!({"quick_ask": config}));
        assert!(!view.remove_quick_ask_channel(&mut config, |_| {
            panic!("the last channel's credentials must remain untouched")
        }));
        assert_eq!(json!(config), before);
        assert!(view.quick_ask_credentials.error);
    }

    #[test]
    fn queued_model_results_cannot_restore_a_removed_channel() {
        let mut config = QuickAskConfig::default();
        let id = config.add_channel();
        let mut view = SettingsView::new(json!({"quick_ask": config}));
        let sender = request_model_list(&mut view, config.clone());
        sender.send(Ok(vec!["orphaned-model".into()])).unwrap();
        config.remove_channel(&id).unwrap();
        view.config["quick_ask"] = json!(config);
        view.committed = view.config.clone();
        let mut events = Vec::new();
        view.poll_quick_ask_models(&Context::default(), &mut events);
        assert!(events.is_empty());
        assert!(!view.quick_ask_models.contains_key(&id));
        assert!(view.config["quick_ask"]["channels"].get(&id).is_none());
    }

    #[test]
    fn credential_drafts_follow_channel_protocol_and_target_but_not_name() {
        let mut config = QuickAskConfig {
            credential_target: "Orbment/Tests/QuickAsk/DraftIsolation".into(),
            ..Default::default()
        };
        let mut state = QuickAskCredentials::default();
        state.select(&config);
        state.secret = "same-channel-draft".into();
        config
            .channels
            .get_mut(&config.active_channel)
            .unwrap()
            .name = "Renamed".into();
        state.select(&config);
        assert_eq!(state.secret, "same-channel-draft");
        config.switch_provider(QuickAskProvider::Anthropic);
        state.select(&config);
        assert!(state.secret.is_empty());
        state.secret = "same-target-draft".into();
        config.credential_target.push_str("/Other");
        state.select(&config);
        assert!(state.secret.is_empty());
        let target = config.credential_target.clone();
        config.add_channel();
        config.switch_provider(QuickAskProvider::Anthropic);
        config.credential_target = target;
        state.secret = "same-protocol-and-target-draft".into();
        state.select(&config);
        assert!(state.secret.is_empty());
        assert_eq!(
            state.identity,
            Some((
                config.active_channel,
                config.provider,
                config.credential_target
            ))
        );
    }

    #[test]
    fn quick_ask_channel_rows_and_request_controls_fit_the_standard_window() {
        let context = Context::default();
        crate::main_host::SharedFonts::install(&context).expect("production settings fonts");
        theme::apply_fluent_dark(&context);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1180.0, 800.0));
        let last_field = label("max_tool_rounds");
        for provider in QuickAskProvider::ALL {
            for show_errors in [false, true] {
                let mut config = orbment_core::config::OrbmentConfig::default();
                let default_channel_names: Vec<_> = config
                    .quick_ask
                    .channels
                    .values()
                    .map(|channel| channel.name.clone())
                    .collect();
                for _ in 0..16 {
                    config.quick_ask.add_channel();
                }
                let id = provider.default_channel_id();
                config.quick_ask.switch_channel(id).unwrap();
                if show_errors {
                    config.quick_ask.channels.get_mut(id).unwrap().name =
                        "很长的自定义渠道名称".repeat(20);
                }
                let target = config.quick_ask.credential_target.clone();
                let mut view = SettingsView::new(json!(config));
                view.select_form_tab("quick_ask", 0);
                view.quick_ask_credentials.identity = Some((id.into(), provider, target));
                view.quick_ask_credentials.saved = Some(true);
                if show_errors {
                    view.quick_ask_credentials.error = true;
                    view.quick_ask_credentials.status = "凭据保存失败，详细错误信息。".repeat(40);
                    view.quick_ask_models.entry(id.into()).or_default().error =
                        Some("模型服务请求失败，详细响应信息。".repeat(40));
                }
                let mut draw = || {
                    context.run(
                        egui::RawInput {
                            screen_rect: Some(screen),
                            ..Default::default()
                        },
                        |context| view.draw(context, &mut Vec::new()),
                    )
                };
                let _ = draw();
                let output = draw();
                for text in [
                    "+ 新增渠道".to_owned(),
                    "删除渠道".to_owned(),
                    "渠道名称与协议".to_owned(),
                    "API 密钥".to_owned(),
                    "接口地址".to_owned(),
                    "模型名称".to_owned(),
                    "思考强度".to_owned(),
                    "凭据目标名称".to_owned(),
                    "请求与工具调用".to_owned(),
                    label("max_tokens"),
                    label("request_timeout_seconds"),
                    last_field.clone(),
                ]
                .into_iter()
                .chain(
                    default_channel_names
                        .iter()
                        .filter(|_| !show_errors)
                        .cloned(),
                ) {
                    let (rect, clip) = output
                        .shapes
                        .iter()
                        .find_map(|clipped| {
                            let egui::Shape::Text(shape) = &clipped.shape else {
                                return None;
                            };
                            (shape.galley.text() == text).then(|| {
                                (
                                    egui::Rect::from_min_size(shape.pos, shape.galley.size()),
                                    clipped.clip_rect,
                                )
                            })
                        })
                        .unwrap_or_else(|| panic!("missing {text} for {provider:?}"));
                    assert!(
                        screen.contains_rect(rect) && clip.contains_rect(rect),
                        "{text} must be fully visible for {provider:?}, errors={show_errors}: {rect:?}"
                    );
                    if text == last_field {
                        assert!(
                            rect.center().y + theme::ROW_HEIGHT * 0.5
                                <= clip.bottom().min(screen.bottom()) + 1.0,
                            "the last request control must fit without scrolling for {provider:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn quick_ask_workbench_only_appears_on_connection_tab() {
        let context = Context::default();
        let config = QuickAskConfig::default();
        let mut view = SettingsView::new(json!({"quick_ask": config}));
        view.selected = "quick_ask".into();
        for index in [0, 1] {
            view.subpages.insert("quick_ask".into(), index);
            let output = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        vec2(1000.0, 1600.0),
                    )),
                    ..Default::default()
                },
                |context| {
                    egui::CentralPanel::default().show(context, |ui| {
                        view.draw_module_form(ui, &mut Vec::new());
                    });
                },
            );
            assert_eq!(
                visible_text(&output, "模型渠道与密钥配置").is_some(),
                index == 0
            );
            assert_eq!(
                visible_text(&output, "请求与工具调用").is_some(),
                index == 0
            );
            for title in [
                "呼出与交互快捷键",
                "全局系统提示词 (System Prompt)",
                "预设快捷指令 (Preset Prompts)",
            ] {
                assert_eq!(visible_text(&output, title).is_some(), index == 1);
            }
        }
    }

    #[test]
    fn disk_panel_survives_settings_and_other_pages_until_disabled() {
        let context = Context::default();
        let config = orbment_core::config::DiskAnalyzerConfig {
            enabled: true,
            ..Default::default()
        };
        let mut view = SettingsView::new(json!({"disk_analyzer": config}));
        let mut events = Vec::new();
        for page in [
            "disk_analysis",
            "disk_top_files",
            "disk_extensions",
            "disk_analyzer",
            "app",
            "disk_analysis",
        ] {
            view.selected = page.into();
            let _ = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 800.0),
                    )),
                    ..Default::default()
                },
                |context| view.draw(context, &mut events),
            );
            assert_eq!(
                view.selected, page,
                "retained disk panel must not steal other pages"
            );
            assert_eq!(view.disk.as_ref().expect("retained disk panel").page, page);
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    SettingsEvent::FileTools(crate::FileToolsAction::DiskOpened)
                ))
                .count(),
            1
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            SettingsEvent::FileTools(crate::FileToolsAction::DiskClosed)
        )));
        view.config["disk_analyzer"]["enabled"] = json!(false);
        let _ = context.run(egui::RawInput::default(), |context| {
            view.draw(context, &mut events);
        });
        assert!(view.disk.is_none());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    SettingsEvent::FileTools(crate::FileToolsAction::DiskClosed)
                ))
                .count(),
            1
        );
    }

    #[test]
    fn pin_history_refresh_keeps_unchanged_textures() {
        let context = Context::default();
        let mut previews = PinPreviews::new(context.clone()).unwrap();
        let key = (PathBuf::from("unchanged.png"), Some(SystemTime::UNIX_EPOCH));
        let texture = context.load_texture(
            "test-pin",
            egui::ColorImage::new([1, 1], egui::Color32::WHITE),
            egui::TextureOptions::LINEAR,
        );
        let id = texture.id();
        previews.current.insert(key.0.clone(), key.clone());
        previews.textures.insert(key.clone(), Ok(texture));
        previews.refresh(&json!([
            {"image_path":"unchanged.png", "is_active":false},
            {"image_path":"new.png"}
        ]));
        assert_eq!(previews.textures[&key].as_ref().unwrap().id(), id);
        previews.refresh(&json!([{"image_path":"new.png"}]));
        assert!(previews.textures.is_empty());
    }

    #[test]
    fn pin_preview_worker_reuses_matching_file_timestamp() {
        let path = std::env::temp_dir().join(format!(
            "orbment-pin-preview-{}-{}.png",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let (key, image) = load_pin_preview(PinPreviewRequest {
            path: path.clone(),
            previous: None,
            scale: 2.0,
        });
        assert!(image.unwrap().is_ok());
        let (_, unchanged) = load_pin_preview(PinPreviewRequest {
            path: path.clone(),
            previous: Some(key),
            scale: 2.0,
        });
        assert!(unchanged.is_none());
        let (_, changed) = load_pin_preview(PinPreviewRequest {
            path: path.clone(),
            previous: Some((path.clone(), Some(SystemTime::UNIX_EPOCH))),
            scale: 2.0,
        });
        assert!(changed.unwrap().is_ok());
        std::fs::remove_file(path).unwrap();
    }

    fn draw_settings(
        context: &Context,
        view: &mut SettingsView,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(700.0, 320.0),
                )),
                events,
                ..Default::default()
            },
            |context| view.draw(context, &mut Vec::new()),
        )
    }

    fn visible_text(output: &egui::FullOutput, text: &str) -> Option<egui::Rect> {
        output.shapes.iter().rev().find_map(|clipped| {
            let egui::Shape::Text(shape) = &clipped.shape else {
                return None;
            };
            let rect = egui::Rect::from_min_size(shape.pos, shape.galley.size());
            (shape.galley.text() == text && clipped.clip_rect.intersects(rect)).then_some(rect)
        })
    }
    #[test]
    fn pin_cards_dispatch_actions_for_the_selected_active_or_historical_id() {
        for (active, label, action, pin_action) in [
            (true, "vision.pins.close", "vision.pin_close", None),
            (false, "vision.pins.restore", "vision.pin_restore", None),
            (false, "vision.pins.delete", "vision.pin_delete", None),
            (
                true,
                "vision.pins.copy",
                "vision.pin_action",
                Some("copy_image"),
            ),
            (
                false,
                "vision.pins.save",
                "vision.pin_action",
                Some("save_image"),
            ),
        ] {
            let context = Context::default();
            let mut view = SettingsView::new(json!({}));
            let entry = json!({
                "id":"pin-under-test", "title":"Example pin", "image_path":null,
                "bounds":{"x":42,"y":73,"width":640,"height":480},
                "created_at":"2026-10-22 10:00:00", "closed_at":"2026-10-22 11:00:00",
                "is_active":active,
            });
            let mut actions = Vec::new();
            let mut draw = |input| {
                context.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            vec2(1000.0, 400.0),
                        )),
                        events: input,
                        ..Default::default()
                    },
                    |context| {
                        egui::CentralPanel::default().show(context, |ui| {
                            view.draw_pin_card(ui, &entry, active, &mut actions);
                        });
                    },
                )
            };
            let _ = draw(Vec::new());
            let output = draw(Vec::new());
            let pointer = visible_text(&output, &tr(label))
                .expect("card action")
                .center();
            for pressed in [true, false] {
                let _ = draw(vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::PointerButton {
                        pos: pointer,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
            }
            assert!(matches!(actions.as_slice(), [SettingsEvent::Action {
                action: actual, params,
            }] if actual == action && params["id"] == "pin-under-test"
                && params.get("action").and_then(Value::as_str) == pin_action));
        }
    }

    fn scroll_settings(
        context: &Context,
        view: &mut SettingsView,
        pointer: egui::Pos2,
    ) -> egui::FullOutput {
        for _ in 0..4 {
            let _ = draw_settings(
                context,
                view,
                vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -10_000.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        draw_settings(context, view, Vec::new())
    }

    #[test]
    fn monitor_display_modes_commit_both_windows_and_open_matching_settings() {
        for (initial_taskbar, initial_floating, _) in MONITOR_DISPLAY_MODES {
            for (taskbar, floating, _) in MONITOR_DISPLAY_MODES {
                if (taskbar, floating) == (initial_taskbar, initial_floating) {
                    continue;
                }
                let mut view = SettingsView::new(json!({
                    "monitor": {
                        "enabled": true,
                        "taskbar": {
                            "enabled": initial_taskbar,
                            "taskbar_left_space_win11": 160,
                            "taskbar_right_space_win11": 280
                        },
                        "floating": { "enabled": initial_floating, "pos_x": 123 }
                    }
                }));
                view.subpages.insert("monitor".into(), 2);
                view.search = "previous filter".into();
                let mut events = Vec::new();
                view.set_monitor_display(taskbar, floating, &mut events);
                assert_eq!(events.len(), 2);
                for (event, field, enabled) in [
                    (&events[0], "taskbar", taskbar),
                    (&events[1], "floating", floating),
                ] {
                    let SettingsEvent::ConfigChanged { path, value } = event else {
                        panic!("mode selection must apply configuration immediately");
                    };
                    assert_eq!(path, &format!("monitor.{field}"));
                    assert_eq!(value["enabled"], enabled);
                    assert_eq!(*value, view.config["monitor"][field]);
                }
                assert_eq!(
                    view.config["monitor"]["taskbar"]["taskbar_left_space_win11"],
                    160
                );
                assert_eq!(
                    view.config["monitor"]["taskbar"]["taskbar_right_space_win11"],
                    280
                );
                assert_eq!(view.config["monitor"]["floating"]["pos_x"], 123);
                assert_eq!(
                    view.subpages["monitor"],
                    if taskbar == floating {
                        2
                    } else {
                        usize::from(floating)
                    }
                );
                if taskbar != floating {
                    assert!(view.search.is_empty());
                }
            }
        }
    }

    #[test]
    fn monitor_notices_and_full_diagnostics_render_on_separate_pages() {
        let context = Context::default();
        let mut view = SettingsView::new(json!({ "app": {}, "monitor": {} }));
        assert!(view.monitor_notices.is_empty());
        view.monitor_notices = vec!["Monitor access is limited".into()];
        view.diagnostics = vec!["Full diagnostic details".into()];
        for (page, notice_visible, diagnostic_visible) in [
            ("monitor", true, false),
            ("diagnostics", false, true),
            ("app", false, false),
        ] {
            view.selected = page.into();
            let _ = draw_settings(&context, &mut view, Vec::new());
            let output = draw_settings(&context, &mut view, Vec::new());
            assert_eq!(
                visible_text(&output, "Monitor access is limited").is_some(),
                notice_visible,
                "{page}"
            );
            assert_eq!(
                visible_text(&output, "Full diagnostic details").is_some(),
                diagnostic_visible,
                "{page}"
            );
        }
    }

    #[test]
    fn monitor_notices_scroll_with_settings_below_the_fixed_heading() {
        let context = Context::default();
        let mut view = SettingsView::new(json!({
            "monitor": { "sample_interval_ms": 1000 }
        }));
        view.selected = "monitor".into();
        view.subpages.insert("monitor".into(), 2);
        view.monitor_notices = (0..40)
            .map(|index| format!("Monitor notice {index}"))
            .collect();
        let _ = draw_settings(&context, &mut view, Vec::new());
        let before = draw_settings(&context, &mut view, Vec::new());
        let heading = visible_text(&before, &nav_label("monitor")).unwrap();
        let first_notice = visible_text(&before, "Monitor notice 0").unwrap();
        assert!(visible_text(&before, &label("sample_interval_ms")).is_none());

        let after = scroll_settings(&context, &mut view, first_notice.center());
        assert!(visible_text(&after, "Monitor notice 0").is_none());
        assert!(visible_text(&after, &label("sample_interval_ms")).is_some());
        assert_eq!(
            visible_text(&after, &nav_label("monitor")).unwrap(),
            heading
        );
    }

    #[test]
    fn diagnostics_scroll_to_entries_beyond_the_viewport() {
        let context = Context::default();
        let mut view = SettingsView::new(json!({}));
        view.selected = "diagnostics".into();
        view.diagnostics = (0..40)
            .map(|index| format!("Diagnostic entry {index}"))
            .collect();
        let _ = draw_settings(&context, &mut view, Vec::new());
        let before = draw_settings(&context, &mut view, Vec::new());
        assert!(visible_text(&before, "Diagnostic entry 0").is_some());
        assert!(visible_text(&before, "Diagnostic entry 39").is_none());

        let after = scroll_settings(&context, &mut view, egui::pos2(500.0, 200.0));
        assert!(visible_text(&after, "Diagnostic entry 0").is_none());
        assert!(visible_text(&after, "Diagnostic entry 39").is_some());
    }

    #[test]
    fn navigation_opens_disabled_modules_without_enabling_them() {
        let context = Context::default();
        let mut view = SettingsView::new(json!({
            "app": {},
            "vision": { "enabled": false, "shell_context_menu": true }
        }));
        let _ = draw_settings(&context, &mut view, Vec::new());
        let output = draw_settings(&context, &mut view, Vec::new());
        let entry = visible_text(&output, &nav_label("vision")).unwrap();
        let pointer = entry.center();
        for pressed in [true, false] {
            let _ = draw_settings(
                &context,
                &mut view,
                vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::PointerButton {
                        pos: pointer,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert_eq!(view.selected, "vision");
        assert_eq!(view.config["vision"]["enabled"], false);
    }

    #[test]
    fn navigation_switch_changes_module_without_changing_page() {
        for page in ["app", "vision"] {
            let context = Context::default();
            let mut view = SettingsView::new(json!({
                "app": {},
                "vision": { "enabled": false, "shell_context_menu": true }
            }));
            view.selected = page.into();
            let mut events = Vec::new();
            let draw = |view: &mut SettingsView,
                        input: Vec<egui::Event>,
                        events: &mut Vec<SettingsEvent>| {
                context.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            vec2(700.0, 320.0),
                        )),
                        events: input,
                        ..Default::default()
                    },
                    |context| view.draw(context, events),
                )
            };
            for enabled in [true, false] {
                let _ = draw(&mut view, Vec::new(), &mut events);
                let output = draw(&mut view, Vec::new(), &mut events);
                let switch = output
                    .shapes
                    .iter()
                    .find_map(|shape| {
                        let egui::Shape::Rect(shape) = &shape.shape else {
                            return None;
                        };
                        (shape.rect.size() == vec2(28.0, 16.0)).then_some(shape.rect)
                    })
                    .expect("sidebar module switch");
                events.clear();
                for pressed in [true, false] {
                    let _ = draw(
                        &mut view,
                        vec![
                            egui::Event::PointerMoved(switch.center()),
                            egui::Event::PointerButton {
                                pos: switch.center(),
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                        &mut events,
                    );
                }
                assert_eq!(view.selected, page);
                assert_eq!(view.config["vision"]["enabled"], enabled);
                assert_eq!(view.committed["vision"]["enabled"], enabled);
                assert!(matches!(
                    events.as_slice(),
                    [SettingsEvent::ConfigChanged { path, value }]
                        if path == "vision.enabled" && value == &Value::Bool(enabled)
                ));
            }
        }
    }

    #[test]
    fn sidebar_search_is_available_on_information_pages() {
        let context = Context::default();
        let mut view = SettingsView::new(json!({}));
        view.selected = "diagnostics".into();
        let _ = draw_settings(&context, &mut view, Vec::new());
        let output = draw_settings(&context, &mut view, Vec::new());
        let search = visible_text(&output, &tr("settings.search")).unwrap();
        assert!(search.right() < 230.0);
    }

    #[test]
    fn navigation_scroll_keeps_title_fixed_and_exposes_last_page() {
        let context = Context::default();
        let modules: serde_json::Map<String, Value> = (0..40)
            .map(|index| (format!("module_{index:02}"), json!({})))
            .collect();
        let mut view = SettingsView::new(Value::Object(modules));
        assert!(view.nav_groups().iter().all(|(group, _)| group != "more"));
        // Secondary modules surface in the navigation only for a matching search.
        view.search = "module_".into();
        let _ = draw_settings(&context, &mut view, Vec::new());
        let before = draw_settings(&context, &mut view, Vec::new());
        let title = visible_text(&before, &tr("settings.brand")).unwrap();
        assert!(visible_text(&before, &nav_label("about")).is_none());

        let after = scroll_settings(&context, &mut view, egui::pos2(100.0, 200.0));
        assert_eq!(visible_text(&after, &tr("settings.brand")).unwrap(), title);
        let about = visible_text(&after, &nav_label("about")).unwrap();
        let pointer = about.center();
        for pressed in [true, false] {
            let _ = draw_settings(
                &context,
                &mut view,
                vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::PointerButton {
                        pos: pointer,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert_eq!(view.selected, "about");
    }

    #[test]
    fn simple_hotkey_permission_is_not_edited_as_a_keyboard_chord() {
        let context = Context::default();
        let mut value = Value::Bool(true);
        let path = "vision.allow_simple_hotkey";
        let id = egui::Id::new(("hotkey-recorder", path));
        let mut changed = false;
        for pressed in [false, true] {
            let input = egui::RawInput {
                events: if pressed {
                    vec![egui::Event::Key {
                        key: egui::Key::F2,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }]
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            let _ = context.run(input, |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    if !pressed {
                        crate::hotkeys::set_recording(true);
                        ui.data_mut(|data| {
                            data.insert_temp(egui::Id::new("hotkey-recorder-active"), id);
                        });
                    }
                    changed |= edit_value(ui, "allow_simple_hotkey", path, &mut value);
                });
            });
        }
        crate::hotkeys::cancel_recorder(&context);
        assert_eq!(value, Value::Bool(true));
        assert!(!changed);
    }

    #[test]
    fn optional_hotkey_drafts_only_commit_valid_nonempty_chords() {
        for (text, expected) in [
            ("", Value::Null),
            ("Ctrl+", Value::Null),
            ("Ctrl+Alt+F12", json!("Ctrl+Alt+F12")),
        ] {
            let context = Context::default();
            let mut view = SettingsView::new(json!({ "memory": { "clean_hotkey": null } }));
            let mut changed = false;
            let mut draw = |context: &Context, initialize: bool| {
                egui::CentralPanel::default().show(context, |ui| {
                    let id = egui::Id::new(("hotkey-recorder", "memory.clean_hotkey"));
                    if initialize {
                        crate::hotkeys::set_recording(true);
                        ui.data_mut(|data| {
                            data.insert_temp(egui::Id::new("hotkey-recorder-active"), id)
                        });
                    }
                    changed |= edit_value(
                        ui,
                        "clean_hotkey",
                        "memory.clean_hotkey",
                        &mut view.config["memory"]["clean_hotkey"],
                    );
                });
            };
            let _ = context.run(egui::RawInput::default(), |context| draw(context, true));
            let input = egui::RawInput {
                events: vec![
                    egui::Event::Text(text.into()),
                    egui::Event::Key {
                        key: egui::Key::Enter,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            };
            let _ = context.run(input, |context| draw(context, false));
            crate::hotkeys::cancel_recorder(&context);
            assert_eq!(view.config["memory"]["clean_hotkey"], expected);
            assert_eq!(changed, !expected.is_null());
            let mut events = Vec::new();
            view.flush_pending(&mut events);
            assert_eq!(events.len(), usize::from(!expected.is_null()));
        }
    }

    #[test]
    fn unrelated_config_updates_preserve_drafts_until_commit() {
        let mut view = SettingsView::new(json!({
            "audio": { "playback_hotkey": "Ctrl+Alt+F11" },
            "monitor": { "sample_interval_ms": 1000 }
        }));
        view.config["audio"]["playback_hotkey"] = json!("Ctrl+Alt+F12");
        view.sync_config(json!({
            "audio": { "playback_hotkey": "Ctrl+Alt+F11" },
            "monitor": { "sample_interval_ms": 2000 }
        }));
        let mut events = Vec::new();
        view.flush_pending(&mut events);
        assert_eq!(view.config["monitor"]["sample_interval_ms"], 2000);
        assert!(
            matches!(events.as_slice(), [SettingsEvent::ConfigChanged { path, value }]
            if path == "audio.playback_hotkey" && value == "Ctrl+Alt+F12")
        );
        events.clear();
        view.flush_pending(&mut events);
        assert!(events.is_empty());
    }

    #[test]
    fn profile_creation_produces_distinct_nonempty_ids() {
        let mut profiles = vec![json!({ "id": "profile-1" }), json!({ "id": "profile-3" })];
        let first = array_item("profiles", &profiles);
        let first_id = first["id"].as_str().unwrap().to_owned();
        profiles.push(first);
        let second = array_item("profiles", &profiles);
        assert!(!first_id.is_empty());
        assert_ne!(first_id, second["id"].as_str().unwrap());
        assert!(!profiles.iter().any(|profile| profile["id"] == second["id"]));
    }

    #[test]
    fn diagnostics_normalize_chords_and_ignore_disabled_modules() {
        let config = json!({
            "audio": { "enabled": true, "playback_hotkey": "Ctrl+Alt+F11", "recording_hotkey": "Alt+Ctrl+F11" },
            "clipboard": { "enabled": false, "open_clipboard_hotkey": "Ctrl+Alt+F11" }
        });
        assert_eq!(hotkey_diagnostics(&config).len(), 1);
    }
    #[test]
    fn selecting_save_directory_preserves_filename_template() {
        assert_eq!(
            save_pattern_in_directory(
                r"C:\old\Orbment_$yyyy-MM-dd_HH-mm-ss$.png",
                r"D:\Screenshots"
            ),
            r"D:\Screenshots\Orbment_$yyyy-MM-dd_HH-mm-ss$.png"
        );
    }
}
