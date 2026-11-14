use crate::i18n::{quick_ask_default_channel_name, tr};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item, Value};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct OrbmentConfig {
    pub app: AppConfig,
    pub mcp: McpConfig,
    pub audio: AudioConfig,
    pub monitor: MonitorConfig,
    pub memory: MemoryConfig,
    #[serde(deserialize_with = "deserialize_clipboard")]
    pub clipboard: ClipboardConfig,
    pub file_search: FileSearchConfig,
    pub disk_analyzer: DiskAnalyzerConfig,
    pub vision: VisionConfig,
    pub utilities: UtilitiesConfig,
    pub quick_ask: QuickAskConfig,
    pub hotkey_engine: HotkeyEngineConfig,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub autostart: bool,
    pub language: String,
    pub idle_destroy_seconds: u32,
    pub tray_middle_click_action: String,
    pub ocr_engine_tier: String,
    pub ocr_idle_unload_seconds: u32,
    pub log_level: String,
    pub log_retention_days: usize,
}
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            autostart: false,
            language: "zh-CN".into(),
            idle_destroy_seconds: 60,
            tray_middle_click_action: "audio.switch_playback".into(),
            ocr_engine_tier: "Ask".into(),
            ocr_idle_unload_seconds: 300,
            log_level: "warn".into(),
            log_retention_days: 14,
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpConfig {
    pub enabled: bool,
    pub allow_memory_clean: bool,
    pub allow_process_kill: bool,
    pub allow_process_reap: bool,
    pub allow_file_unlock: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum CycleMode {
    #[default]
    Available,
    All,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct AudioDeviceSelection {
    pub id: String,
    #[serde(default, alias = "name_clean")]
    pub name: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct KnobRemapConfig {
    pub intercept_volume_mute: bool,
    pub action: String,
}
impl Default for KnobRemapConfig {
    fn default() -> Self {
        Self {
            intercept_volume_mute: false,
            action: "audio.switch_playback".into(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AudioConfig {
    pub enabled: bool,
    pub selected_playback_devices: Vec<AudioDeviceSelection>,
    pub selected_recording_devices: Vec<AudioDeviceSelection>,
    pub cycle_mode: CycleMode,
    pub playback_hotkey: String,
    pub recording_hotkey: String,
    pub mute_recording_hotkey: String,
    pub change_communications: bool,
    pub switch_foreground_program: bool,
    pub keep_volume_enabled: bool,
    pub auto_add_new_devices: bool,
    pub quick_menu_enabled: bool,
    pub tray_icon: AudioTrayIconConfig,
    pub switch_icon_mode: String,
    pub icon_double_click: String,
    pub tooltip_info: String,
    pub switch_device_notification: String,
    pub switch_profile_notification: String,
    pub mic_mute_notification: String,
    pub banner_position: String,
    pub custom_banner_position: [i32; 2],
    pub banner_opacity_pct: u8,
    pub max_number_notifications: u8,
    pub notify_using_primary_screen: bool,
    pub banner_display_info: String,
    pub mic_mute_banner_mode: String,
    pub mic_unmute_banner_mode: String,
    pub profiles: Vec<AudioProfile>,
    pub app_sound_rules: Vec<AppSoundRule>,
    pub knob_remap: KnobRemapConfig,
    pub banner_on_screen_time_ms: u32,
}
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            selected_playback_devices: vec![],
            selected_recording_devices: vec![],
            cycle_mode: CycleMode::Available,
            playback_hotkey: "Ctrl+Alt+F11".into(),
            recording_hotkey: "Ctrl+Alt+F7".into(),
            mute_recording_hotkey: String::new(),
            change_communications: false,
            switch_foreground_program: false,
            keep_volume_enabled: false,
            auto_add_new_devices: false,
            quick_menu_enabled: false,
            tray_icon: AudioTrayIconConfig::default(),
            switch_icon_mode: "Never".into(),
            icon_double_click: "SwitchDevice".into(),
            tooltip_info: "Playback".into(),
            switch_device_notification: "Banner".into(),
            switch_profile_notification: "None".into(),
            mic_mute_notification: "None".into(),
            banner_position: "TopLeft".into(),
            custom_banner_position: [0, 0],
            banner_opacity_pct: 100,
            max_number_notifications: 5,
            notify_using_primary_screen: false,
            banner_display_info: "Full".into(),
            mic_mute_banner_mode: "Persistent".into(),
            mic_unmute_banner_mode: "None".into(),
            profiles: Vec::new(),
            app_sound_rules: Vec::new(),
            knob_remap: KnobRemapConfig::default(),
            banner_on_screen_time_ms: 3000,
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AudioTrayIconConfig {
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AudioProfile {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub playback_device_id: Option<String>,
    pub recording_device_id: Option<String>,
    pub hotkey: Option<String>,
    pub process_path: Option<String>,
    pub on_process_start: bool,
    pub on_window_activate: bool,
    pub change_communications: bool,
}
impl Default for AudioProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            playback_device_id: None,
            recording_device_id: None,
            hotkey: None,
            process_path: None,
            on_process_start: false,
            on_window_activate: false,
            change_communications: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AppSoundRule {
    pub process_path: String,
    pub playback_device_id: Option<String>,
    pub recording_device_id: Option<String>,
    pub enabled: bool,
}
impl Default for AppSoundRule {
    fn default() -> Self {
        Self {
            process_path: String::new(),
            playback_device_id: None,
            recording_device_id: None,
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MetricLabels {
    pub up: String,
    pub down: String,
    pub cpu: String,
    pub memory: String,
    pub gpu: String,
    pub disk: String,
    pub cpu_temperature: String,
    pub gpu_temperature: String,
    pub disk_temperature: String,
    pub cpu_frequency: String,
    pub today_traffic: String,
    pub total_speed: String,
    pub disk_read: String,
    pub disk_write: String,
}
impl Default for MetricLabels {
    fn default() -> Self {
        Self {
            up: tr("monitor.up"),
            down: tr("monitor.down"),
            cpu: tr("monitor.cpu"),
            memory: tr("monitor.memory"),
            gpu: tr("monitor.gpu"),
            disk: tr("monitor.disk"),
            cpu_temperature: tr("monitor.cpu_temperature"),
            gpu_temperature: tr("monitor.gpu_temperature"),
            disk_temperature: tr("monitor.disk_temperature"),
            cpu_frequency: tr("monitor.cpu_frequency"),
            today_traffic: tr("monitor.today_traffic"),
            total_speed: tr("monitor.total_speed"),
            disk_read: tr("monitor.disk_read"),
            disk_write: tr("monitor.disk_write"),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TaskbarConfig {
    pub enabled: bool,
    pub display_mask: u32,
    pub font_name: String,
    pub size: f32,
    pub style: u32,
    pub back_color: u32,
    pub transparent_color: u32,
    pub status_bar_color: u32,
    pub specify_each_item_color: bool,
    pub text_colors: Vec<u32>,
    pub labels: MetricLabels,
    pub on_left: bool,
    pub snap: bool,
    pub transparent_background: bool,
    pub auto_set_background_color: bool,
    pub auto_adapt_light_theme: bool,
    pub horizontal_arrange: bool,
    pub value_right_align: bool,
    pub speed_short_mode: bool,
    pub separate_value_unit_with_space: bool,
    pub show_tool_tip: bool,
    pub digits_number: u8,
    pub memory_display_mode: u8,
    pub item_space: u32,
    pub vertical_margin: i32,
    pub item_order: Vec<u8>,
    pub unit_byte: bool,
    pub speed_unit: u8,
    pub hide_unit: bool,
    pub hide_percent: bool,
    pub double_click_action: u8,
    pub double_click_exe: String,
    pub show_status_bar: bool,
    pub show_netspeed_figure: bool,
    pub netspeed_figure_max_value: u32,
    pub netspeed_figure_max_value_unit: u8,
    pub cm_graph_type: bool,
    pub show_graph_dashed_box: bool,
    pub graph_color_following_system: bool,
    pub taskbar_left_space_win11: u32,
    pub taskbar_right_space_win11: u32,
}
impl Default for TaskbarConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            display_mask: 3,
            font_name: tr("monitor.font"),
            size: 9.0,
            style: 0,
            back_color: 0,
            transparent_color: 0,
            status_bar_color: 0x005a5a5a,
            specify_each_item_color: false,
            text_colors: vec![0xffffff; 26],
            labels: MetricLabels::default(),
            on_left: false,
            snap: false,
            transparent_background: true,
            auto_set_background_color: false,
            auto_adapt_light_theme: true,
            horizontal_arrange: false,
            value_right_align: false,
            speed_short_mode: false,
            separate_value_unit_with_space: true,
            show_tool_tip: true,
            digits_number: 4,
            memory_display_mode: 0,
            item_space: 4,
            vertical_margin: 0,
            item_order: vec![0, 1, 10, 4, 2, 6, 5, 3, 11, 7, 8, 9, 12],
            unit_byte: true,
            speed_unit: 0,
            hide_unit: false,
            hide_percent: false,
            double_click_action: 0,
            double_click_exe: "C:\\Windows\\system32\\Taskmgr.exe".into(),
            show_status_bar: false,
            show_netspeed_figure: false,
            netspeed_figure_max_value: 512,
            netspeed_figure_max_value_unit: 0,
            cm_graph_type: true,
            show_graph_dashed_box: false,
            graph_color_following_system: false,
            taskbar_left_space_win11: 160,
            taskbar_right_space_win11: 280,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub enabled: bool,
    pub sample_interval_ms: u32,
    pub cpu_usage_by_times: bool,
    pub hardware_monitor_mask: u32,
    pub hard_disk_name: String,
    pub cpu_core_name: String,
    pub pawnio_module_dir: String,
    pub adapter_auto_select: bool,
    pub adapter_select_all: bool,
    pub selected_adapter_name: String,
    pub hidden_adapters: Vec<String>,
    pub taskbar: TaskbarConfig,
    pub floating: FloatingConfig,
    pub labels: MetricLabels,
    pub memory_display_mode: u8,
    pub double_click_action: u8,
    pub double_click_exe: String,
    pub alerts: MonitorAlerts,
    pub unit_byte: bool,
    pub speed_unit: u8,
    pub speed_short_mode: bool,
    pub separate_value_unit_with_space: bool,
    pub hide_unit: bool,
    pub hide_percent: bool,
}
impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: 1000,
            cpu_usage_by_times: true,
            hardware_monitor_mask: 0,
            hard_disk_name: String::new(),
            cpu_core_name: "Core Average".into(),
            pawnio_module_dir: String::new(),
            adapter_auto_select: true,
            adapter_select_all: false,
            selected_adapter_name: String::new(),
            hidden_adapters: vec![],
            taskbar: TaskbarConfig::default(),
            floating: FloatingConfig::default(),
            labels: MetricLabels::default(),
            memory_display_mode: 0,
            double_click_action: 0,
            double_click_exe: std::env::var("SystemRoot")
                .map(|root| format!("{root}\\System32\\Taskmgr.exe"))
                .unwrap_or_default(),
            alerts: MonitorAlerts::default(),
            unit_byte: true,
            speed_unit: 0,
            speed_short_mode: false,
            separate_value_unit_with_space: true,
            hide_unit: false,
            hide_percent: false,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct FloatingConfig {
    pub enabled: bool,
    pub pos_x: i32,
    pub pos_y: i32,
    pub opacity_pct: u8,
    pub always_on_top: bool,
    pub lock_position: bool,
    pub mouse_penetrate: bool,
    pub hide_on_fullscreen: bool,
    pub allow_out_of_border: u8,
    pub show_cpu_memory: bool,
    pub swap_up_down: bool,
    pub font_name: String,
    pub size: f32,
    pub style: u32,
    pub specify_each_item_color: bool,
    pub text_colors: Vec<u32>,
    pub skin_selected: String,
    pub allow_skin_cover_font: bool,
    pub allow_skin_cover_text: bool,
    pub snap: bool,
}
impl Default for FloatingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pos_x: -1,
            pos_y: -1,
            opacity_pct: 80,
            always_on_top: true,
            lock_position: false,
            mouse_penetrate: false,
            hide_on_fullscreen: true,
            allow_out_of_border: 0,
            show_cpu_memory: false,
            swap_up_down: false,
            font_name: tr("monitor.font"),
            size: 10.0,
            style: 0,
            specify_each_item_color: false,
            text_colors: vec![0xffffff; 13],
            skin_selected: String::new(),
            allow_skin_cover_font: true,
            allow_skin_cover_text: true,
            snap: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MonitorAlerts {
    pub traffic_enabled: bool,
    pub traffic_value: u64,
    pub traffic_unit: String,
    pub memory_enabled: bool,
    pub memory_pct: u8,
    pub cpu_temperature_enabled: bool,
    pub cpu_temperature_c: f64,
    pub gpu_temperature_enabled: bool,
    pub gpu_temperature_c: f64,
    pub disk_temperature_enabled: bool,
    pub disk_temperature_c: f64,
}
impl Default for MonitorAlerts {
    fn default() -> Self {
        Self {
            traffic_enabled: false,
            traffic_value: 200,
            traffic_unit: "MB".into(),
            memory_enabled: false,
            memory_pct: 80,
            cpu_temperature_enabled: false,
            cpu_temperature_c: 80.0,
            gpu_temperature_enabled: false,
            gpu_temperature_c: 80.0,
            disk_temperature_enabled: false,
            disk_temperature_c: 80.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub auto_clean_threshold_enabled: bool,
    pub auto_clean_interval_enabled: bool,
    pub clean_hotkey: Option<String>,
    pub clean_regions_mask: u32,
    pub warn_on_standby_modified: bool,
    pub confirm_before_clean: bool,
    pub auto_clean_threshold_pct: u8,
    pub auto_clean_interval_mins: u32,
    pub show_clean_result_notification: bool,
    pub notification_sound: bool,
    pub tray_icon: MemoryTrayIconConfig,
    pub reaper: ReaperConfig,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_clean_threshold_enabled: false,
            auto_clean_interval_enabled: false,
            clean_hotkey: Some("Ctrl+Shift+F1".into()),
            clean_regions_mask: 0x07,
            warn_on_standby_modified: true,
            confirm_before_clean: false,
            auto_clean_threshold_pct: 85,
            auto_clean_interval_mins: 30,
            show_clean_result_notification: true,
            notification_sound: false,
            tray_icon: MemoryTrayIconConfig::default(),
            reaper: ReaperConfig::default(),
        }
    }
}

impl MemoryConfig {
    pub fn needs_background_service(&self) -> bool {
        self.enabled
            && (self.auto_clean_threshold_enabled
                || self.auto_clean_interval_enabled
                || self.tray_icon.enabled)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryTrayIconConfig {
    pub enabled: bool,
    pub transparent_bg: bool,
    pub show_border: bool,
    pub round_corners: bool,
    pub antialias: bool,
    pub font: String,
    pub text_color: u32,
    pub bg_color: u32,
    pub warn_level_pct: u8,
    pub warn_color: u32,
    pub danger_level_pct: u8,
    pub danger_color: u32,
    pub middle_click_action: MemoryTrayAction,
}
impl Default for MemoryTrayIconConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transparent_bg: false,
            show_border: false,
            round_corners: false,
            antialias: true,
            font: "Segoe UI".into(),
            text_color: 0xffffffff,
            bg_color: 0xff008000,
            warn_level_pct: 70,
            warn_color: 0xffffff00,
            danger_level_pct: 90,
            danger_color: 0xffff0000,
            middle_click_action: MemoryTrayAction::CleanMemory,
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum MemoryTrayAction {
    CleanMemory,
    ToggleWindow,
    OpenTaskMgr,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ProcessFamily {
    pub name: String,
    pub owners: Vec<String>,
    pub companions: Vec<String>,
    pub exemptions: Vec<String>,
    pub path_prefix: Option<String>,
    pub include_descendants: bool,
}
impl Default for ProcessFamily {
    fn default() -> Self {
        Self {
            name: String::new(),
            owners: vec![],
            companions: vec![],
            exemptions: vec![],
            path_prefix: None,
            include_descendants: true,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ReaperConfig {
    pub families: Vec<ProcessFamily>,
    pub close_timeout_seconds: u32,
    pub show_in_treemap: bool,
}
impl Default for ReaperConfig {
    fn default() -> Self {
        Self {
            families: vec![],
            close_timeout_seconds: 5,
            show_in_treemap: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum TopmostTarget {
    Foreground,
    UnderCursor,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum ColorFormat {
    Hex,
    Rgb,
    Hsl,
    Oklch,
    Hsb,
    Cmyk,
    Oklab,
    Lab,
    Bgr,
    Custom,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum KeepAwakeMode {
    System,
    Display,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum KeepAwakeDuration {
    Indefinite,
    Minutes30,
    Hour1,
    Hours2,
    Process,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct UtilitiesConfig {
    pub enabled: bool,
    pub topmost_hotkey: Option<String>,
    pub topmost_target: TopmostTarget,
    pub topmost_border_color: u32,
    pub color_picker_hotkey: Option<String>,
    pub color_format: ColorFormat,
    pub color_template: String,
    pub keep_awake_hotkey: Option<String>,
    pub keep_awake_mode: KeepAwakeMode,
    pub keep_awake_duration: KeepAwakeDuration,
    pub keep_awake_process_id: Option<u32>,
    pub brightness_hotkey_up: Option<String>,
    pub brightness_hotkey_down: Option<String>,
    pub brightness_step_pct: u8,
    pub brightness_debounce_ms: u32,
}
impl Default for UtilitiesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            topmost_hotkey: Some("Ctrl+Win+T".into()),
            topmost_target: TopmostTarget::Foreground,
            topmost_border_color: 0x00e6915e,
            color_picker_hotkey: None,
            color_format: ColorFormat::Hex,
            color_template: "%ReX%GrX%BlX".into(),
            keep_awake_hotkey: None,
            keep_awake_mode: KeepAwakeMode::System,
            keep_awake_duration: KeepAwakeDuration::Indefinite,
            keep_awake_process_id: None,
            brightness_hotkey_up: None,
            brightness_hotkey_down: None,
            brightness_step_pct: 10,
            brightness_debounce_ms: 100,
        }
    }
}
// Keep field defaults beside their declarations so sparse imported sections and new
// installations use exactly the same presets.
macro_rules! config_section {
    ($name:ident { $($field:ident: $ty:ty = $default:expr),* $(,)? }) => {
        #[derive(Clone, Debug, Deserialize, Serialize)]
        #[serde(default)]
        pub struct $name {
            $(pub $field: $ty,)*
        }
        impl Default for $name {
            fn default() -> Self {
                Self { $($field: $default,)* }
            }
        }
    };
}

config_section!(ClipboardConfig {
    enabled: bool = true,
    open_clipboard_hotkey: String = "Alt+C".into(),
    open_preference_hotkey: String = String::new(),
    replace_win_v: bool = false,
    quick_paste: ClipboardQuickPasteConfig = Default::default(),
    capture: ClipboardCaptureConfig = Default::default(),
    content: ClipboardContentConfig = Default::default(),
    display: ClipboardDisplayConfig = Default::default(),
    sensitive: ClipboardSensitiveConfig = Default::default(),
    history: ClipboardHistoryConfig = Default::default(),
    search: ClipboardSearchConfig = Default::default(),
    window: ClipboardWindowConfig = Default::default(),
    preview: ClipboardPreviewConfig = Default::default(),
    feedback: ClipboardFeedbackConfig = Default::default(),
    filters: ClipboardFiltersConfig = Default::default(),
});

fn deserialize_clipboard<'de, D>(deserializer: D) -> std::result::Result<ClipboardConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut value = serde_json::Value::deserialize(deserializer)?;
    if let Some(object) = value.as_object_mut()
        && let Some(idle) = object.remove("idle_destroy_seconds")
    {
        let window = object
            .entry("window")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(window) = window.as_object_mut() {
            window.entry("idle_destroy_seconds").or_insert(idle);
        }
    }
    ClipboardConfig::deserialize(value).map_err(serde::de::Error::custom)
}

impl ClipboardConfig {
    pub fn validate(&self) -> Result<()> {
        let kinds = ["files", "image", "html", "rtf", "text"];
        ensure!(
            self.capture.order.len() == kinds.len()
                && kinds.iter().all(|kind| {
                    self.capture
                        .order
                        .iter()
                        .filter(|entry| entry.as_str() == *kind)
                        .count()
                        == 1
                }),
            "{}: clipboard.capture.order",
            tr("config.invalid_value")
        );
        for (key, value, choices) in [
            (
                "content.auto_paste",
                self.content.auto_paste.as_str(),
                &[
                    "disabled",
                    "singleClickPaste",
                    "doubleClickPaste",
                    "singleClickCopy",
                    "doubleClickCopy",
                ][..],
            ),
            (
                "content.middle_click",
                self.content.middle_click.as_str(),
                &[
                    "disabled",
                    "singleClickPaste",
                    "singleClickPastePlain",
                    "singleClickCopy",
                    "singleClickCopyPlain",
                    "pastePlain",
                    "delete",
                ][..],
            ),
            (
                "content.sort",
                self.content.sort.as_str(),
                &["updatedAtDesc", "createdAtDesc", "useCountDesc"][..],
            ),
            (
                "history.unit",
                self.history.unit.as_str(),
                &["hours", "days", "weeks", "months", "forever"][..],
            ),
            (
                "window.position",
                self.window.position.as_str(),
                &["followCursor", "center", "remember", "fixed", "dock"][..],
            ),
            (
                "window.dock_edge",
                self.window.dock_edge.as_str(),
                &["left", "right", "top", "bottom"][..],
            ),
            (
                "window.select_range_on_open",
                self.window.select_range_on_open.as_str(),
                &["preserve", "all", "favorite"][..],
            ),
            (
                "window.select_category_on_open",
                self.window.select_category_on_open.as_str(),
                &["preserve", "all", "text", "image", "files"][..],
            ),
        ] {
            ensure!(
                choices.contains(&value),
                "{}: clipboard.{key}",
                tr("config.invalid_value")
            );
        }
        let actions = [
            "paste",
            "pastePlain",
            "pastePath",
            "copy",
            "copyPlain",
            "openLink",
            "sendEmail",
            "reveal",
            "note",
            "star",
            "pinItem",
            "delete",
        ];
        for configured in [&self.content.item_actions, &self.content.item_action_order] {
            let mut seen = std::collections::BTreeSet::new();
            ensure!(
                configured
                    .iter()
                    .all(|action| actions.contains(&action.as_str()) && seen.insert(action)),
                "{}: clipboard.content.item_actions",
                tr("config.invalid_value")
            );
        }
        ensure!(
            self.display.text_max_lines > 0
                && self.display.image_max_height > 0
                && self.display.file_max_count > 0
                && !self.window.select_group_on_open.trim().is_empty(),
            "{}: clipboard.display/window",
            tr("config.invalid_value")
        );
        Ok(())
    }
}

config_section!(ClipboardQuickPasteConfig {
    enabled: bool = false,
    prefix: String = "Ctrl+Shift".into(),
});
config_section!(ClipboardCaptureConfig {
    text: bool = true,
    html: bool = true,
    rtf: bool = true,
    image: bool = true,
    files: bool = true,
    max_text_mb: u32 = 4,
    max_image_mb: u32 = 100,
    order: Vec<String> = ["files", "image", "html", "rtf", "text"].map(String::from).into(),
    image_ocr: bool = true,
});
config_section!(ClipboardContentConfig {
    auto_paste: String = "doubleClickPaste".into(),
    middle_click: String = "disabled".into(),
    copy_plain: bool = false,
    paste_plain: bool = false,
    copy_then_hide_window: bool = false,
    paste_files_as_path: bool = false,
    show_original_preview: bool = true,
    delete_confirm: bool = true,
    delete_favorite_items: bool = false,
    delete_favorite_items_confirm: bool = true,
    delete_favorite_items_only_in_fav_group: bool = true,
    delete_pinned_items: bool = false,
    delete_pinned_items_confirm: bool = true,
    auto_favorite: bool = false,
    update_on_reuse: bool = false,
    sort: String = "updatedAtDesc".into(),
    item_actions: Vec<String> = ["copy", "star", "pinItem", "delete"].map(String::from).into(),
    item_action_order: Vec<String> = [
        "paste", "pastePlain", "pastePath", "copy", "copyPlain", "openLink",
        "sendEmail", "reveal", "note", "star", "pinItem", "delete",
    ].map(String::from).into(),
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardDeletePolicy {
    Protected,
    ConfirmationRequired,
    Allowed,
}

impl ClipboardContentConfig {
    pub fn delete_policy(
        &self,
        is_favorite: bool,
        is_pinned: bool,
        in_favorites_group: bool,
    ) -> ClipboardDeletePolicy {
        if (is_favorite
            && (!self.delete_favorite_items
                || (self.delete_favorite_items_only_in_fav_group && !in_favorites_group)))
            || (is_pinned && !self.delete_pinned_items)
        {
            ClipboardDeletePolicy::Protected
        } else if self.delete_confirm
            || (is_favorite && self.delete_favorite_items_confirm)
            || (is_pinned && self.delete_pinned_items_confirm)
        {
            ClipboardDeletePolicy::ConfirmationRequired
        } else {
            ClipboardDeletePolicy::Allowed
        }
    }
}

config_section!(ClipboardDisplayConfig {
    text_max_lines: u32 = 3,
    image_max_height: u32 = 64,
    file_max_count: u32 = 3,
});
config_section!(ClipboardSensitiveConfig {
    collect_secrets: bool = false,
    redact_secrets: bool = true,
});
config_section!(ClipboardHistoryConfig {
    retention_value: u32 = 0,
    unit: String = "forever".into(),
    max_count: u32 = 0,
    cleanup_interval_hours: u32 = 0,
});
config_section!(ClipboardSearchConfig {
    default_focus: bool = false,
    clear_on_hide: bool = true,
});
config_section!(ClipboardWindowConfig {
    position: String = "followCursor".into(),
    pos_x: i32 = 0,
    pos_y: i32 = 0,
    dock_edge: String = "right".into(),
    scroll_to_top_on_open: bool = true,
    select_range_on_open: String = "preserve".into(),
    select_category_on_open: String = "preserve".into(),
    select_group_on_open: String = "preserve".into(),
    lightweight_mode: bool = true,
    idle_destroy_seconds: u32 = 60,
});
config_section!(ClipboardPreviewConfig {
    hover_enabled: bool = false,
    hover_delay_ms: u32 = 500,
    space_enabled: bool = true,
});
config_section!(ClipboardFeedbackConfig {
    copy_sound: bool = false,
});
config_section!(ClipboardFiltersConfig {
    excluded_app_ids: Vec<String> = Vec::new(),
});

config_section!(VisionConfig {
    enabled: bool = true,
    desktop_toolbar_mode: u32 = 2,
    allow_simple_hotkey: bool = true,
    disable_in_fullscreen_game: bool = true,
    ignore_process_list: Vec<String> = Vec::new(),
    shell_context_menu: bool = false,
    hotkeys: std::collections::BTreeMap<String, String> = [
        ("capture".into(), "F1".into()), ("pin".into(), "F2".into()),
        ("restore_last".into(), "Shift+F2".into()),
    ].into(),
    global_mouse: VisionGlobalMouseConfig = Default::default(),
    screenshot: VisionScreenshotConfig = Default::default(),
    mark: VisionMarkConfig = Default::default(),
    pin: VisionPinConfig = Default::default(),
    ocr: VisionOcrConfig = Default::default(),
    long_shot: VisionLongShotConfig = Default::default(),
    movie: VisionMovieConfig = Default::default(),
    save: VisionSaveConfig = Default::default(),
});
config_section!(VisionGlobalMouseConfig {
    bindings: Vec<MouseGestureBinding> = Vec::new(),
    show_mask_on_drag: bool = true,
    drag_threshold: u32 = 8,
});
pub const MOUSE_GESTURE_ACTIONS: &[&str] = &[
    "vision.capture_screen",
    "vision.pin_last",
    "vision.ocr",
    "quick_ask.capture",
];

config_section!(MouseGestureBinding {
    modifiers: Vec<String> = Vec::new(),
    button: String = "X1".into(),
    gesture: String = "drag".into(),
    action: String = "vision.pin_last".into(),
});
config_section!(VisionScreenshotConfig {
    confirm_on_close_by_key: bool = false,
    select_mode: String = "Smart".into(),
    keep_mode: bool = true,
    enable_round_rect: bool = false,
    ratio: f32 = 0.0,
    unselected_mask_color: String = "80000000".into(),
    border_color: String = "ff5e91e6".into(),
    action_bar_buttons: u32 = 127,
});
config_section!(VisionMarkConfig {
    edit_item_order: Vec<Vec<String>> = [
        vec!["arrow", "brokenLine"], vec!["geometry"], vec!["pencil", "marker"],
        vec!["mosaic", "autoMosaic"], vec!["serial", "text"], vec!["watermark"],
        vec!["magnifier", "highLight"],
    ].into_iter().map(|group| group.into_iter().map(String::from).collect()).collect(),
    common_color_argb: String = "ff5e91e6".into(),
    arrow: VisionStrokeConfig = Default::default(),
    broken_line: VisionStrokeConfig = Default::default(),
    geometry: VisionStrokeConfig = Default::default(),
    pencil: VisionStrokeConfig = Default::default(),
    marker: VisionStrokeConfig = VisionStrokeConfig { width: 16.0, ..Default::default() },
    mosaic: VisionMosaicConfig = Default::default(),
    auto_mosaic: VisionMosaicConfig = VisionMosaicConfig { strength: 15.7, ..Default::default() },
    serial: VisionSerialConfig = Default::default(),
    text: VisionTextConfig = Default::default(),
    watermark: VisionWatermarkConfig = Default::default(),
    magnifier: VisionMagnifierConfig = Default::default(),
    high_light: VisionHighLightConfig = Default::default(),
});
config_section!(VisionStrokeConfig {
    width: f32 = 3.0,
    filling: bool = false,
    path_mode: u32 = 2,
    start_style: String = "None".into(),
    end_style: String = "Arrow".into(),
    double_click_comment: bool = true,
    text_direction: String = "Auto".into(),
    text: VisionTextConfig = Default::default(),
});
config_section!(VisionMosaicConfig {
    mode: String = "Pixel".into(),
    strength: f32 = 21.24,
});
config_section!(VisionSerialConfig {
    global_count: bool = false,
    start: u32 = 1,
    shape: String = "Circle".into(),
    text: VisionTextConfig = Default::default(),
});
config_section!(VisionTextConfig {
    font: String = "Microsoft YaHei UI".into(),
    size: f32 = 17.6,
    bold: bool = false,
    italic: bool = false,
    outline: bool = false,
    outline_width: f32 = 1.0,
    outline_color_argb: String = "ff000000".into(),
    background: bool = false,
    background_color_argb: String = "ffffffff".into(),
});
config_section!(VisionWatermarkConfig {
    text: String = String::new(),
    tiled: bool = true,
    spacing: f32 = 32.0,
    angle: f32 = -30.0,
    opacity_pct: u32 = 30,
    style: VisionTextConfig = Default::default(),
});
config_section!(VisionMagnifierConfig {
    scale: f32 = 2.0,
    radius: f32 = 64.0,
});
config_section!(VisionHighLightConfig {
    mask_color_argb: String = "80000000".into(),
    border_width: f32 = 2.0,
});
config_section!(VisionPinConfig {
    confirmation_flags: u32 = 14,
    text_selectable_default: bool = true,
    double_click_text_select: bool = true,
    show_text_popup_menu: bool = true,
    default_opacity_pct: u32 = 100,
    stays_on_top: bool = true,
    show_taskbar_icon_when_unpinned: bool = false,
    restore_unclosed_on_startup: bool = true,
    max_history_pins: u32 = 50,
});
config_section!(VisionOcrConfig {
    remove_spaces: bool = false,
    punctuation_type: String = "Auto".into(),
    vertical_reading: bool = false,
    copy_text_confirmation: u32 = 65533,
});
config_section!(VisionLongShotConfig {
    auto_crop_on_scroll_back: bool = true,
});
config_section!(VisionMovieConfig {
    fps: u32 = 24,
    encode_format: u32 = 0,
    toolbar_display_type: u32 = 2,
    exit_record_after_encode: bool = true,
    key_mouse: VisionKeyMouseConfig = Default::default(),
});
config_section!(VisionKeyMouseConfig {
    record_mouse: bool = true,
    record_keyboard: bool = true,
    smooth_track: bool = true,
    simplify_animation: bool = false,
    key_keep_time_ms: u32 = 1500,
    max_history_count: u32 = 3,
    pointer_scale: f64 = 1.0,
    animation_speed: f64 = 1.0,
    left_color: [u8; 4] = [70, 150, 255, 210],
    right_color: [u8; 4] = [255, 100, 80, 210],
    middle_color: [u8; 4] = [90, 220, 130, 210],
    show_characters: bool = true,
    show_combinations: bool = true,
    show_modifiers: bool = true,
    show_function_keys: bool = true,
    key_font_size: u32 = 24,
    key_text_color: [u8; 4] = [255, 255, 255, 255],
    key_background_color: [u8; 4] = [25, 25, 30, 200],
    character_merge_limit: u32 = 24,
});
config_section!(VisionSaveConfig {
    manual_save_pattern: String = "Orbment_$yyyy-MM-dd_HH-mm-ss$.png".into(),
    auto_save_types_mask: u32 = 127,
    save_quality: u32 = 44,
});

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct FileSearchConfig {
    pub enabled: bool,
    pub backend: String,
    pub residency_mode: String,
    pub idle_release_seconds: u32,
    pub hotkey: String,
    pub included_drives: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub everything_exe_path: String,
    pub auto_spawn_headless: bool,
}
impl Default for FileSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: "BuiltinMft".into(),
            residency_mode: "SnapshotIncrement".into(),
            idle_release_seconds: 300,
            hotkey: "Alt+F".into(),
            included_drives: Vec::new(),
            exclude_paths: Vec::new(),
            everything_exe_path: String::new(),
            auto_spawn_headless: true,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct DiskAnalyzerConfig {
    pub enabled: bool,
    pub display_unit: String,
    pub show_allocated_size: bool,
    pub default_detail_depth: u32,
    pub min_block_pixel_size: u32,
    pub show_free_space: bool,
    pub monitor_fs_changes: bool,
    pub max_top_files: u32,
    pub palette_colors: Vec<String>,
}
impl Default for DiskAnalyzerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            display_unit: "Dynamic".into(),
            show_allocated_size: true,
            default_detail_depth: 3,
            min_block_pixel_size: 6,
            show_free_space: false,
            monitor_fs_changes: true,
            max_top_files: 1000,
            palette_colors: [
                "#5B8FF9", "#61DDAA", "#65789B", "#F6BD16", "#7262FD", "#78D3F8", "#9661BC",
                "#F6903D", "#008685", "#F08BB4", "#6DC8EC", "#FF9D4D",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum QuickAskProvider {
    #[default]
    OpenAI,
    OpenAiCompatible,
    Anthropic,
    Google,
}

impl QuickAskProvider {
    pub const ALL: [Self; 4] = [
        Self::OpenAI,
        Self::OpenAiCompatible,
        Self::Anthropic,
        Self::Google,
    ];

    pub const fn as_key(&self) -> &'static str {
        match self {
            Self::OpenAI => "OpenAI",
            Self::OpenAiCompatible => "OpenAiCompatible",
            Self::Anthropic => "Anthropic",
            Self::Google => "Google",
        }
    }

    pub const fn default_channel_id(&self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
            Self::OpenAiCompatible => "openai_compatible",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
        }
    }

    pub const fn default_base_url(&self) -> &'static str {
        match self {
            Self::OpenAI | Self::OpenAiCompatible => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta",
        }
    }

    pub const fn default_model(&self) -> &'static str {
        match self {
            Self::OpenAI | Self::OpenAiCompatible => "gpt-4o-mini",
            Self::Anthropic => "claude-sonnet-4-5",
            Self::Google => "gemini-2.5-flash",
        }
    }

    pub const fn default_credential_target(&self) -> &'static str {
        match self {
            Self::OpenAI => "Orbment/QuickAsk/openai",
            Self::OpenAiCompatible => "Orbment/QuickAsk/openai_compatible",
            Self::Anthropic => "Orbment/QuickAsk/anthropic",
            Self::Google => "Orbment/QuickAsk/google",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum ThinkingEffort {
    #[default]
    Off,
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct QuickAskChannelConfig {
    pub id: String,
    pub name: String,
    pub provider: QuickAskProvider,
    pub base_url: String,
    pub model: String,
    pub credential_target: String,
    pub thinking_effort: ThinkingEffort,
    pub cached_models: Vec<String>,
}

impl Default for QuickAskChannelConfig {
    fn default() -> Self {
        Self::for_provider(QuickAskProvider::default())
    }
}

impl QuickAskChannelConfig {
    pub fn for_provider(provider: QuickAskProvider) -> Self {
        Self {
            id: provider.default_channel_id().into(),
            name: quick_ask_default_channel_name(provider),
            provider,
            base_url: provider.default_base_url().into(),
            model: provider.default_model().into(),
            credential_target: provider.default_credential_target().into(),
            thinking_effort: ThinkingEffort::default(),
            cached_models: Vec::new(),
        }
    }

    pub fn default_credential_target(&self) -> String {
        format!("Orbment/QuickAsk/{}", self.id)
    }

    pub fn default_name(&self) -> String {
        if let Some(provider) = QuickAskProvider::ALL
            .into_iter()
            .find(|provider| provider.default_channel_id() == self.id)
        {
            quick_ask_default_channel_name(provider)
        } else if let Some(number) = self.id.strip_prefix("channel-") {
            tr("quick_ask.custom_channel").replace("{number}", number)
        } else {
            self.id.clone()
        }
    }

    fn validate(&self, path: &str) -> Result<()> {
        validate_quick_ask_connection(
            self.provider,
            &self.base_url,
            &self.model,
            &self.credential_target,
            path,
        )?;
        ensure!(
            self.cached_models
                .iter()
                .all(|model| !model.trim().is_empty() && !model.chars().any(char::is_control)),
            "{}: {path}.cached_models",
            tr("config.invalid_value")
        );
        Ok(())
    }
}

const QUICK_ASK_ACTIVE_FIELDS: [&str; 5] = [
    "provider",
    "base_url",
    "model",
    "credential_target",
    "thinking_effort",
];

#[derive(Clone, Debug, Serialize)]
pub struct QuickAskConfig {
    pub enabled: bool,
    pub hotkey: Option<String>,
    pub provider: QuickAskProvider,
    pub base_url: String,
    pub model: String,
    pub credential_target: String,
    pub thinking_effort: ThinkingEffort,
    pub active_channel: String,
    pub channel_order: Vec<String>,
    pub channels: BTreeMap<String, QuickAskChannelConfig>,
    pub system_prompt: String,
    pub prompts: Vec<QuickAskPrompt>,
    pub max_tokens: u32,
    pub request_timeout_seconds: u32,
    pub max_tool_rounds: u32,
}

impl Default for QuickAskConfig {
    fn default() -> Self {
        let provider = QuickAskProvider::default();
        let channel = QuickAskChannelConfig::for_provider(provider);
        Self {
            enabled: false,
            hotkey: Some("Alt+Q".into()),
            provider,
            base_url: channel.base_url,
            model: channel.model,
            credential_target: channel.credential_target,
            thinking_effort: channel.thinking_effort,
            active_channel: provider.default_channel_id().into(),
            channel_order: QuickAskProvider::ALL
                .into_iter()
                .map(|provider| provider.default_channel_id().into())
                .collect(),
            channels: QuickAskProvider::ALL
                .into_iter()
                .map(|provider| {
                    (
                        provider.default_channel_id().into(),
                        QuickAskChannelConfig::for_provider(provider),
                    )
                })
                .collect(),
            system_prompt: String::new(),
            prompts: default_quick_ask_prompts(),
            max_tokens: 4096,
            request_timeout_seconds: 120,
            max_tool_rounds: 8,
        }
    }
}

impl<'de> Deserialize<'de> for QuickAskConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize the normalized fields without recursively invoking this migration.
        #[derive(Deserialize)]
        #[serde(remote = "QuickAskConfig")]
        struct Fields {
            enabled: bool,
            hotkey: Option<String>,
            provider: QuickAskProvider,
            base_url: String,
            model: String,
            credential_target: String,
            thinking_effort: ThinkingEffort,
            active_channel: String,
            channel_order: Vec<String>,
            channels: BTreeMap<String, QuickAskChannelConfig>,
            system_prompt: String,
            prompts: Vec<QuickAskPrompt>,
            max_tokens: u32,
            request_timeout_seconds: u32,
            max_tool_rounds: u32,
        }

        use serde::de::Error;
        let mut input = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let requested_provider = input
            .get("provider")
            .cloned()
            .map(serde_json::from_value::<QuickAskProvider>)
            .transpose()
            .map_err(D::Error::custom)?;
        let mut config = Self::default();
        let mut migrated_ids = BTreeMap::new();
        if let Some(channels) = input.remove("channels") {
            let (entries, legacy) = match channels {
                serde_json::Value::Object(channels) => {
                    let legacy = !channels.is_empty()
                        && channels.iter().all(|(key, fields)| {
                            QuickAskProvider::ALL
                                .iter()
                                .any(|provider| provider.as_key() == key)
                                && fields.as_object().is_some_and(|fields| {
                                    !fields.contains_key("id") && !fields.contains_key("provider")
                                })
                        });
                    (
                        channels
                            .into_iter()
                            .map(|(key, fields)| (Some(key), fields))
                            .collect::<Vec<_>>(),
                        legacy,
                    )
                }
                serde_json::Value::Array(channels) => (
                    channels.into_iter().map(|fields| (None, fields)).collect(),
                    false,
                ),
                _ => {
                    return Err(D::Error::custom(
                        "quick_ask.channels must be a map or array",
                    ));
                }
            };
            if !legacy {
                config.channels.clear();
                config.channel_order.clear();
            }
            let mut seen = std::collections::BTreeSet::new();
            for (key, value) in entries {
                let fields = value.as_object().ok_or_else(|| {
                    D::Error::custom("quick_ask.channels entries must be objects")
                })?;
                let legacy_provider = key.as_deref().and_then(|key| {
                    (!fields.contains_key("id") && !fields.contains_key("provider"))
                        .then(|| {
                            QuickAskProvider::ALL
                                .into_iter()
                                .find(|provider| provider.as_key() == key)
                        })
                        .flatten()
                });
                let provider = fields
                    .get("provider")
                    .cloned()
                    .map(serde_json::from_value::<QuickAskProvider>)
                    .transpose()
                    .map_err(D::Error::custom)?
                    .or(legacy_provider)
                    .unwrap_or(QuickAskProvider::OpenAiCompatible);
                let id = match (key, legacy_provider) {
                    (Some(key), Some(provider)) => {
                        let id = provider.default_channel_id().to_owned();
                        migrated_ids.insert(key, id.clone());
                        id
                    }
                    (Some(key), None) => key,
                    (None, _) => fields
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            D::Error::custom("quick_ask.channels array entries require an id")
                        })?
                        .to_owned(),
                };
                if !seen.insert(id.clone()) {
                    return Err(D::Error::custom(format!(
                        "duplicate quick_ask channel id: {id}"
                    )));
                }
                let mut defaults = QuickAskChannelConfig::for_provider(provider);
                defaults.id.clone_from(&id);
                defaults.name = defaults.default_name();
                defaults.credential_target = defaults.default_credential_target();
                let mut channel = serde_json::to_value(defaults).map_err(D::Error::custom)?;
                channel
                    .as_object_mut()
                    .expect("channel object")
                    .extend(fields.clone());
                let channel = serde_json::from_value(channel).map_err(D::Error::custom)?;
                if !legacy {
                    config.channel_order.push(id.clone());
                }
                config.channels.insert(id, channel);
            }
        }
        if let Some(order) = input.remove("channel_order") {
            let order: Vec<String> = serde_json::from_value(order).map_err(D::Error::custom)?;
            config.channel_order = order
                .into_iter()
                .map(|id| migrated_ids.get(&id).cloned().unwrap_or(id))
                .collect();
        }
        config.active_channel = if let Some(active) = input.remove("active_channel") {
            let id: String = serde_json::from_value(active).map_err(D::Error::custom)?;
            migrated_ids.get(&id).cloned().unwrap_or(id)
        } else {
            config
                .channel_order
                .iter()
                .find(|id| {
                    requested_provider.is_some_and(|provider| {
                        config
                            .channels
                            .get(*id)
                            .is_some_and(|channel| channel.provider == provider)
                    })
                })
                .or_else(|| config.channel_order.first())
                .cloned()
                .unwrap_or_default()
        };
        if config.channels.contains_key(&config.active_channel) {
            config.load_active_channel();
        }
        let mut normalized = serde_json::to_value(config).map_err(D::Error::custom)?;
        normalized
            .as_object_mut()
            .expect("configuration object")
            .extend(input);
        let mut config = Fields::deserialize(normalized).map_err(D::Error::custom)?;
        // Explicit top-level connection settings are authoritative for imports.
        // Editable drafts may be invalid until the owning ConfigStore validates them.
        if config.channels.contains_key(&config.active_channel) {
            config.sync_active_channel();
        }
        Ok(config)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuickAskPrompt {
    pub id: String,
    pub label: String,
    pub prompt: String,
}
fn default_quick_ask_prompts() -> Vec<QuickAskPrompt> {
    [
        (
            "explain",
            "quick_ask.preset_explain",
            "quick_ask.prompt_explain",
        ),
        (
            "summarize",
            "quick_ask.preset_summarize",
            "quick_ask.prompt_summarize",
        ),
        ("tools", "quick_ask.preset_tools", "quick_ask.prompt_tools"),
    ]
    .into_iter()
    .map(|(id, label, prompt)| QuickAskPrompt {
        id: id.into(),
        label: tr(label),
        prompt: tr(prompt),
    })
    .collect()
}
config_section!(HotkeyEngineConfig {
    enabled: bool = false,
    rules: Vec<HotkeyRule> = Vec::new(),
});
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HotkeyRule {
    pub id: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub trigger: HotkeyTrigger,
    #[serde(default)]
    pub active_process_filter: Vec<String>,
    pub action: HotkeyRuleAction,
}
fn enabled_by_default() -> bool {
    true
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind")]
pub enum HotkeyTrigger {
    Chord { keys: String },
    PhysicalKey { key: String },
    DoubleModifier { modifier: String, interval_ms: u32 },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind")]
pub enum HotkeyRuleAction {
    SendInput {
        keys: String,
    },
    Command {
        executable: String,
        #[serde(default)]
        arguments: Vec<String>,
    },
    ActionBus {
        name: String,
        #[serde(default)]
        parameters: std::collections::BTreeMap<String, serde_json::Value>,
    },
}

impl QuickAskConfig {
    pub fn chat_endpoint_url(&self) -> Result<url::Url> {
        self.endpoint_url(false)
    }

    pub fn models_endpoint_url(&self) -> Result<url::Url> {
        self.endpoint_url(true)
    }

    fn endpoint_url(&self, models: bool) -> Result<url::Url> {
        let base = self.base_url.trim();
        ensure!(
            !base.chars().any(char::is_whitespace),
            "{}",
            tr("config.invalid_value")
        );
        let mut url = url::Url::parse(base)?;
        ensure!(
            matches!(url.scheme(), "https" | "http")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "{}",
            tr("config.invalid_value")
        );
        let path = url.path().trim_end_matches('/');
        let path = match self.provider {
            QuickAskProvider::OpenAI
            | QuickAskProvider::OpenAiCompatible
            | QuickAskProvider::Anthropic => {
                let suffix = match self.provider {
                    QuickAskProvider::OpenAI => "/responses",
                    QuickAskProvider::OpenAiCompatible => "/chat/completions",
                    QuickAskProvider::Anthropic => "/messages",
                    QuickAskProvider::Google => unreachable!(),
                };
                let base = path
                    .strip_suffix(suffix)
                    .or_else(|| path.strip_suffix("/models"))
                    .or_else(|| {
                        (self.provider != QuickAskProvider::Anthropic)
                            .then(|| {
                                path.strip_suffix("/responses")
                                    .or_else(|| path.strip_suffix("/chat/completions"))
                            })
                            .flatten()
                    })
                    .unwrap_or(path);
                let base = if path.is_empty() { "/v1" } else { base };
                format!("{base}{}", if models { "/models" } else { suffix })
            }
            QuickAskProvider::Google => {
                let full_endpoint = path.ends_with(":streamGenerateContent");
                if full_endpoint && !models {
                    path.to_owned()
                } else {
                    let base = if full_endpoint {
                        path.rsplit_once("/models/")
                            .context(tr("config.invalid_value"))?
                            .0
                    } else {
                        path.strip_suffix("/models").unwrap_or(path)
                    };
                    let base = if path.is_empty() { "/v1beta" } else { base };
                    if models {
                        format!("{base}/models")
                    } else {
                        let model = self.model.trim().trim_start_matches("models/");
                        ensure!(
                            !model.is_empty() && !model.contains(['/', '?', '#', ':']),
                            "{}",
                            tr("config.invalid_value")
                        );
                        format!("{base}/models/{model}:streamGenerateContent")
                    }
                }
            }
        };
        url.set_path(&path);
        if self.provider == QuickAskProvider::Google {
            let query: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(key, _)| key != "alt")
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            url.set_query(None);
            if !query.is_empty() || !models {
                let mut pairs = url.query_pairs_mut();
                pairs.extend_pairs(query);
                if !models {
                    pairs.append_pair("alt", "sse");
                }
            }
        }
        Ok(url)
    }

    pub fn active_channel_config(&self) -> &QuickAskChannelConfig {
        self.channels
            .get(&self.active_channel)
            .expect("validated active channel")
    }

    pub fn ordered_channel_ids(&self) -> &[String] {
        &self.channel_order
    }

    pub fn sync_active_channel(&mut self) {
        let channel = self
            .channels
            .get_mut(&self.active_channel)
            .expect("validated active channel");
        channel.provider = self.provider;
        channel.base_url.clone_from(&self.base_url);
        channel.model.clone_from(&self.model);
        channel
            .credential_target
            .clone_from(&self.credential_target);
        channel.thinking_effort = self.thinking_effort;
    }

    pub fn switch_provider(&mut self, provider: QuickAskProvider) {
        self.provider = provider;
        self.sync_active_channel();
    }

    pub fn switch_channel(&mut self, id: &str) -> Result<()> {
        ensure!(
            self.channels.contains_key(id),
            "{}: quick_ask.active_channel ({id})",
            tr("config.invalid_value")
        );
        self.sync_active_channel();
        self.active_channel = id.into();
        self.load_active_channel();
        Ok(())
    }

    pub fn add_channel(&mut self) -> String {
        let mut number = 5;
        let channel = loop {
            let mut channel =
                QuickAskChannelConfig::for_provider(QuickAskProvider::OpenAiCompatible);
            channel.id = format!("channel-{number}");
            channel.name = channel.default_name();
            channel.credential_target = channel.default_credential_target();
            if !self.channels.contains_key(&channel.id)
                && !self
                    .channels
                    .values()
                    .any(|existing| existing.name.trim() == channel.name)
            {
                break channel;
            }
            number += 1;
        };
        let id = channel.id.clone();
        self.channels.insert(id.clone(), channel);
        self.channel_order.push(id.clone());
        self.switch_channel(&id).expect("newly inserted channel");
        id
    }

    pub fn remove_channel(&mut self, id: &str) -> Result<QuickAskChannelConfig> {
        ensure!(
            self.channels.contains_key(id),
            "{}: quick_ask.channels.{id}",
            tr("config.invalid_value")
        );
        ensure!(self.channels.len() > 1, "{}", tr("quick_ask.last_channel"));
        self.sync_active_channel();
        let removed = self.channels.remove(id).expect("existing channel");
        self.channel_order.retain(|existing| existing != id);
        if self.active_channel == id {
            self.active_channel = self
                .channel_order
                .first()
                .expect("remaining channel")
                .clone();
            self.load_active_channel();
        }
        Ok(removed)
    }

    fn load_active_channel(&mut self) {
        let channel = self
            .channels
            .get(&self.active_channel)
            .expect("validated active channel");
        self.provider = channel.provider;
        self.base_url.clone_from(&channel.base_url);
        self.model.clone_from(&channel.model);
        self.credential_target
            .clone_from(&channel.credential_target);
        self.thinking_effort = channel.thinking_effort;
    }

    pub fn validate(&self) -> Result<()> {
        validate_quick_ask_connection(
            self.provider,
            &self.base_url,
            &self.model,
            &self.credential_target,
            "quick_ask",
        )?;
        ensure!(
            !self.channels.is_empty() && self.channels.contains_key(&self.active_channel),
            "{}: quick_ask.active_channel",
            tr("config.invalid_value")
        );
        let mut ordered = std::collections::BTreeSet::new();
        ensure!(
            self.channel_order.len() == self.channels.len()
                && self
                    .channel_order
                    .iter()
                    .all(|id| self.channels.contains_key(id) && ordered.insert(id)),
            "{}: quick_ask.channel_order",
            tr("config.invalid_value")
        );
        let mut names = std::collections::BTreeSet::new();
        for (key, channel) in &self.channels {
            let path = format!("quick_ask.channels.{key}");
            ensure!(
                key == &channel.id
                    && !channel.id.trim().is_empty()
                    && !channel.id.chars().any(char::is_control),
                "{}: {path}.id",
                tr("config.invalid_value")
            );
            ensure!(
                !channel.name.trim().is_empty()
                    && !channel.name.chars().any(char::is_control)
                    && names.insert(channel.name.trim()),
                "{}: {path}.name",
                tr("config.invalid_value")
            );
            channel.validate(&path)?;
        }
        ensure!(
            self.hotkey
                .as_ref()
                .is_none_or(|key| !key.trim().is_empty())
                && self.max_tokens > 0
                && (1..=3600).contains(&self.request_timeout_seconds)
                && (1..=32).contains(&self.max_tool_rounds),
            "{}: quick_ask",
            tr("config.invalid_value")
        );
        let mut ids = std::collections::BTreeSet::new();
        for prompt in &self.prompts {
            ensure!(
                !prompt.id.trim().is_empty()
                    && ids.insert(&prompt.id)
                    && !prompt.label.trim().is_empty()
                    && !prompt.prompt.trim().is_empty(),
                "{}: quick_ask.prompts",
                tr("config.invalid_value")
            );
        }
        Ok(())
    }
}

fn validate_quick_ask_connection(
    provider: QuickAskProvider,
    base_url: &str,
    model: &str,
    credential_target: &str,
    path: &str,
) -> Result<()> {
    let base = base_url.trim();
    let base = if provider == QuickAskProvider::Google {
        base.strip_suffix(":streamGenerateContent?alt=sse")
            .unwrap_or(base)
    } else {
        base
    };
    ensure!(
        (base.starts_with("https://") || base.starts_with("http://"))
            && !base.chars().any(|c| c.is_whitespace() || c.is_control())
            && !base.contains(['?', '#', '@', '\\'])
            && base.split_once("://").is_some_and(|(_, rest)| !rest
                .split('/')
                .next()
                .unwrap_or_default()
                .is_empty()),
        "{}: {path}.base_url",
        tr("config.invalid_value")
    );
    let url = url::Url::parse(base)
        .with_context(|| format!("{}: {path}.base_url", tr("config.invalid_value")))?;
    ensure!(
        url.host_str().is_some() && url.username().is_empty() && url.password().is_none(),
        "{}: {path}.base_url",
        tr("config.invalid_value")
    );
    ensure!(
        !model.trim().is_empty() && !model.chars().any(char::is_control),
        "{}: {path}.model",
        tr("config.invalid_value")
    );
    ensure!(
        !credential_target.trim().is_empty()
            && !credential_target.chars().any(char::is_control)
            && credential_target.encode_utf16().count() <= 32767,
        "{}: {path}.credential_target",
        tr("config.invalid_value")
    );
    Ok(())
}

impl HotkeyEngineConfig {
    pub fn validate(&self) -> Result<()> {
        let mut ids = std::collections::BTreeSet::new();
        for rule in &self.rules {
            let trigger_valid = match &rule.trigger {
                HotkeyTrigger::Chord { keys } => !keys.trim().is_empty(),
                HotkeyTrigger::PhysicalKey { key } => !key.trim().is_empty(),
                HotkeyTrigger::DoubleModifier {
                    modifier,
                    interval_ms,
                } => {
                    ["Ctrl", "Alt", "Shift", "Win"].contains(&modifier.as_str())
                        && (1..=2000).contains(interval_ms)
                }
            };
            let action_valid = match &rule.action {
                HotkeyRuleAction::SendInput { keys } => !keys.trim().is_empty(),
                HotkeyRuleAction::Command {
                    executable,
                    arguments,
                } => {
                    !executable.trim().is_empty()
                        && !executable.contains('\0')
                        && arguments.iter().all(|value| !value.contains('\0'))
                }
                HotkeyRuleAction::ActionBus { name, .. } => name
                    .split_once('.')
                    .is_some_and(|(module, action)| !module.is_empty() && !action.is_empty()),
            };
            ensure!(
                !rule.id.trim().is_empty()
                    && ids.insert(&rule.id)
                    && trigger_valid
                    && action_valid
                    && rule
                        .active_process_filter
                        .iter()
                        .all(|name| !name.trim().is_empty() && !name.contains(['/', '\\', '\0'])),
                "{}: hotkey_engine.rules",
                tr("config.invalid_value")
            );
        }
        Ok(())
    }
}

impl VisionConfig {
    pub fn validate(&self) -> Result<()> {
        fn color(value: &str) -> bool {
            value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        }
        fn text(value: &VisionTextConfig) -> bool {
            !value.font.trim().is_empty()
                && value.size.is_finite()
                && value.size > 0.0
                && value.outline_width.is_finite()
                && value.outline_width >= 0.0
                && color(&value.outline_color_argb)
                && color(&value.background_color_argb)
        }
        ensure!(
            self.desktop_toolbar_mode <= 2
                && self.global_mouse.drag_threshold > 0
                && self
                    .ignore_process_list
                    .iter()
                    .all(|name| !name.trim().is_empty() && !name.contains(['/', '\\', '\0']))
                && self
                    .hotkeys
                    .iter()
                    .all(|(action, key)| !action.trim().is_empty() && !key.trim().is_empty())
                && ["Smart", "Rectangle", "RoundRect", "Polygon", "Freehand"]
                    .contains(&self.screenshot.select_mode.as_str())
                && self.screenshot.ratio.is_finite()
                && self.screenshot.ratio >= 0.0
                && color(&self.screenshot.unselected_mask_color)
                && color(&self.screenshot.border_color)
                && color(&self.mark.common_color_argb)
                && (1..=100).contains(&self.pin.default_opacity_pct)
                && self.pin.max_history_pins > 0
                && ["Auto", "Chinese", "English"].contains(&self.ocr.punctuation_type.as_str())
                && (1..=240).contains(&self.movie.fps)
                && self.movie.encode_format <= 2
                && self.movie.toolbar_display_type <= 2
                && (1..=60000).contains(&self.movie.key_mouse.key_keep_time_ms)
                && (1..=32).contains(&self.movie.key_mouse.max_history_count)
                && (0.1..=10.0).contains(&self.movie.key_mouse.pointer_scale)
                && (0.1..=10.0).contains(&self.movie.key_mouse.animation_speed)
                && (8..=128).contains(&self.movie.key_mouse.key_font_size)
                && self.movie.key_mouse.character_merge_limit <= 256
                && self.save.save_quality <= 100
                && !self.save.manual_save_pattern.trim().is_empty(),
            "{}: vision",
            tr("config.invalid_value")
        );
        let mut bindings = std::collections::BTreeSet::new();
        for binding in &self.global_mouse.bindings {
            let modifiers: std::collections::BTreeSet<_> = binding.modifiers.iter().collect();
            let wheel = binding.gesture.starts_with("wheel");
            ensure!(
                ["Left", "Right", "Middle", "X1", "X2"].contains(&binding.button.as_str())
                    && [
                        "click",
                        "doubleClick",
                        "drag",
                        "wheelUp",
                        "wheelDown",
                        "wheelLeft",
                        "wheelRight"
                    ]
                    .contains(&binding.gesture.as_str())
                    && binding.modifiers.iter().all(
                        |modifier| ["Ctrl", "Shift", "Alt", "Win"].contains(&modifier.as_str())
                    )
                    && modifiers.len() == binding.modifiers.len()
                    && (wheel || binding.button != "Left" || !modifiers.is_empty())
                    && MOUSE_GESTURE_ACTIONS.contains(&binding.action.as_str())
                    && bindings.insert((
                        modifiers,
                        if wheel { "" } else { binding.button.as_str() },
                        &binding.gesture,
                    )),
                "{}: vision.global_mouse.bindings",
                tr("config.invalid_value")
            );
        }
        let mut tools = std::collections::BTreeSet::new();
        for group in &self.mark.edit_item_order {
            ensure!(
                !group.is_empty()
                    && group.iter().all(|tool| [
                        "arrow",
                        "brokenLine",
                        "geometry",
                        "pencil",
                        "marker",
                        "mosaic",
                        "autoMosaic",
                        "serial",
                        "text",
                        "watermark",
                        "magnifier",
                        "highLight"
                    ]
                    .contains(&tool.as_str())
                        && tools.insert(tool)),
                "{}: vision.mark.edit_item_order",
                tr("config.invalid_value")
            );
        }
        for stroke in [
            &self.mark.arrow,
            &self.mark.broken_line,
            &self.mark.geometry,
            &self.mark.pencil,
            &self.mark.marker,
        ] {
            ensure!(
                stroke.width.is_finite()
                    && stroke.width > 0.0
                    && stroke.path_mode <= 2
                    && text(&stroke.text)
                    && ["Auto", "Follow", "Flat"].contains(&stroke.text_direction.as_str())
                    && [&stroke.start_style, &stroke.end_style]
                        .iter()
                        .all(|style| ["None", "Arrow", "Circle", "Square", "Diamond"]
                            .contains(&style.as_str())),
                "{}: vision.mark.stroke",
                tr("config.invalid_value")
            );
        }
        for mosaic in [&self.mark.mosaic, &self.mark.auto_mosaic] {
            ensure!(
                ["Pixel", "Blur"].contains(&mosaic.mode.as_str())
                    && mosaic.strength.is_finite()
                    && mosaic.strength > 0.0,
                "{}: vision.mark.mosaic",
                tr("config.invalid_value")
            );
        }
        ensure!(
            text(&self.mark.text)
                && text(&self.mark.serial.text)
                && text(&self.mark.watermark.style)
                && ["Circle", "Square", "None"].contains(&self.mark.serial.shape.as_str())
                && self.mark.watermark.spacing.is_finite()
                && self.mark.watermark.spacing > 0.0
                && self.mark.watermark.angle.is_finite()
                && self.mark.watermark.opacity_pct <= 100
                && self.mark.magnifier.scale.is_finite()
                && self.mark.magnifier.scale >= 1.0
                && self.mark.magnifier.radius.is_finite()
                && self.mark.magnifier.radius > 0.0
                && color(&self.mark.high_light.mask_color_argb)
                && self.mark.high_light.border_width.is_finite()
                && self.mark.high_light.border_width >= 0.0,
            "{}: vision.mark",
            tr("config.invalid_value")
        );
        Ok(())
    }
}

impl OrbmentConfig {
    fn validate(&self) -> Result<()> {
        self.clipboard.validate()?;
        self.vision.validate()?;
        self.quick_ask.validate()?;
        self.hotkey_engine.validate()?;
        ensure!(
            self.app.language == "zh-CN",
            "{}: app.language",
            tr("config.invalid_value")
        );
        ensure!(
            ["BuiltinMft", "EverythingBridge"].contains(&self.file_search.backend.as_str())
                && ["Resident", "SnapshotIncrement", "OnDemandOnly"]
                    .contains(&self.file_search.residency_mode.as_str()),
            "{}: file_search",
            tr("config.invalid_value")
        );
        ensure!(
            ["Dynamic", "Bytes", "KB", "MB", "GB", "TB"]
                .contains(&self.disk_analyzer.display_unit.as_str())
                && (1..=8).contains(&self.disk_analyzer.default_detail_depth)
                && self.disk_analyzer.min_block_pixel_size > 0
                && self.disk_analyzer.max_top_files > 0
                && !self.disk_analyzer.palette_colors.is_empty()
                && self.disk_analyzer.palette_colors.iter().all(|color| {
                    color.len() == 7
                        && color.starts_with('#')
                        && color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
                }),
            "{}: disk_analyzer",
            tr("config.invalid_value")
        );
        ensure!(
            ["Ask", "WindowsNative", "OfflineOnnx"].contains(&self.app.ocr_engine_tier.as_str()),
            "{}: app.ocr_engine_tier",
            tr("config.invalid_value")
        );
        let monitor = &self.monitor;
        let audio = &self.audio;
        let memory = &self.memory;
        let utilities = &self.utilities;
        ensure!(
            memory.clean_regions_mask != 0
                && memory.clean_regions_mask & !0x3f == 0
                && (1..=100).contains(&memory.auto_clean_threshold_pct)
                && memory.auto_clean_interval_mins > 0
                && memory.tray_icon.warn_level_pct < memory.tray_icon.danger_level_pct
                && memory.tray_icon.danger_level_pct <= 100
                && !memory.tray_icon.font.trim().is_empty()
                && memory.reaper.close_timeout_seconds <= 60,
            "{}: memory",
            tr("config.invalid_value")
        );
        let mut family_names = std::collections::BTreeSet::new();
        for family in &memory.reaper.families {
            ensure!(
                !family.name.trim().is_empty()
                    && family_names.insert(family.name.to_lowercase())
                    && !family.owners.is_empty()
                    && !family.companions.is_empty()
                    && family
                        .owners
                        .iter()
                        .chain(&family.companions)
                        .chain(&family.exemptions)
                        .all(|name| !name.trim().is_empty() && !name.contains(['/', '\\']))
                    && family
                        .path_prefix
                        .as_ref()
                        .is_none_or(|path| !path.trim().is_empty()),
                "{}: memory.reaper.families",
                tr("config.invalid_value")
            );
        }
        ensure!(
            (1..=100).contains(&utilities.brightness_step_pct)
                && (1..=5000).contains(&utilities.brightness_debounce_ms)
                && utilities.topmost_border_color <= 0x00ffffff
                && (utilities.keep_awake_duration != KeepAwakeDuration::Process
                    || utilities.keep_awake_process_id.is_some_and(|pid| pid > 0)),
            "{}: utilities",
            tr("config.invalid_value")
        );
        for hotkey in [
            &utilities.topmost_hotkey,
            &utilities.color_picker_hotkey,
            &utilities.keep_awake_hotkey,
            &utilities.brightness_hotkey_up,
            &utilities.brightness_hotkey_down,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                !hotkey.trim().is_empty(),
                "{}: utilities.hotkey",
                tr("config.invalid_value")
            );
        }
        ensure!(
            monitor.sample_interval_ms > 0,
            "{}: monitor.sample_interval_ms",
            tr("config.invalid_value")
        );
        ensure!(
            audio.banner_on_screen_time_ms > 0,
            "{}: audio.banner_on_screen_time_ms",
            tr("config.invalid_value")
        );
        ensure!(
            self.app.log_retention_days > 0,
            "{}: app.log_retention_days",
            tr("config.invalid_value")
        );
        ensure!(
            (10..=100).contains(&audio.banner_opacity_pct) && audio.max_number_notifications > 0,
            "{}: audio.banner_opacity_pct/max_number_notifications",
            tr("config.invalid_value")
        );
        ensure!(
            (20..=100).contains(&monitor.floating.opacity_pct),
            "{}: monitor.floating.opacity_pct",
            tr("config.invalid_value")
        );
        for (key, size) in [
            ("monitor.taskbar.size", monitor.taskbar.size),
            ("monitor.floating.size", monitor.floating.size),
        ] {
            ensure!(
                size.is_finite() && size >= 1.0,
                "{}: {key}",
                tr("config.invalid_value")
            );
        }
        ensure!(
            monitor.speed_unit <= 3
                && monitor.memory_display_mode <= 2
                && monitor.double_click_action <= 7
                && monitor.floating.allow_out_of_border <= 2
                && monitor.alerts.memory_pct <= 100,
            "{}",
            tr("config.invalid_value")
        );
        for temperature in [
            monitor.alerts.cpu_temperature_c,
            monitor.alerts.gpu_temperature_c,
            monitor.alerts.disk_temperature_c,
        ] {
            ensure!(
                temperature.is_finite() && (0.0..=200.0).contains(&temperature),
                "{}",
                tr("config.invalid_value")
            );
        }
        for (key, value, choices) in [
            (
                "app.log_level",
                self.app.log_level.as_str(),
                &["trace", "debug", "info", "warn", "error", "off"][..],
            ),
            (
                "audio.switch_icon_mode",
                audio.switch_icon_mode.as_str(),
                &["Playback", "Recording", "Both", "Never"][..],
            ),
            (
                "audio.icon_double_click",
                audio.icon_double_click.as_str(),
                &["SwitchDevice", "OpenSettings"][..],
            ),
            (
                "audio.tooltip_info",
                audio.tooltip_info.as_str(),
                &["Playback", "Recording", "Both", "None"][..],
            ),
            (
                "audio.switch_device_notification",
                audio.switch_device_notification.as_str(),
                &["Banner", "SystemToast", "Sound", "None"][..],
            ),
            (
                "audio.switch_profile_notification",
                audio.switch_profile_notification.as_str(),
                &["Banner", "SystemToast", "None"][..],
            ),
            (
                "audio.mic_mute_notification",
                audio.mic_mute_notification.as_str(),
                &["Banner", "SystemToast", "None"][..],
            ),
            (
                "audio.banner_position",
                audio.banner_position.as_str(),
                &[
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
                ][..],
            ),
            (
                "audio.banner_display_info",
                audio.banner_display_info.as_str(),
                &["Full", "IconOnly", "TextOnly"][..],
            ),
            (
                "audio.mic_mute_banner_mode",
                audio.mic_mute_banner_mode.as_str(),
                &["None", "Temporary", "Persistent"][..],
            ),
            (
                "audio.mic_unmute_banner_mode",
                audio.mic_unmute_banner_mode.as_str(),
                &["None", "Temporary", "Persistent"][..],
            ),
            (
                "monitor.alerts.traffic_unit",
                monitor.alerts.traffic_unit.as_str(),
                &["MB", "GB"][..],
            ),
        ] {
            ensure!(
                choices.contains(&value),
                "{}: {key}",
                tr("config.invalid_value")
            );
        }
        let mut profile_ids = std::collections::BTreeSet::new();
        for profile in &audio.profiles {
            ensure!(
                !profile.id.trim().is_empty() && profile_ids.insert(&profile.id),
                "{}: audio.profiles.id",
                tr("config.invalid_value")
            );
        }
        Ok(())
    }
}

pub fn resolve_data_dir(
    exe_dir: &Path,
    explicit: Option<&Path>,
    environment: Option<&Path>,
    appdata: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = explicit.or(environment) {
        return Ok(path.to_owned());
    }
    if exe_dir.join("portable.flag").is_file() || exe_dir.join("OrbmentData").is_dir() {
        return Ok(exe_dir.join("OrbmentData"));
    }
    Ok(appdata
        .context("APPDATA is not set; use --data-dir or portable.flag")?
        .join("Orbment"))
}

pub struct ConfigStore {
    path: PathBuf,
    document: DocumentMut,
    pub config: OrbmentConfig,
}
impl ConfigStore {
    pub fn load(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join("config.toml");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let document = text
            .parse::<DocumentMut>()
            .with_context(|| tr("config.invalid_syntax"))?;
        let config: OrbmentConfig =
            toml_edit::de::from_str(&text).with_context(|| tr("config.invalid_value"))?;
        config.validate()?;
        let store = Self {
            path,
            document,
            config,
        };
        if !store.path.exists() {
            store.save()?;
        }
        Ok(store)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn reload(&mut self) -> Result<()> {
        *self = Self::load(self.path.parent().expect("config parent"))?;
        Ok(())
    }
    /// Returns effective values, including defaults absent from the sparse TOML document.
    pub fn get(&self, dotted_key: &str) -> Result<serde_json::Value> {
        let value = serde_json::to_value(&self.config)?;
        if dotted_key.is_empty() {
            return Ok(value);
        }
        Ok(json_path(&value, &config_path(dotted_key)?)?.clone())
    }

    /// Trusted settings UI mutation; external callers must use `set_external`.
    pub fn set_json(&mut self, dotted_key: &str, value: serde_json::Value) -> Result<()> {
        let (document, config) = self.prepare_update(dotted_key, &value)?;
        self.commit_update(document, config)
    }

    /// Persist changes from the audio service without replacing unrelated audio settings.
    pub fn save_audio_selections(
        &mut self,
        playback: &[AudioDeviceSelection],
        recording: &[AudioDeviceSelection],
    ) -> Result<bool> {
        let audio = &self.config.audio;
        if audio.selected_playback_devices == playback
            && audio.selected_recording_devices == recording
        {
            return Ok(false);
        }
        let effective = serde_json::to_value(&self.config)?;
        let mut document = self.document.clone();
        for (key, old, new) in [
            (
                "selected_playback_devices",
                audio.selected_playback_devices.as_slice(),
                playback,
            ),
            (
                "selected_recording_devices",
                audio.selected_recording_devices.as_slice(),
                recording,
            ),
        ] {
            if old != new {
                update_document(
                    document.as_item_mut(),
                    &effective,
                    &["audio", key],
                    &serde_json::to_value(new)?,
                )?;
            }
        }
        let mut config = self.config.clone();
        config.audio.selected_playback_devices = playback.to_vec();
        config.audio.selected_recording_devices = recording.to_vec();
        self.commit_update(document, config)?;
        Ok(true)
    }

    /// Apply a trusted update in memory; call `save` after a batch to persist it.
    pub fn apply_json(&mut self, dotted_key: &str, value: serde_json::Value) -> Result<()> {
        let (document, config) = self.prepare_update(dotted_key, &value)?;
        self.document = document;
        self.config = config;
        Ok(())
    }

    pub fn set_external(&mut self, dotted_key: &str, value: serde_json::Value) -> Result<()> {
        let allowed_memory = self.config.mcp.allow_memory_clean;
        ensure!(
            !external_path_protected(dotted_key, allowed_memory),
            "{}: {dotted_key}",
            tr("config.protected")
        );
        let (document, config) = self.prepare_update(dotted_key, &value)?;
        reject_protected_changes(
            "",
            &serde_json::to_value(&self.config)?,
            &serde_json::to_value(&config)?,
            allowed_memory,
        )?;
        self.commit_update(document, config)
    }

    fn prepare_update(
        &self,
        dotted_key: &str,
        value: &serde_json::Value,
    ) -> Result<(DocumentMut, OrbmentConfig)> {
        let keys = config_path(dotted_key)?;
        let effective = serde_json::to_value(&self.config)?;
        let mut candidate = effective.clone();
        if keys == ["quick_ask", "provider"] {
            let provider = serde_json::from_value(value.clone())
                .with_context(|| tr("config.invalid_value"))?;
            let mut quick_ask = self.config.quick_ask.clone();
            quick_ask.switch_provider(provider);
            candidate["quick_ask"] = serde_json::to_value(quick_ask)?;
        } else if keys == ["quick_ask", "active_channel"] {
            let id: String = serde_json::from_value(value.clone())
                .with_context(|| tr("config.invalid_value"))?;
            let mut quick_ask = self.config.quick_ask.clone();
            quick_ask.switch_channel(&id)?;
            candidate["quick_ask"] = serde_json::to_value(quick_ask)?;
        } else {
            *json_path_mut(&mut candidate, &keys)? = value.clone();
            if keys.starts_with(&["quick_ask", "channels"]) {
                // Channel edits own their mirrored values; imports otherwise prefer
                // explicitly supplied top-level fields for legacy compatibility.
                let quick_ask = candidate["quick_ask"]
                    .as_object_mut()
                    .expect("configuration object");
                for field in QUICK_ASK_ACTIVE_FIELDS {
                    quick_ask.remove(field);
                }
                if keys.len() == 2 {
                    quick_ask.remove("channel_order");
                }
            }
        }
        let mut config: OrbmentConfig =
            serde_json::from_value(candidate).with_context(|| tr("config.invalid_value"))?;
        if keys == ["quick_ask", "channels"] && value.is_object() {
            let mut order: Vec<_> = self
                .config
                .quick_ask
                .channel_order
                .iter()
                .filter(|id| config.quick_ask.channels.contains_key(*id))
                .cloned()
                .collect();
            for id in &config.quick_ask.channel_order {
                if !order.contains(id) {
                    order.push(id.clone());
                }
            }
            config.quick_ask.channel_order = order;
        }
        config.validate()?;
        let mut document = self.document.clone();
        let normalized = serde_json::to_value(&config)?;
        update_document(
            document.as_item_mut(),
            &effective,
            &keys,
            json_path(&normalized, &keys)?,
        )?;
        if keys[0] == "quick_ask" && keys.len() > 1 {
            // Persist the normalized channel set so legacy keys cannot reappear on
            // reload and a selection atomically saves both outgoing and incoming state.
            for field in QUICK_ASK_ACTIVE_FIELDS.into_iter().chain([
                "active_channel",
                "channel_order",
                "channels",
            ]) {
                let path = ["quick_ask", field];
                if keys != path {
                    update_document(
                        document.as_item_mut(),
                        &effective,
                        &path,
                        json_path(&normalized, &path)?,
                    )?;
                }
            }
        }
        Ok((document, config))
    }

    fn commit_update(&mut self, document: DocumentMut, config: OrbmentConfig) -> Result<()> {
        atomic_write(&self.path, document.to_string().as_bytes())
            .with_context(|| tr("config.write_failed"))?;
        self.document = document;
        self.config = config;
        Ok(())
    }
    pub fn save(&self) -> Result<()> {
        atomic_write(&self.path, self.document.to_string().as_bytes())
    }
}

fn config_path(path: &str) -> Result<Vec<&str>> {
    let keys: Vec<_> = path.split('.').collect();
    ensure!(
        keys.iter().all(|key| !key.is_empty()
            && key
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))),
        "{}: {path}",
        tr("config.invalid_path")
    );
    Ok(keys)
}

fn json_path<'a>(mut value: &'a serde_json::Value, keys: &[&str]) -> Result<&'a serde_json::Value> {
    for key in keys {
        value = if value.is_array() {
            key.parse::<usize>().ok().and_then(|index| value.get(index))
        } else {
            value.get(*key)
        }
        .with_context(|| format!("{}: {key}", tr("config.unknown_path")))?;
    }
    Ok(value)
}

fn json_path_mut<'a>(
    mut value: &'a mut serde_json::Value,
    keys: &[&str],
) -> Result<&'a mut serde_json::Value> {
    for key in keys {
        value = if value.is_array() {
            key.parse::<usize>()
                .ok()
                .and_then(|index| value.get_mut(index))
        } else {
            value.get_mut(*key)
        }
        .with_context(|| format!("{}: {key}", tr("config.unknown_path")))?;
    }
    Ok(value)
}

fn json_toml(value: &serde_json::Value) -> Result<Item> {
    use serde_json::Value as Json;
    Ok(match value {
        Json::Null => Item::None,
        Json::Bool(value) => Item::Value((*value).into()),
        Json::String(value) => Item::Value(value.clone().into()),
        Json::Number(value) => {
            let number = if let Some(integer) = value.as_i64() {
                Value::from(integer)
            } else {
                ensure!(!value.is_u64(), "{}", tr("config.invalid_value"));
                Value::from(value.as_f64().with_context(|| tr("config.invalid_value"))?)
            };
            Item::Value(number)
        }
        Json::Array(values) => {
            let mut array = toml_edit::Array::new();
            for value in values {
                let item = json_toml(value)?;
                array.push(
                    item.into_value()
                        .map_err(|_| anyhow::anyhow!("{}", tr("config.invalid_value")))?,
                );
            }
            Item::Value(array.into())
        }
        Json::Object(values) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, value) in values {
                if !value.is_null() {
                    table.insert(
                        key,
                        json_toml(value)?
                            .into_value()
                            .map_err(|_| anyhow::anyhow!("{}", tr("config.invalid_value")))?,
                    );
                }
            }
            Item::Value(table.into())
        }
    })
}

fn update_document(
    item: &mut Item,
    effective: &serde_json::Value,
    keys: &[&str],
    value: &serde_json::Value,
) -> Result<()> {
    let Some((key, rest)) = keys.split_first() else {
        let mut replacement = json_toml(value)?;
        if let (Some(previous), Some(next)) = (item.as_value(), replacement.as_value_mut()) {
            *next.decor_mut() = previous.decor().clone();
        }
        *item = replacement;
        return Ok(());
    };
    if item.is_none() {
        *item = if effective.is_array() {
            json_toml(effective)?
        } else {
            Item::Value(toml_edit::InlineTable::new().into())
        };
    }
    let child = if effective.is_array() {
        item.get_mut(key.parse::<usize>()?)
    } else {
        item.get_mut(*key)
    }
    .with_context(|| tr("config.invalid_path"))?;
    update_document(child, json_path(effective, &[*key])?, rest, value)
}

/// Policy is evaluated on dotted segments, never substring matches of values.
pub fn external_path_protected(path: &str, allow_memory_clean: bool) -> bool {
    let parts: Vec<_> = path.split('.').collect();
    let first = parts.first().copied().unwrap_or_default();
    let last = parts.last().copied().unwrap_or_default();
    matches!(first, "mcp" | "quick_ask" | "hotkey_engine")
        || path == "app.autostart"
        || matches!(
            last,
            "double_click_exe"
                | "command"
                | "executable"
                | "executable_path"
                | "command_path"
                | "script_path"
                | "exe_path"
                | "launcher_path"
                | "editor_path"
                | "pawnio_module_dir"
        )
        || (!allow_memory_clean
            && first == "memory"
            && parts
                .get(1)
                .is_some_and(|key| key.starts_with("auto_clean_")))
}

fn reject_protected_changes(
    path: &str,
    old: &serde_json::Value,
    new: &serde_json::Value,
    allow_memory_clean: bool,
) -> Result<()> {
    if old == new {
        return Ok(());
    }
    ensure!(
        !external_path_protected(path, allow_memory_clean),
        "{}: {path}",
        tr("config.protected")
    );
    if old.is_object() || new.is_object() {
        let keys: std::collections::BTreeSet<_> = old
            .as_object()
            .into_iter()
            .flat_map(|map| map.keys())
            .chain(new.as_object().into_iter().flat_map(|map| map.keys()))
            .collect();
        for key in keys {
            let nested = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            reject_protected_changes(&nested, &old[key], &new[key], allow_memory_clean)?;
        }
    } else if old.is_array() || new.is_array() {
        let count = old
            .as_array()
            .map_or(0, Vec::len)
            .max(new.as_array().map_or(0, Vec::len));
        for index in 0..count {
            reject_protected_changes(
                &format!("{path}.{index}"),
                &old[index],
                &new[index],
                allow_memory_clean,
            )?;
        }
    }
    Ok(())
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::Storage::FileSystem::{
                MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
            },
            core::PCWSTR,
        };
        let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(destination.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )?;
        }
    }
    #[cfg(not(windows))]
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_ask_migrates_legacy_settings_without_losing_explicit_credentials() -> Result<()> {
        let config: QuickAskConfig = toml_edit::de::from_str(
            r#"
provider = "OpenAI"
base_url = "http://localhost:8080/v1"
model = "local-model"
credential_target = "Orbment/QuickAsk/default"
max_tokens = 2048
"#,
        )?;
        config.validate()?;
        assert_eq!(config.active_channel, "openai");
        assert_eq!(
            config.ordered_channel_ids(),
            ["openai", "openai_compatible", "anthropic", "google"]
        );
        assert_eq!(config.base_url, "http://localhost:8080/v1");
        assert_eq!(config.model, "local-model");
        assert_eq!(config.max_tokens, 2048);
        assert_eq!(config.thinking_effort, ThinkingEffort::Off);
        assert_eq!(
            config.active_channel_config().credential_target,
            "Orbment/QuickAsk/default"
        );
        assert_eq!(config.channels["openai"].model, "local-model");
        for provider in QuickAskProvider::ALL {
            let sparse: QuickAskConfig =
                serde_json::from_value(serde_json::json!({"provider": provider}))?;
            sparse.validate()?;
            assert_eq!(sparse.active_channel, provider.default_channel_id());
            assert_eq!(sparse.base_url, provider.default_base_url());
            assert_eq!(sparse.model, provider.default_model());
            assert_eq!(
                sparse.credential_target,
                provider.default_credential_target()
            );
            for target in ["custom/key", "Orbment/QuickAsk/default"] {
                let explicit: QuickAskConfig = serde_json::from_value(serde_json::json!({
                    "provider": provider,
                    "credential_target": target,
                    "model": "custom-model"
                }))?;
                assert_eq!(explicit.credential_target, target);
                assert_eq!(explicit.active_channel_config().credential_target, target);
                assert_eq!(explicit.active_channel_config().model, "custom-model");
                for other in QuickAskProvider::ALL
                    .into_iter()
                    .filter(|other| *other != provider)
                {
                    assert_eq!(
                        explicit.channels[other.default_channel_id()],
                        QuickAskChannelConfig::for_provider(other)
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn quick_ask_migrates_pascal_case_channels_and_top_level_overrides() -> Result<()> {
        let mut input = serde_json::json!({
            "provider": "Google",
            "channels": {
                "Google": {
                    "model": "custom-gemini",
                    "thinking_effort": "Max",
                    "cached_models": ["custom-gemini", "another-model"]
                },
                "Anthropic": {"thinking_effort": "Low", "credential_target": "Orbment/QuickAsk/default"}
            }
        });
        let config: QuickAskConfig = serde_json::from_value(input.clone())?;
        config.validate()?;
        assert_eq!(config.active_channel, "google");
        assert_eq!(config.model, "custom-gemini");
        assert_eq!(config.base_url, QuickAskProvider::Google.default_base_url());
        assert_eq!(config.thinking_effort, ThinkingEffort::Max);
        assert_eq!(
            config.channels["anthropic"].model,
            QuickAskProvider::Anthropic.default_model()
        );
        assert_eq!(
            config.channels["anthropic"].credential_target,
            "Orbment/QuickAsk/default"
        );
        assert!(!config.channels.contains_key("Google"));
        input["model"] = serde_json::json!("legacy-override");
        input["active_channel"] = serde_json::json!("Google");
        let config: QuickAskConfig = serde_json::from_value(input)?;
        assert_eq!(config.model, "legacy-override");
        assert_eq!(config.channels["google"].model, "legacy-override");
        assert_eq!(config.channels["google"].cached_models.len(), 2);
        let round_trip: QuickAskConfig = serde_json::from_str(&serde_json::to_string(&config)?)?;
        assert_eq!(
            serde_json::to_value(round_trip)?,
            serde_json::to_value(config)?
        );
        Ok(())
    }

    #[test]
    fn quick_ask_imports_arbitrary_map_and_vector_channels_with_stable_order() -> Result<()> {
        use serde_json::json;
        let input = json!({
            "active_channel": "OpenAI",
            "model": "top-level-choice",
            "channels": [
                {"id": "z-last", "name": "本地代理", "provider": "OpenAiCompatible", "model": "local"},
                {"id": "OpenAI", "name": "自定义 Responses", "provider": "OpenAI", "model": "saved",
                 "cached_models": ["saved"], "credential_target": "shared/custom"},
                {"id": "a-first", "name": "另一代理", "provider": "OpenAiCompatible"}
            ]
        });
        let config: QuickAskConfig = serde_json::from_value(input)?;
        config.validate()?;
        assert_eq!(
            config.ordered_channel_ids(),
            ["z-last", "OpenAI", "a-first"]
        );
        assert_eq!(config.channels.len(), 3);
        assert_eq!(config.active_channel, "OpenAI");
        assert_eq!(config.model, "top-level-choice");
        assert_eq!(config.channels["OpenAI"].model, "top-level-choice");
        assert_eq!(config.channels["OpenAI"].name, "自定义 Responses");
        assert_eq!(config.channels["OpenAI"].cached_models, ["saved"]);
        assert_eq!(config.channels["OpenAI"].credential_target, "shared/custom");
        assert_eq!(
            config.channels["z-last"].credential_target,
            "Orbment/QuickAsk/z-last"
        );
        let restored: QuickAskConfig = serde_json::from_value(serde_json::to_value(&config)?)?;
        assert_eq!(
            serde_json::to_value(restored)?,
            serde_json::to_value(config)?
        );
        let sparse: QuickAskConfig = serde_json::from_value(json!({
            "channels": {
                "z-last": {"provider": "Google"},
                "OpenAI": {"id": "OpenAI", "provider": "OpenAiCompatible", "name": "Keep exact ID"}
            }
        }))?;
        sparse.validate()?;
        assert_eq!(sparse.ordered_channel_ids(), ["OpenAI", "z-last"]);
        assert_eq!(
            sparse.channels["OpenAI"].provider,
            QuickAskProvider::OpenAiCompatible
        );
        assert_eq!(sparse.channels["z-last"].model, "gemini-2.5-flash");
        assert!(
            serde_json::from_value::<QuickAskConfig>(json!({
                "channels": [{"id": "duplicate"}, {"id": "duplicate"}]
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<QuickAskConfig>(json!({
                "channels": {"OpenAI": {}, "openai": {"id": "openai"}}
            }))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn quick_ask_editable_channel_drafts_deserialize_before_validation() -> Result<()> {
        for provider in QuickAskProvider::ALL {
            for field in ["id", "name", "base_url", "model", "credential_target"] {
                let mut input = serde_json::to_value(QuickAskConfig::default())?;
                input["channels"][provider.default_channel_id()][field] = serde_json::json!("");
                if provider == QuickAskProvider::OpenAI && QUICK_ASK_ACTIVE_FIELDS.contains(&field)
                {
                    input[field] = serde_json::json!("");
                }
                let draft: QuickAskConfig = serde_json::from_value(input)?;
                assert!(draft.validate().is_err(), "{}.{field}", provider.as_key());
                let restored: QuickAskConfig =
                    serde_json::from_value(serde_json::to_value(&draft)?)?;
                assert!(restored.validate().is_err());
                assert_eq!(
                    serde_json::to_value(restored)?,
                    serde_json::to_value(draft)?
                );
            }
        }
        Ok(())
    }

    #[test]
    fn quick_ask_switches_four_independent_channels_and_retains_model_caches() -> Result<()> {
        let mut config = QuickAskConfig::default();
        let efforts = [
            ThinkingEffort::Low,
            ThinkingEffort::Medium,
            ThinkingEffort::High,
            ThinkingEffort::Max,
        ];
        let mut expected = BTreeMap::new();
        for (provider, effort) in QuickAskProvider::ALL.into_iter().zip(efforts) {
            let id = provider.default_channel_id();
            config.switch_channel(id)?;
            config.base_url = format!("https://{id}.example/v1");
            config.model = format!("{id}-model");
            config.credential_target = format!("custom/{id}");
            config.thinking_effort = effort;
            config.channels.get_mut(id).unwrap().cached_models = vec![config.model.clone()];
            config.sync_active_channel();
            expected.insert(id, config.active_channel_config().clone());
        }
        for provider in QuickAskProvider::ALL.into_iter().rev() {
            config.switch_channel(provider.default_channel_id())?;
            let channel = &expected[provider.default_channel_id()];
            assert_eq!(config.provider, channel.provider);
            assert_eq!(config.base_url, channel.base_url);
            assert_eq!(config.model, channel.model);
            assert_eq!(config.credential_target, channel.credential_target);
            assert_eq!(config.thinking_effort, channel.thinking_effort);
            assert_eq!(config.active_channel_config(), channel);
            config.validate()?;
        }
        Ok(())
    }

    #[test]
    fn quick_ask_custom_channels_isolate_same_protocol_and_preserve_renames() -> Result<()> {
        let mut config = QuickAskConfig::default();
        let first = config.add_channel();
        assert_eq!(first, "channel-5");
        assert_eq!(config.active_channel, first);
        assert_eq!(config.provider, QuickAskProvider::OpenAiCompatible);
        assert_eq!(config.active_channel_config().name, "自定义渠道 5");
        assert_eq!(config.credential_target, "Orbment/QuickAsk/channel-5");
        config.base_url = "https://first.example/v1".into();
        config.model = "first-model".into();
        config.thinking_effort = ThinkingEffort::Low;
        let channel = config.channels.get_mut(&first).unwrap();
        channel.name = "工作代理".into();
        channel.cached_models = vec!["first-model".into()];
        let second = config.add_channel();
        assert_eq!(second, "channel-6");
        assert_eq!(config.channels[&first].model, "first-model");
        config.base_url = "https://second.example/v1".into();
        config.model = "second-model".into();
        config.thinking_effort = ThinkingEffort::Max;
        config.sync_active_channel();
        let second_channel = config.active_channel_config().clone();
        config.switch_channel(&first)?;
        assert_eq!(config.model, "first-model");
        assert_eq!(config.active_channel_config().name, "工作代理");
        assert_eq!(
            config.active_channel_config().cached_models,
            ["first-model"]
        );
        let mut expected = config.active_channel_config().clone();
        config.switch_provider(QuickAskProvider::OpenAI);
        expected.provider = QuickAskProvider::OpenAI;
        assert_eq!(config.active_channel_config(), &expected);
        assert_eq!(
            config.active_channel_config().default_credential_target(),
            "Orbment/QuickAsk/channel-5"
        );
        assert_eq!(config.channels[&second], second_channel);
        config.switch_channel(&second)?;
        assert_eq!(config.model, "second-model");
        assert_eq!(config.provider, QuickAskProvider::OpenAiCompatible);
        assert_eq!(config.channel_order.last(), Some(&second));
        config.validate()?;
        let restored: QuickAskConfig = serde_json::from_value(serde_json::to_value(&config)?)?;
        assert_eq!(
            serde_json::to_value(restored)?,
            serde_json::to_value(config)?
        );
        Ok(())
    }

    #[test]
    fn quick_ask_channel_removal_returns_saved_state_and_rejects_last_atomically() -> Result<()> {
        let mut config = QuickAskConfig::default();
        let id = config.add_channel();
        config.model = "unsynced-model".into();
        let removed = config.remove_channel("anthropic")?;
        assert_eq!(removed.id, "anthropic");
        assert_eq!(config.active_channel, id);
        assert_eq!(config.channels[&id].model, "unsynced-model");
        let removed = config.remove_channel(&id)?;
        assert_eq!(removed.model, "unsynced-model");
        assert_eq!(removed.credential_target, "Orbment/QuickAsk/channel-5");
        assert_eq!(config.active_channel, "openai");
        assert_eq!(
            config.ordered_channel_ids(),
            ["openai", "openai_compatible", "google"]
        );
        config.remove_channel("openai")?;
        assert_eq!(config.active_channel, "openai_compatible");
        config.remove_channel("google")?;
        let before = serde_json::to_value(&config)?;
        assert!(config.remove_channel("openai_compatible").is_err());
        assert!(config.remove_channel("missing").is_err());
        assert!(config.switch_channel("missing").is_err());
        assert_eq!(serde_json::to_value(&config)?, before);
        config.validate()?;
        Ok(())
    }

    #[test]
    fn quick_ask_store_switches_save_legacy_channels_and_restore_dotted_edits() -> Result<()> {
        use serde_json::json;
        let dir =
            std::env::temp_dir().join(format!("orbment-quick-ask-channels-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("config.toml"),
            "# retained\n[quick_ask]\nprovider = 'OpenAI'\nbase_url = 'http://localhost:8080/v1'\nmodel = 'legacy-model' # custom\ncredential_target = 'custom/legacy'\nfuture_key = 42\n",
        )?;
        let mut store = ConfigStore::load(&dir)?;
        store.set_json("quick_ask.active_channel", json!("anthropic"))?;
        store.reload()?;
        assert_eq!(
            store.config.quick_ask.model,
            QuickAskProvider::Anthropic.default_model()
        );
        store.set_json("quick_ask.active_channel", json!("openai"))?;
        assert_eq!(store.config.quick_ask.model, "legacy-model");
        assert_eq!(store.config.quick_ask.base_url, "http://localhost:8080/v1");
        assert_eq!(store.config.quick_ask.credential_target, "custom/legacy");
        let mut expected = BTreeMap::new();
        for provider in QuickAskProvider::ALL {
            let id = provider.default_channel_id();
            store.apply_json("quick_ask.active_channel", json!(id))?;
            store.apply_json(
                "quick_ask.base_url",
                json!(format!("https://{id}.example/v1")),
            )?;
            store.apply_json("quick_ask.model", json!(format!("{id}-selected")))?;
            store.apply_json("quick_ask.credential_target", json!(format!("custom/{id}")))?;
            store.apply_json("quick_ask.thinking_effort", json!("High"))?;
            let before = store.config.quick_ask.model.clone();
            store.apply_json(
                &format!("quick_ask.channels.{id}.cached_models"),
                json!(["first-fetched-model", "second-fetched-model"]),
            )?;
            assert_eq!(store.config.quick_ask.model, before);
            expected.insert(id, store.config.quick_ask.active_channel_config().clone());
        }
        store.save()?;
        store.reload()?;
        for provider in QuickAskProvider::ALL {
            let id = provider.default_channel_id();
            store.set_json("quick_ask.active_channel", json!(id))?;
            store.reload()?;
            let channel = &expected[id];
            assert_eq!(store.config.quick_ask.active_channel_config(), channel);
            assert_eq!(store.config.quick_ask.base_url, channel.base_url);
            assert_eq!(store.config.quick_ask.model, channel.model);
            assert_eq!(
                store.config.quick_ask.credential_target,
                channel.credential_target
            );
            assert_eq!(
                store.config.quick_ask.thinking_effort,
                channel.thinking_effort
            );
        }
        store.set_json("quick_ask.channels.google.model", json!("new-active-model"))?;
        assert_eq!(store.config.quick_ask.model, "new-active-model");
        store.set_json(
            "quick_ask.channels.anthropic.model",
            json!("new-inactive-model"),
        )?;
        assert_eq!(store.config.quick_ask.model, "new-active-model");
        store.set_json(
            "quick_ask.channels.anthropic.cached_models",
            json!(["new-inactive-model"]),
        )?;
        store.reload()?;
        assert_eq!(store.config.quick_ask.model, "new-active-model");
        store.set_json("quick_ask.active_channel", json!("anthropic"))?;
        assert_eq!(store.config.quick_ask.model, "new-inactive-model");
        assert_eq!(
            store.config.quick_ask.active_channel_config().cached_models,
            ["new-inactive-model"]
        );
        let saved = fs::read_to_string(store.path())?;
        assert!(saved.contains("# retained") && saved.contains("# custom"));
        assert!(saved.contains("future_key = 42"));
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn quick_ask_store_edits_protocol_metadata_and_channel_order_independently() -> Result<()> {
        use serde_json::json;
        let dir =
            std::env::temp_dir().join(format!("orbment-quick-ask-custom-{}", std::process::id()));
        let mut store = ConfigStore::load(&dir)?;
        let mut config = store.config.quick_ask.clone();
        let id = config.add_channel();
        config.model = "custom-selected".into();
        config.sync_active_channel();
        store.set_json("quick_ask", serde_json::to_value(config)?)?;
        store.set_json(&format!("quick_ask.channels.{id}.name"), json!("代理工作"))?;
        store.set_json(
            &format!("quick_ask.channels.{id}.cached_models"),
            json!(["available-only"]),
        )?;
        let mut expected = store.config.quick_ask.active_channel_config().clone();
        store.set_json("quick_ask.provider", json!("OpenAI"))?;
        expected.provider = QuickAskProvider::OpenAI;
        assert_eq!(store.config.quick_ask.active_channel, id);
        assert_eq!(store.config.quick_ask.active_channel_config(), &expected);
        assert_eq!(store.config.quick_ask.model, "custom-selected");
        store.set_json(
            &format!("quick_ask.channels.{id}.provider"),
            json!("Anthropic"),
        )?;
        assert_eq!(store.config.quick_ask.provider, QuickAskProvider::Anthropic);
        assert_eq!(store.config.quick_ask.model, "custom-selected");
        let mut order = store.config.quick_ask.channel_order.clone();
        order.reverse();
        store.set_json("quick_ask.channel_order", json!(order))?;
        let mut channels = store.get("quick_ask.channels")?;
        channels["extra"] = json!({"id": "extra", "name": "附加渠道", "provider": "Google"});
        store.set_json("quick_ask.channels", channels)?;
        order.push("extra".into());
        assert_eq!(store.config.quick_ask.channel_order, order);
        let mut imported = store.get("quick_ask")?;
        imported["model"] = json!("imported-selection");
        imported["channels"][&id]["model"] = json!("stale-mirror");
        store.set_json("quick_ask", imported)?;
        store.reload()?;
        assert_eq!(store.config.quick_ask.model, "imported-selection");
        assert_eq!(
            store.config.quick_ask.active_channel_config().model,
            "imported-selection"
        );
        assert_eq!(
            store.config.quick_ask.active_channel_config().name,
            "代理工作"
        );
        assert_eq!(
            store.config.quick_ask.active_channel_config().cached_models,
            ["available-only"]
        );
        assert_eq!(store.config.quick_ask.channel_order, order);
        let mut config = store.config.quick_ask.clone();
        config.remove_channel(&id)?;
        store.set_json("quick_ask", serde_json::to_value(&config)?)?;
        store.reload()?;
        assert_eq!(
            serde_json::to_value(&store.config.quick_ask)?,
            serde_json::to_value(config)?
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn quick_ask_store_rejects_invalid_active_and_inactive_channels_atomically() -> Result<()> {
        use serde_json::json;
        let dir =
            std::env::temp_dir().join(format!("orbment-quick-ask-invalid-{}", std::process::id()));
        let mut store = ConfigStore::load(&dir)?;
        store.set_json(
            "quick_ask.channels.google.base_url",
            json!("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"),
        )?;
        let before = store.get("quick_ask")?;
        let disk = fs::read(store.path())?;
        for provider in QuickAskProvider::ALL {
            for (field, value) in [
                ("id", json!("mismatched-id")),
                ("name", json!(" ")),
                ("provider", json!("Unsupported")),
                ("base_url", json!("file:///tmp/model")),
                ("base_url", json!("https:///missing-host")),
                ("base_url", json!("https://example.test:bad-port/v1")),
                ("base_url", json!("https://example.test/v1?key=secret")),
                ("base_url", json!("https://user@example.test/v1")),
                ("base_url", json!(null)),
                ("model", json!(" ")),
                ("credential_target", json!("")),
                ("credential_target", json!("invalid\0target")),
                ("credential_target", json!("x".repeat(32768))),
                ("thinking_effort", json!("Ultra")),
                ("cached_models", json!([""])),
                ("cached_models", json!([42])),
            ] {
                let path = format!(
                    "quick_ask.channels.{}.{field}",
                    provider.default_channel_id()
                );
                assert!(store.set_json(&path, value.clone()).is_err(), "{path}");
                assert!(store.apply_json(&path, value).is_err(), "{path}");
                assert_eq!(store.get("quick_ask")?, before);
                assert_eq!(fs::read(store.path())?, disk);
            }
        }
        for (path, value) in [
            ("quick_ask.provider", json!("Unsupported")),
            ("quick_ask.active_channel", json!("missing")),
            ("quick_ask.model", json!("")),
            ("quick_ask.credential_target", json!("\0")),
            ("quick_ask.channel_order", json!(["google"])),
            (
                "quick_ask.channel_order",
                json!(["openai", "openai", "anthropic", "google"]),
            ),
            (
                "quick_ask.channel_order",
                json!(["openai", "missing", "anthropic", "google"]),
            ),
            (
                "quick_ask.channels.anthropic.name",
                json!(store.config.quick_ask.channels["openai"].name),
            ),
            ("quick_ask.channels", json!({})),
            (
                "quick_ask",
                json!({"channels": {"Anthropic": {"model": ""}}}),
            ),
            (
                "quick_ask",
                json!({"channels": [], "active_channel": "missing"}),
            ),
        ] {
            assert!(store.set_json(path, value).is_err(), "{path}");
            assert_eq!(store.get("quick_ask")?, before);
            assert_eq!(fs::read(store.path())?, disk);
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn quick_ask_protocol_endpoints_normalize_responses_chat_and_model_lists() -> Result<()> {
        for (provider, suffix) in [
            (QuickAskProvider::OpenAI, "/responses"),
            (QuickAskProvider::OpenAiCompatible, "/chat/completions"),
            (QuickAskProvider::Anthropic, "/messages"),
        ] {
            for (input, expected_base) in [
                ("https://proxy.example".to_owned(), "/v1"),
                ("https://proxy.example/v1/".to_owned(), "/v1"),
                (format!("https://proxy.example/custom{suffix}"), "/custom"),
                (format!("https://proxy.example{suffix}"), ""),
                ("https://proxy.example/v1/models".to_owned(), "/v1"),
            ] {
                let config = QuickAskConfig {
                    provider,
                    base_url: input,
                    ..Default::default()
                };
                assert_eq!(
                    config.chat_endpoint_url()?.path(),
                    format!("{expected_base}{suffix}")
                );
                assert_eq!(
                    config.models_endpoint_url()?.path(),
                    format!("{expected_base}/models")
                );
            }
        }
        for provider in [QuickAskProvider::OpenAI, QuickAskProvider::OpenAiCompatible] {
            for endpoint in ["/v1/responses", "/v1/chat/completions"] {
                let config = QuickAskConfig {
                    provider,
                    base_url: format!("https://proxy.example{endpoint}"),
                    ..Default::default()
                };
                assert_eq!(config.models_endpoint_url()?.path(), "/v1/models");
                assert_eq!(
                    config.chat_endpoint_url()?.path(),
                    if provider == QuickAskProvider::OpenAI {
                        "/v1/responses"
                    } else {
                        "/v1/chat/completions"
                    }
                );
            }
        }
        let config = QuickAskConfig {
            provider: QuickAskProvider::Google,
            base_url: "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse".into(),
            ..Default::default()
        };
        assert_eq!(config.chat_endpoint_url()?.query(), Some("alt=sse"));
        assert_eq!(config.models_endpoint_url()?.path(), "/v1beta/models");
        assert_eq!(config.models_endpoint_url()?.query(), None);
        Ok(())
    }

    #[test]
    fn new_and_sparse_vision_configs_leave_shell_integration_disabled() -> Result<()> {
        for vision in [
            VisionConfig::default(),
            serde_json::from_value(serde_json::json!({}))?,
            toml_edit::de::from_str("")?,
        ] {
            assert!(!vision.shell_context_menu);
        }
        Ok(())
    }

    #[test]
    fn default_shortcuts_enable_capture_and_avoid_optional_conflicts() -> Result<()> {
        for vision in [
            VisionConfig::default(),
            serde_json::from_value(serde_json::json!({}))?,
        ] {
            assert!(vision.allow_simple_hotkey);
            assert_eq!(
                vision.hotkeys.get("capture").map(String::as_str),
                Some("F1")
            );
            assert_eq!(vision.hotkeys.get("pin").map(String::as_str), Some("F2"));
            vision.validate()?;
        }
        assert!(UtilitiesConfig::default().color_picker_hotkey.is_none());
        assert!(AudioConfig::default().mute_recording_hotkey.is_empty());
        assert!(ClipboardConfig::default().open_preference_hotkey.is_empty());
        assert_eq!(ClipboardConfig::default().open_clipboard_hotkey, "Alt+C");
        Ok(())
    }

    #[test]
    fn explicit_vision_shortcuts_override_new_defaults() -> Result<()> {
        let disabled: VisionConfig = serde_json::from_value(serde_json::json!({
            "allow_simple_hotkey": false, "hotkeys": {}
        }))?;
        assert!(!disabled.allow_simple_hotkey);
        assert!(disabled.hotkeys.is_empty());
        let custom: VisionConfig = serde_json::from_value(serde_json::json!({
            "hotkeys": {"capture": "Ctrl+F9", "pin_last": "Ctrl+F10"}
        }))?;
        assert_eq!(custom.hotkeys.len(), 2);
        assert_eq!(custom.hotkeys["capture"], "Ctrl+F9");
        assert_eq!(custom.hotkeys["pin_last"], "Ctrl+F10");
        Ok(())
    }

    #[test]
    fn clipboard_deletion_checks_protection_before_confirmation() {
        use ClipboardDeletePolicy::{Allowed, ConfirmationRequired, Protected};
        let mut config = ClipboardContentConfig::default();
        assert_eq!(
            config.delete_policy(false, false, false),
            ConfirmationRequired
        );
        assert_eq!(config.delete_policy(true, false, true), Protected);
        assert_eq!(config.delete_policy(false, true, true), Protected);

        config.delete_confirm = false;
        config.delete_favorite_items = true;
        assert_eq!(config.delete_policy(false, false, false), Allowed);
        assert_eq!(config.delete_policy(true, false, false), Protected);
        assert_eq!(
            config.delete_policy(true, false, true),
            ConfirmationRequired
        );
        assert_eq!(config.delete_policy(true, true, true), Protected);

        config.delete_pinned_items = true;
        config.delete_favorite_items_confirm = false;
        assert_eq!(config.delete_policy(true, false, true), Allowed);
        assert_eq!(config.delete_policy(true, true, true), ConfirmationRequired);
        config.delete_pinned_items_confirm = false;
        assert_eq!(config.delete_policy(true, true, true), Allowed);
        assert_eq!(config.delete_policy(true, true, false), Protected);
        config.delete_favorite_items_only_in_fav_group = false;
        assert_eq!(config.delete_policy(true, true, false), Allowed);
    }

    #[test]
    fn external_updates_protect_permissions_paths_and_ancestor_replacements() -> Result<()> {
        use serde_json::json;
        let dir =
            std::env::temp_dir().join(format!("orbment-config-protection-{}", std::process::id()));
        let mut store = ConfigStore::load(&dir)?;
        for (path, value) in [
            ("mcp.allow_memory_clean", json!(true)),
            ("mcp", json!({"allow_memory_clean":true})),
            ("app.autostart", json!(true)),
            ("app", json!({"autostart":true})),
            ("monitor.double_click_exe", json!("untrusted.exe")),
            ("monitor.pawnio_module_dir", json!("untrusted")),
            ("monitor", json!({"pawnio_module_dir":"untrusted"})),
            ("memory.auto_clean_threshold_enabled", json!(true)),
            ("memory", json!({"auto_clean_interval_enabled":true})),
            (
                "hotkey_engine",
                json!({"rules":[{"command":"untrusted.exe"}]}),
            ),
            ("quick_ask", json!({"endpoint":"https://untrusted.invalid"})),
            ("quick_ask.enabled", json!(true)),
            ("hotkey_engine.enabled", json!(true)),
        ] {
            assert!(store.set_external(path, value).is_err(), "{path}");
        }
        store.set_external("monitor.sample_interval_ms", json!(1500))?;
        assert_eq!(store.get("monitor.sample_interval_ms")?, 1500);
        let mut monitor = store.get("monitor")?;
        monitor["sample_interval_ms"] = json!(2000);
        store.set_external("monitor", monitor)?;
        assert_eq!(store.get("monitor.sample_interval_ms")?, 2000);
        store.set_json("mcp.allow_memory_clean", json!(true))?;
        store.set_external("memory.auto_clean_threshold_enabled", json!(true))?;
        assert!(store.config.memory.auto_clean_threshold_enabled);
        store.set_json("app.autostart", json!(true))?;
        // Omitting an existing protected value in an ancestor replacement resets defaults.
        assert!(
            store
                .set_external("app", json!({"language":"zh-CN"}))
                .is_err()
        );
        assert!(store.config.app.autostart);
        store.set_external(
            "audio.knob_remap",
            json!({
                "action":"audio.switch_playback", "future_executable_path":"untrusted.exe"
            }),
        )?;
        assert!(!fs::read_to_string(store.path())?.contains("untrusted.exe"));
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn typed_nested_updates_are_atomic_and_preserve_profile_options() -> Result<()> {
        use serde_json::json;
        let dir =
            std::env::temp_dir().join(format!("orbment-config-nested-{}", std::process::id()));
        let mut store = ConfigStore::load(&dir)?;
        store.set_json(
            "audio.profiles",
            json!([{"id":"desk","name":"Desk","playback_device_id":"speaker"}]),
        )?;
        store.set_json("audio.profiles.0.playback_device_id", json!(null))?;
        store.set_json("audio.profiles.0.name", json!("Updated"))?;
        store.set_json("monitor.floating", json!({"enabled":true}))?;
        store.set_json("monitor.floating.pos_x", json!(25))?;
        store.set_json("monitor.sample_interval_ms", json!(2000))?;
        let before = fs::read(store.path())?;
        for (path, value) in [
            ("monitor.sample_interval_ms", json!(0)),
            ("monitor.sample_interval_ms", json!("1000")),
            ("audio.enabled", json!(null)),
            ("audio.switch_device_notification", json!("unknown")),
            ("audio.profiles.0.id", json!("")),
            ("audio..enabled", json!(true)),
            ("audio.profiles.9.name", json!("missing")),
            ("app.language", json!("en-US")),
            (
                "clipboard.window.select_category_on_open",
                json!("favorite"),
            ),
            (
                "vision.global_mouse.bindings",
                json!([{"action":"vision.pin_create"}]),
            ),
        ] {
            assert!(store.set_json(path, value).is_err(), "{path}");
            assert_eq!(fs::read(store.path())?, before);
        }
        let loaded = ConfigStore::load(&dir)?;
        assert_eq!(loaded.config.audio.profiles[0].name, "Updated");
        assert!(loaded.config.audio.profiles[0].playback_device_id.is_none());
        assert_eq!(loaded.config.monitor.floating.pos_x, 25);
        assert_eq!(loaded.config.monitor.sample_interval_ms, 2000);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn saving_migrated_audio_selections_preserves_explicit_knob_settings() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("orbment-audio-selections-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("config.toml"),
            r#"audio = { knob_remap = { intercept_volume_mute = true }, future_option = 42, selected_playback_devices = [{ id = "old", name = "Speaker" }] }"#,
        )?;
        let mut store = ConfigStore::load(&dir)?;
        let playback = [AudioDeviceSelection {
            id: "new".into(),
            name: "Speaker".into(),
        }];
        assert!(store.save_audio_selections(&playback, &[])?);
        store.reload()?;
        assert_eq!(store.config.audio.selected_playback_devices, playback);
        assert!(store.config.audio.knob_remap.intercept_volume_mute);
        assert!(!AudioConfig::default().knob_remap.intercept_volume_mute);
        let saved = fs::read_to_string(store.path())?;
        assert!(saved.contains("future_option = 42"));
        assert!(!saved.contains("selected_recording_devices"));
        fs::remove_file(store.path())?;
        fs::remove_dir(dir)?;
        Ok(())
    }

    #[test]
    fn sparse_updates_preserve_unknowns_comments_and_defaults() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("orbment-config-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("config.toml"),
            "# retained\n[audio]\nenabled = false # switch\nfuture_key = 42\n[plugin]\nname = 'extension'\n",
        )?;
        let mut store = ConfigStore::load(&dir)?;
        assert!(!store.config.audio.enabled);
        assert_eq!(store.config.monitor.sample_interval_ms, 1000);
        store.set_json("audio.enabled", serde_json::json!(true))?;
        let saved = fs::read_to_string(store.path())?;
        assert!(saved.contains("# retained") && saved.contains("# switch"));
        assert!(saved.contains("future_key = 42") && saved.contains("name = 'extension'"));
        assert!(!saved.contains("sample_interval_ms"));
        assert!(ConfigStore::load(&dir)?.config.audio.enabled);
        store.set_json("monitor.taskbar.text_colors", serde_json::json!([]))?;
        assert!(
            ConfigStore::load(&dir)?
                .config
                .monitor
                .taskbar
                .text_colors
                .is_empty()
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn portable_and_explicit_directory_precedence() -> Result<()> {
        let root = std::env::temp_dir().join(format!("orbment-portable-{}", std::process::id()));
        fs::create_dir_all(&root)?;
        fs::write(root.join("portable.flag"), "")?;
        assert_eq!(
            resolve_data_dir(&root, None, None, None)?,
            root.join("OrbmentData")
        );
        assert_eq!(
            resolve_data_dir(
                &root,
                Some(Path::new("explicit")),
                Some(Path::new("env")),
                None
            )?,
            PathBuf::from("explicit")
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn legacy_taskbar_config_gains_layout_defaults_without_overwriting_choices() -> Result<()> {
        let config: TaskbarConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "display_mask": 15,
            "back_color": 123
        }))?;
        assert!(config.transparent_background);
        assert!(!config.horizontal_arrange);
        assert_eq!(
            config.item_order,
            vec![0, 1, 10, 4, 2, 6, 5, 3, 11, 7, 8, 9, 12]
        );
        assert_eq!(config.back_color, 123);
        let explicit: TaskbarConfig = serde_json::from_value(serde_json::json!({
            "transparent_background": false,
            "horizontal_arrange": true,
            "item_order": [4, 0]
        }))?;
        assert!(!explicit.transparent_background);
        assert!(explicit.horizontal_arrange);
        assert_eq!(explicit.item_order, vec![4, 0]);
        Ok(())
    }
}
