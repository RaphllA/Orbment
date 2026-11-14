#![windows_subsystem = "windows"]

mod client;
mod clipboard;
mod elevation;
mod file_tools;
mod memory;
mod modules;
mod preview;
mod quickask;
mod recording_editor;
mod vision;

use anyhow::{Context, Result, bail, ensure};
use clipboard::ClipboardModule;
use file_tools::FileTools;
use memory::MemoryModule;
use modules::{AudioModule, MonitorModule};
use orbment_analyzer::memory::MemoryEvent;
use orbment_audio::AudioEvent;
use orbment_core::{
    action_bus::ActionBus,
    autostart,
    config::{ConfigStore, atomic_write, resolve_data_dir},
    db, diag,
    i18n::{self, tr},
    ipc::{self, PendingRequest, PipeServer, RequestSender},
    lifecycle::{ActivateTrigger, CoreContext, ModuleManager, OrbmentModule, ServiceState},
    logging,
    ocr::OcrDecision,
    single_instance,
};
use orbment_monitor::MonitorUpdate;
use orbment_ui::{
    D2dContext, EventLoop, FileToolsAction, Hotkeys, MainHostWindow, MenuItem, SearchPopupWindow,
    SettingsEvent, Tray, UiEvent, UtilitiesModule, wake,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{self, Sender},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HWND,
        UI::Shell::NIN_SELECT,
        UI::WindowsAndMessaging::{
            IDYES, MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_YESNO, MessageBoxW, PostQuitMessage,
            WM_CONTEXTMENU, WM_LBUTTONDBLCLK, WM_MBUTTONUP,
        },
    },
    core::PCWSTR,
};

// shellapi.h defines this as NIN_SELECT | NINF_KEY; windows-rs omits the macro.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

pub enum Event {
    Audio(AudioEvent),
    Memory(MemoryEvent),
    Monitor(Box<MonitorUpdate>),
    MonitorPosition(i32, i32),
    MonitorAction(u8),
    MonitorHistory(Result<Value>),
    ActionCompleted(Result<Value>),
}
pub fn send_event(sender: &Sender<Event>, hwnd: usize, event: Event) {
    if sender.send(event).is_ok() {
        wake(HWND(hwnd as *mut std::ffi::c_void));
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn show_error(title: &str, error: &anyhow::Error) {
    let title = wide(title);
    let text = wide(&format!("{error:#}"));
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

struct Options {
    data_dir: Option<PathBuf>,
    restart_from: Option<u32>,
    pin_image: Option<PathBuf>,
    render_ui_previews: Option<PathBuf>,
}
impl Options {
    fn parse() -> Result<Self> {
        let mut options = Self {
            data_dir: None,
            restart_from: None,
            pin_image: None,
            render_ui_previews: None,
        };
        let mut args = std::env::args_os().skip(1);
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--data-dir") => {
                    options.data_dir = Some(PathBuf::from(
                        args.next().context("--data-dir requires a path")?,
                    ))
                }
                Some("--pin-image") => {
                    options.pin_image = Some(PathBuf::from(
                        args.next().context("--pin-image requires a path")?,
                    ));
                }
                Some("--render-ui-previews") => {
                    options.render_ui_previews = Some(PathBuf::from(
                        args.next()
                            .context("--render-ui-previews requires an output directory")?,
                    ));
                }
                // Ordinary startup now always runs at the caller's privilege level.
                Some("--no-elevate") => {}
                Some("--restart-from") => {
                    options.restart_from = Some(
                        args.next()
                            .context("--restart-from requires a process ID")?
                            .to_string_lossy()
                            .parse()
                            .context("invalid restart process ID")?,
                    );
                }
                _ => bail!("unknown argument: {}", arg.to_string_lossy()),
            }
        }
        Ok(options)
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GetConfig {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetConfig {
    path: String,
    value: Value,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SwitchDevice {
    device_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteProcess {
    device_id: String,
    pid: Option<u32>,
    process_path: Option<String>,
    process_name: Option<String>,
    #[serde(default)]
    recording: bool,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TrafficHistory {
    start_date: Option<String>,
    end_date: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    id: String,
}
fn parameters<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(if value.is_null() { json!({}) } else { value })
        .context("invalid action parameters")
}

type Action = (String, Value);
struct App {
    store: ConfigStore,
    ctx: CoreContext,
    database: rusqlite::Connection,
    last_checkpoint: Instant,
    last_log_cleanup: Instant,
    manager: ModuleManager,
    audio: Rc<RefCell<AudioModule>>,
    monitor: Rc<RefCell<MonitorModule>>,
    memory: Rc<RefCell<MemoryModule>>,
    clipboard: Rc<RefCell<ClipboardModule>>,
    file_tools: FileTools,
    vision: Rc<RefCell<vision::VisionModule>>,
    quickask: Rc<RefCell<quickask::QuickAskModule>>,
    vision_gestures: Option<orbment_vision::global_mouse::GlobalMouseRegistration>,
    vision_hook_config: Option<Value>,
    search_popup: SearchPopupWindow,
    utilities: Rc<RefCell<UtilitiesModule>>,
    graphics: Rc<D2dContext>,
    settings: Option<MainHostWindow>,
    hotkeys: Hotkeys,
    hotkey_actions: BTreeMap<i32, Action>,
    hotkey_conflicts: Vec<String>,
    monitor_unavailable: BTreeMap<String, String>,
    tray: Tray,
    audio_tray: Option<Tray>,
    memory_tray: Option<Tray>,
    memory_load: u32,
    last_playback: Option<String>,
    last_recording: Option<String>,
    audio_icon_recording: bool,
    hwnd: HWND,
    autostart_enabled: bool,
    pipe: Option<PipeServer>,
    requests: RequestSender,
    events: Sender<Event>,
}
impl App {
    fn start_modules(&mut self) {
        if let Err(error) = self.vision.borrow_mut().start_service(&self.ctx) {
            self.report(&error);
        }
        if let Err(error) = self.quickask.borrow_mut().start_service(&self.ctx) {
            self.report(&error);
        }
        let audio_result = self.audio.borrow_mut().start_service(&self.ctx);
        if let Err(error) = audio_result {
            self.report(&error);
        }
        let monitor_result = self.monitor.borrow_mut().start_service(&self.ctx);
        if let Err(error) = monitor_result {
            self.report(&error);
        }
        let ui_result = self
            .monitor
            .borrow_mut()
            .open_ui(ActivateTrigger::Action, &self.ctx);
        if let Err(error) = ui_result {
            self.report(&error);
        }
        for result in [
            self.memory.borrow_mut().start_service(&self.ctx),
            self.utilities.borrow_mut().start_service(&self.ctx),
            self.clipboard.borrow_mut().start_service(&self.ctx),
        ] {
            if let Err(error) = result {
                self.report(&error);
            }
        }
        if let Err(error) = self.sync_clipboard_hook() {
            self.report(&error);
        }
        if let Err(error) = self.sync_search_hook() {
            self.report(&error);
        }
        if let Err(error) = self.sync_vision_hook() {
            self.report(&error);
        }
        self.register_hotkeys();
        if let Err(error) = self.refresh_audio_tray() {
            self.report(&error);
        }
        if let Err(error) = self.refresh_memory_tray() {
            self.report(&error);
        }
    }
    fn sync_clipboard_hook(&mut self) -> Result<()> {
        let clipboard = &self.ctx.config.clipboard;
        self.audio
            .borrow_mut()
            .hook
            .set_replace_win_v(clipboard.enabled && clipboard.replace_win_v)
    }
    fn sync_search_hook(&mut self) -> Result<()> {
        let search = &self.ctx.config.file_search;
        self.audio
            .borrow_mut()
            .hook
            .set_double_ctrl(search.enabled && search.hotkey.eq_ignore_ascii_case("DoubleCtrl"))
    }
    fn choose_ocr_engine(&mut self) -> Result<()> {
        if self
            .ctx
            .ocr
            .decision(&self.ctx.config.app.ocr_engine_tier)?
            == OcrDecision::ChooseEngine
        {
            self.prepare_modal()?;
            let chosen = orbment_ui::choose_ocr_engine(self.hwnd, |tier| {
                self.set_config("app.ocr_engine_tier", json!(tier), false)?;
                Ok(())
            })?;
            ensure!(chosen.is_some(), "{}", tr("app.cancelled"));
        }
        Ok(())
    }
    fn sync_vision_hook(&mut self) -> Result<()> {
        let enabled = self.ctx.config.vision.enabled
            && self.vision.borrow().service_state() == ServiceState::Running;
        self.utilities.borrow_mut().set_color_pin_enabled(enabled);
        let config = &self.ctx.config.vision;
        let snapshot = json!({
            "enabled": enabled,
            "global_mouse": config.global_mouse,
            "ignore_process_list": config.ignore_process_list,
            "disable_in_fullscreen_game": config.disable_in_fullscreen_game,
        });
        let mut audio = self.audio.borrow_mut();
        let mut failure = None;
        if self.vision_hook_config.as_ref() != Some(&snapshot) {
            let result = (|| -> Result<()> {
                if let Some(gestures) = &mut self.vision_gestures {
                    gestures.uninstall(&mut audio.hook)?;
                }
                self.vision_gestures = None;
                if enabled && !config.global_mouse.bindings.is_empty() {
                    let hwnd = self.hwnd.0 as usize;
                    self.vision_gestures = Some(
                        orbment_vision::global_mouse::GlobalMouseRegistration::install(
                            &mut audio.hook,
                            config,
                            move || wake(HWND(hwnd as *mut _)),
                        )?,
                    );
                }
                self.vision_hook_config = Some(snapshot);
                Ok(())
            })();
            if let Err(error) = result {
                failure = Some(error);
            }
        }
        if let Err(error) = self
            .vision
            .borrow_mut()
            .sync_recording_hooks(&mut audio.hook, &self.ctx)
        {
            failure.get_or_insert(error);
        }
        failure.map_or(Ok(()), Err)
    }
    fn vision_events(&mut self) -> Vec<Action> {
        let (mut actions, errors) = self.vision.borrow_mut().poll();
        if let Err(error) = self
            .vision
            .borrow_mut()
            .sync_recording_hooks(&mut self.audio.borrow_mut().hook, &self.ctx)
        {
            self.report(&error);
        }
        for error in errors {
            self.report(&error);
        }
        for context in self.vision.borrow_mut().take_quick_contexts() {
            if let Err(error) = self.quickask.borrow_mut().show_context(context, &self.ctx) {
                self.report(&error);
            }
        }
        let device_lost = self.vision.borrow_mut().take_device_lost();
        if device_lost && let Err(error) = self.rebuild_graphics() {
            self.report(&error);
        }
        let events = self
            .vision_gestures
            .as_ref()
            .map(|gestures| gestures.take_events())
            .unwrap_or_default();
        for event in events {
            use orbment_vision::global_mouse::GesturePhase;
            let result = (|| -> Result<()> {
                match event.phase {
                    GesturePhase::DragFinished => {
                        self.vision.borrow_mut().gesture_preview(None)?;
                        let region = orbment_vision::Rect::from_points(event.start, event.current)
                            .validate()?;
                        match event.action.as_str() {
                            "vision.capture_screen" | "vision.pin_last" => {
                                actions.push((
                                    "vision.capture_screen".into(),
                                    json!({"region":region,"pin":true}),
                                ));
                            }
                            "quick_ask.capture" => {
                                actions.push((
                                    "vision.capture_screen".into(),
                                    json!({"region":region}),
                                ));
                                actions.push(("quick_ask.capture".into(), Value::Null));
                            }
                            "vision.ocr" => {
                                actions.push(("vision.ocr_show".into(), json!({"region":region})))
                            }
                            _ => actions.push((event.action, json!({"region":region}))),
                        }
                    }
                    GesturePhase::Click | GesturePhase::DoubleClick | GesturePhase::Wheel => {
                        let action = if event.action == "vision.capture_screen" {
                            "vision.capture"
                        } else {
                            &event.action
                        };
                        actions.push((action.into(), Value::Null));
                    }
                    GesturePhase::Cancelled => self.vision.borrow_mut().gesture_preview(None)?,
                    GesturePhase::DragStarted | GesturePhase::DragUpdated => {
                        if event.show_mask {
                            self.vision.borrow_mut().gesture_preview(Some(
                                orbment_vision::Rect::from_points(event.start, event.current),
                            ))?;
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                self.report(&error);
            }
        }
        let color = self.utilities.borrow_mut().take_color_pin();
        if let Some((rgb, x, y)) = color
            && let Err(error) = self.vision.borrow_mut().pin_color(rgb, x, y)
        {
            self.report(&error);
        }
        for path in self.vision.borrow_mut().take_exported() {
            if let Err(error) = vision::copy_recording_file(self.hwnd, &path) {
                self.report(&error);
            }
            if let Err(error) = self.tray.notify(
                &tr("vision.recording_exported"),
                &path.display().to_string(),
            ) {
                self.report(&error);
            }
        }
        actions
    }
    fn register_hotkeys(&mut self) {
        let previous_conflicts = std::mem::take(&mut self.hotkey_conflicts);
        let config = &self.ctx.config.audio;
        let mut bindings = vec![
            (
                1,
                config.playback_hotkey.clone(),
                if config.quick_menu_enabled {
                    "audio.quick_menu_playback"
                } else {
                    "audio.switch_playback"
                },
                Value::Null,
            ),
            (
                2,
                config.recording_hotkey.clone(),
                if config.quick_menu_enabled {
                    "audio.quick_menu_recording"
                } else {
                    "audio.switch_recording"
                },
                Value::Null,
            ),
            (
                3,
                config.mute_recording_hotkey.clone(),
                "audio.toggle_mic_mute",
                Value::Null,
            ),
        ];
        for (index, profile) in config
            .profiles
            .iter()
            .filter(|profile| profile.enabled)
            .enumerate()
        {
            if let Some(chord) = &profile.hotkey {
                bindings.push((
                    100 + index as i32,
                    chord.clone(),
                    "audio.apply_profile",
                    json!({"id":profile.id}),
                ));
            }
        }
        if self.audio.borrow().service_state() != ServiceState::Running {
            bindings.clear();
        }
        if self.ctx.config.memory.enabled {
            bindings.push((
                10,
                self.ctx
                    .config
                    .memory
                    .clean_hotkey
                    .clone()
                    .unwrap_or_default(),
                "memory.clean",
                Value::Null,
            ));
        }
        if self.ctx.config.utilities.enabled {
            let utilities = &self.ctx.config.utilities;
            for (id, chord, action) in [
                (11, &utilities.topmost_hotkey, "utilities.set_topmost"),
                (12, &utilities.color_picker_hotkey, "utilities.pick_color"),
                (13, &utilities.keep_awake_hotkey, "utilities.keep_awake"),
                (
                    14,
                    &utilities.brightness_hotkey_up,
                    "utilities.brightness_up",
                ),
                (
                    15,
                    &utilities.brightness_hotkey_down,
                    "utilities.brightness_down",
                ),
            ] {
                if let Some(chord) = chord {
                    bindings.push((id, chord.clone(), action, Value::Null));
                }
            }
        }
        if self.ctx.config.clipboard.enabled {
            let clipboard = &self.ctx.config.clipboard;
            bindings.push((
                20,
                clipboard.open_clipboard_hotkey.clone(),
                "clipboard.toggle",
                Value::Null,
            ));
            bindings.push((
                30,
                clipboard.open_preference_hotkey.clone(),
                "clipboard.open_settings",
                Value::Null,
            ));
            if clipboard.quick_paste.enabled {
                for index in 1..=9 {
                    bindings.push((
                        20 + index,
                        format!("{}+{index}", clipboard.quick_paste.prefix),
                        "clipboard.quick_paste",
                        json!({"index":index}),
                    ));
                }
            }
        }
        if self.ctx.config.file_search.enabled
            && !self
                .ctx
                .config
                .file_search
                .hotkey
                .eq_ignore_ascii_case("DoubleCtrl")
        {
            bindings.push((
                40,
                self.ctx.config.file_search.hotkey.clone(),
                "file_search.toggle",
                Value::Null,
            ));
        }
        if self.ctx.config.vision.enabled {
            for (index, (action, chord)) in self.ctx.config.vision.hotkeys.iter().enumerate() {
                if !self.ctx.config.vision.allow_simple_hotkey
                    && !chord.split('+').any(|key| {
                        ["ctrl", "control", "alt", "shift", "win", "windows", "super"]
                            .iter()
                            .any(|modifier| key.trim().eq_ignore_ascii_case(modifier))
                    })
                    && !chord.is_empty()
                {
                    self.hotkey_conflicts
                        .push(format!("{chord}: simple vision hotkeys are disabled"));
                    continue;
                }
                if let Some(resolved) = vision::hotkey_action(action) {
                    bindings.push((500 + index as i32, chord.clone(), resolved, Value::Null));
                } else {
                    self.hotkey_conflicts
                        .push(format!("Unknown vision hotkey action: {action}"));
                }
            }
        }
        if self.ctx.config.quick_ask.enabled
            && let Some(chord) = &self.ctx.config.quick_ask.hotkey
        {
            bindings.push((450, chord.clone(), "quick_ask.open", Value::Null));
        }
        bindings.retain(|(_, chord, _, _)| !chord.is_empty());
        let desired: Vec<_> = bindings
            .iter()
            .map(|(id, chord, _, _)| (*id, chord.as_str()))
            .collect();
        let failures = self.hotkeys.sync(&desired);
        self.hotkey_actions
            .retain(|id, _| desired.iter().any(|(wanted, _)| wanted == id));
        for (id, chord, action, params) in bindings {
            if let Some((_, error)) = failures.iter().find(|(failed, _)| *failed == id) {
                tracing::warn!(id, action, chord, error = %format!("{error:#}"), "Global hotkey registration failed");
                self.hotkey_conflicts
                    .push(hotkey_failure_message(action, &chord, error));
            } else {
                self.hotkey_actions.insert(id, (action.into(), params));
            }
        }
        if !self.hotkey_conflicts.is_empty() && self.hotkey_conflicts != previous_conflicts {
            let _ = self.tray.notify(
                &tr("notification.hotkey_conflicts"),
                &self.hotkey_conflicts.join("\n"),
            );
        }
    }
    fn refresh_memory_tray(&mut self) -> Result<()> {
        let config = &self.ctx.config.memory;
        if !config.enabled || !config.tray_icon.enabled {
            self.memory_tray.take();
            return Ok(());
        }
        if self.memory_tray.is_none() {
            self.memory_tray = Some(Tray::new_with_id(self.hwnd, 3)?);
        }
        if let Some(tray) = &mut self.memory_tray {
            tray.set_memory_icon(self.memory_load, &config.tray_icon)?;
            tray.set_tooltip(
                &tr("memory.tray_tooltip").replace("{percent}", &self.memory_load.to_string()),
            )?;
        }
        Ok(())
    }
    fn prepare_modal(&self) -> Result<()> {
        if self.vision.borrow().has_active_capture() {
            self.vision.borrow_mut().close_active_capture();
        }
        self.search_popup.hide();
        self.clipboard.borrow_mut().close_ui(&self.ctx)
    }
    fn confirm(&self, message: &str) -> bool {
        if let Err(error) = self.prepare_modal() {
            self.report(&error);
            return false;
        }
        let title = wide(&tr("app.name"));
        let message = wide(message);
        unsafe {
            MessageBoxW(
                Some(self.hwnd),
                PCWSTR(message.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_YESNO | MB_ICONWARNING,
            ) == IDYES
        }
    }
    fn clean_memory(&mut self, params: Value) -> Result<Value> {
        ensure!(self.ctx.config.memory.enabled, "{}", tr("memory.disabled"));
        if !single_instance::is_elevated()? {
            if self.confirm(&tr("memory.elevation_prompt")) {
                elevation::restart(&self.ctx.data_dir)?;
                unsafe { PostQuitMessage(0) };
            }
            return Ok(json!({"queued":false,"requires_elevation":true}));
        }
        let config = &self.ctx.config.memory;
        let mask = params
            .get("regions_mask")
            .and_then(Value::as_u64)
            .map(|mask| mask as u32)
            .unwrap_or(config.clean_regions_mask);
        let confirmed = params
            .get("confirmed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !confirmed && config.confirm_before_clean && !self.confirm(&tr("memory.confirm_clean")) {
            return Ok(json!({"queued":false}));
        }
        if !confirmed
            && config.warn_on_standby_modified
            && mask & orbment_analyzer::memory_clean::STALL_RISK_MASK != 0
            && !self.confirm(&tr("memory.confirm_expensive"))
        {
            return Ok(json!({"queued":false}));
        }
        self.memory.borrow_mut().queue_clean(mask)?;
        Ok(json!({"queued":true}))
    }
    fn refresh_audio_tray(&mut self) -> Result<()> {
        if self.audio.borrow().service_state() != ServiceState::Running {
            self.audio_tray.take();
            self.tray.set_icon_path("")?;
            self.tray.set_tooltip(&tr("app.name"))?;
            self.last_playback = None;
            self.last_recording = None;
            return Ok(());
        }
        let snapshot = self.audio.borrow().service()?.snapshot()?;
        let selections_changed = self
            .store
            .save_audio_selections(&snapshot.selected_playback, &snapshot.selected_recording)?;
        if selections_changed {
            self.ctx.config.audio.selected_playback_devices = snapshot.selected_playback.clone();
            self.ctx.config.audio.selected_recording_devices = snapshot.selected_recording.clone();
        }
        if let Some(host) = &self.settings {
            if selections_changed {
                host.sync_config(self.store.get("")?);
            }
            host.set_audio_devices(serde_json::to_value(&snapshot)?);
        }
        let config = &self.ctx.config.audio;
        let playback = snapshot
            .playback
            .iter()
            .find(|device| Some(&device.id) == snapshot.default_playback.as_ref());
        let recording = snapshot
            .recording
            .iter()
            .find(|device| Some(&device.id) == snapshot.default_recording.as_ref());
        let playback_text = playback.map(|device| device.name.as_str()).unwrap_or("");
        let recording_text = recording.map(|device| device.name.as_str()).unwrap_or("");
        let tooltip = match config.tooltip_info.as_str() {
            "Playback" => format!("{}: {playback_text}", tr("audio.playback")),
            "Recording" => format!("{}: {recording_text}", tr("audio.recording")),
            "Both" => format!(
                "{}: {playback_text}\n{}: {recording_text}",
                tr("audio.playback"),
                tr("audio.recording")
            ),
            _ => tr("app.name"),
        };
        let recording_changed = snapshot.default_recording != self.last_recording;
        let playback_changed = snapshot.default_playback != self.last_playback;
        if recording_changed || playback_changed {
            self.audio_icon_recording = recording_changed && !playback_changed;
        }
        let use_recording = config.switch_icon_mode == "Recording"
            || (config.switch_icon_mode == "Both" && self.audio_icon_recording);
        let icon_path = if config.switch_icon_mode == "Never" {
            ""
        } else if use_recording {
            snapshot
                .recording_metadata
                .iter()
                .find(|device| Some(&device.id) == snapshot.default_recording.as_ref())
                .and_then(|device| device.icon_path.as_deref())
                .unwrap_or("")
        } else {
            snapshot
                .playback_metadata
                .iter()
                .find(|device| Some(&device.id) == snapshot.default_playback.as_ref())
                .and_then(|device| device.icon_path.as_deref())
                .unwrap_or("")
        };
        if config.tray_icon.enabled {
            if self.audio_tray.is_none() {
                self.audio_tray = Some(Tray::new_with_id(self.hwnd, 2)?);
            }
            self.tray.set_icon_path("")?;
            self.tray.set_tooltip(&tr("app.name"))?;
            if let Some(tray) = &mut self.audio_tray {
                tray.set_icon_path(icon_path)?;
                tray.set_tooltip(&tooltip)?;
            }
        } else {
            self.audio_tray.take();
            self.tray.set_icon_path(icon_path)?;
            self.tray.set_tooltip(&tooltip)?;
        }
        self.last_playback = snapshot.default_playback;
        self.last_recording = snapshot.default_recording;
        Ok(())
    }
    fn report(&self, error: &anyhow::Error) {
        tracing::warn!("{error:#}");
        let _ = self.tray.notify(&tr("app.error"), &format!("{error:#}"));
    }
    fn status(&self) -> Result<Value> {
        let mut status = diag::collect(
            self.manager.snapshot(),
            self.audio.borrow().hook.references(),
        )?;
        status["database_path"] = json!(self.ctx.data_dir.join("orbment.db"));
        status["config_path"] = json!(self.store.path());
        status["mcp_enabled"] = json!(self.ctx.config.mcp.enabled);
        status["hotkey_conflicts"] = json!(self.hotkey_conflicts);
        status["autostart_enabled"] = json!(self.autostart_enabled);
        status["monitor_unavailable"] = json!(self.monitor_unavailable);
        status["file_tools"] = self.file_tools.diagnostics();
        Ok(status)
    }
    fn sync_settings(&mut self) -> Result<()> {
        if self.settings.is_none() {
            return Ok(());
        }
        let config = self.store.get("")?;
        let devices = match self.audio.borrow().service.as_ref() {
            Some(service) => serde_json::to_value(service.snapshot()?)?,
            None => json!({}),
        };
        let mut diagnostics = self.hotkey_conflicts.clone();
        let monitor_notices = self
            .monitor_unavailable
            .values()
            .map(|key| tr(key))
            .collect();
        diagnostics.push(serde_json::to_string_pretty(&self.status()?)?);
        let pin_history = if self.vision.borrow().service_state() == ServiceState::Running {
            serde_json::to_value(self.vision.borrow_mut().service()?.pin_history()?)?
        } else {
            json!([])
        };
        if let Some(host) = &mut self.settings {
            host.sync_config(config);
            host.set_audio_devices(devices);
            host.set_pin_history(pin_history);
            host.set_diagnostics(diagnostics);
            host.set_monitor_notices(monitor_notices);
        }
        Ok(())
    }
    fn sync_pin_history(&mut self) -> Result<()> {
        if let Some(host) = &self.settings {
            let history = if self.vision.borrow().service_state() == ServiceState::Running {
                serde_json::to_value(self.vision.borrow_mut().service()?.pin_history()?)?
            } else {
                json!([])
            };
            host.set_pin_history(history);
        }
        Ok(())
    }
    fn show_settings(&mut self) -> Result<()> {
        if self.settings.as_ref().is_some_and(|host| !host.is_alive()) {
            self.settings.take();
            if let Err(error) = self.file_tools.action(FileToolsAction::DiskClosed) {
                self.report(&error);
            }
        }
        if self.settings.is_none() {
            self.settings = Some(MainHostWindow::new(
                self.graphics.clone(),
                self.store.get("")?,
                self.hwnd,
            )?);
            if let Some(host) = &self.settings {
                host.set_file_tools_state(self.file_tools.state.clone());
            }
        }
        self.sync_settings()?;
        if let Some(host) = &self.settings {
            host.show();
        }
        let events = self.events.clone();
        let hwnd = self.hwnd.0 as usize;
        self.request_history(Value::Null, move |result| {
            send_event(&events, hwnd, Event::MonitorHistory(result));
        });
        Ok(())
    }
    fn sync_pipe(&mut self) -> Result<()> {
        if self.ctx.config.mcp.enabled && self.pipe.is_none() {
            let requests = self.requests.clone();
            self.pipe = Some(PipeServer::start(move |request| requests.call(request))?);
        } else if !self.ctx.config.mcp.enabled {
            self.pipe.take();
        }
        Ok(())
    }
    fn apply_config(&mut self, path: &str) -> Result<()> {
        self.ctx.config = self.store.config.clone();
        let mut errors = Vec::new();
        let mut hotkeys_dirty = false;
        if (path.is_empty() || path == "app" || path == "app.autostart")
            && self.autostart_enabled != self.ctx.config.app.autostart
        {
            match autostart::set_enabled(self.ctx.config.app.autostart, &self.ctx.data_dir) {
                Ok(()) => self.autostart_enabled = self.ctx.config.app.autostart,
                Err(error) => errors.push(error),
            }
        }
        if (path.is_empty() || path == "app" || path == "app.log_level")
            && let Err(error) = logging::set_level(&self.ctx.config.app.log_level)
        {
            errors.push(error);
        }
        if (path.is_empty() || path == "app" || path == "app.log_retention_days")
            && let Err(error) = logging::cleanup_old_logs(
                &self.ctx.data_dir,
                self.ctx.config.app.log_retention_days,
            )
        {
            errors.push(error);
        }
        if path.is_empty()
            || path == "vision"
            || path.starts_with("vision.")
            || path == "app"
            || path.starts_with("app.ocr_")
        {
            if let Err(error) = self.vision.borrow_mut().apply_config(&self.ctx) {
                errors.push(error);
            }
            if let Err(error) = self.sync_vision_hook() {
                errors.push(error);
            }
            hotkeys_dirty = true;
        }
        if path.is_empty() || path == "quick_ask" || path.starts_with("quick_ask.") {
            if let Err(error) = self.quickask.borrow_mut().start_service(&self.ctx) {
                errors.push(error);
            }
            hotkeys_dirty = true;
        }
        if path.is_empty() || path == "audio" || path.starts_with("audio.") {
            if let Err(error) = self.audio.borrow_mut().apply_config(&self.ctx) {
                errors.push(error);
            }
            hotkeys_dirty = true;
            if let Err(error) = self.refresh_audio_tray() {
                errors.push(error);
            }
        }
        if path.is_empty() || path == "monitor" || path.starts_with("monitor.") {
            self.monitor_unavailable.clear();
            if let Err(error) = self.monitor.borrow_mut().apply_config(&self.ctx) {
                errors.push(error);
            }
        }
        if path.is_empty() || path == "memory" || path.starts_with("memory.") {
            if let Err(error) = self.memory.borrow_mut().apply_config(&self.ctx) {
                errors.push(error);
            }
            if let Err(error) = self.refresh_memory_tray() {
                errors.push(error);
            }
            hotkeys_dirty = true;
        }
        if path.is_empty() || path == "utilities" || path.starts_with("utilities.") {
            if let Err(error) = self.utilities.borrow_mut().apply_config(&self.ctx) {
                errors.push(error);
            }
            hotkeys_dirty = true;
        }
        if path.is_empty()
            || path == "clipboard"
            || path.starts_with("clipboard.")
            || path == "app"
            || path.starts_with("app.ocr_")
            || path == "vision"
            || path.starts_with("vision.")
        {
            if let Err(error) = self.clipboard.borrow_mut().apply_config(&self.ctx) {
                errors.push(error);
            }
            if let Err(error) = self.sync_clipboard_hook() {
                errors.push(error);
            }
            hotkeys_dirty = true;
        }
        if path.is_empty()
            || path == "file_search"
            || path.starts_with("file_search.")
            || path == "disk_analyzer"
            || path.starts_with("disk_analyzer.")
        {
            if !self.ctx.config.file_search.enabled {
                self.search_popup.hide();
            }
            if let Err(error) = self.file_tools.configure(
                self.ctx.config.file_search.clone(),
                self.ctx.config.disk_analyzer.clone(),
            ) {
                errors.push(error);
            }
            if let Err(error) = self.sync_search_hook() {
                errors.push(error);
            }
            hotkeys_dirty = true;
        }
        if hotkeys_dirty {
            self.register_hotkeys();
        }
        if let Err(error) = self.sync_pipe() {
            errors.push(error);
        }
        if let Err(error) = self.sync_settings() {
            errors.push(error);
        }
        if !errors.is_empty() {
            bail!(
                "{}",
                errors
                    .iter()
                    .map(|error| format!("{error:#}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        Ok(())
    }
    fn set_config(&mut self, path: &str, value: Value, external: bool) -> Result<Value> {
        if external {
            self.store.set_external(path, value)?;
        } else {
            self.store.set_json(path, value)?;
        }
        self.apply_config(path)?;
        self.store.get(path)
    }
    fn request_history(
        &self,
        params: Value,
        complete: impl FnOnce(Result<Value>) + Send + 'static,
    ) {
        let request = match parameters::<TrafficHistory>(params) {
            Ok(request) => request,
            Err(error) => {
                complete(Err(error));
                return;
            }
        };
        if let Some(service) = &self.monitor.borrow().service {
            service.traffic_history_async(request.start_date, request.end_date, move |result| {
                complete(result.and_then(|rows| serde_json::to_value(rows).map_err(Into::into)));
            });
        } else {
            let data_dir = self.ctx.data_dir.clone();
            std::thread::spawn(move || {
                let result = (|| -> Result<Value> {
                    let database = db::open(&data_dir)?;
                    Ok(serde_json::to_value(orbment_monitor::history::query(
                        &database,
                        request.start_date.as_deref(),
                        request.end_date.as_deref(),
                    )?)?)
                })();
                complete(result);
            });
        }
    }

    fn dispatch_request(&mut self, pending: PendingRequest) {
        if pending.is_cancelled() {
            return;
        }
        let action = pending.request.action.as_str();
        if file_tools::is_request(action) {
            self.file_tools.request(pending);
        } else if action == "vision.ocr" {
            let job = (|| -> Result<vision::OcrJob> {
                if pending.request.params.get("engine").is_none() {
                    self.choose_ocr_engine()?;
                }
                self.vision
                    .borrow_mut()
                    .prepare_ocr(pending.request.params.clone(), &self.ctx)
            })();
            match job {
                Ok(job) => self.vision.borrow_mut().request_ocr(job, pending),
                Err(error) => pending.reply(Err(error)),
            }
        } else if action == "vision.capture_screen" {
            let job = self
                .vision
                .borrow_mut()
                .prepare_capture(pending.request.params.clone(), &self.ctx);
            match job {
                Ok(job) => self.vision.borrow_mut().request_capture(job, pending),
                Err(error) => pending.reply(Err(error)),
            }
        } else if matches!(
            action,
            "memory.clean"
                | "memory.overview"
                | "memory.port_lookup"
                | "memory.process_kill"
                | "memory.process_reap_preview"
                | "memory.process_reap"
        ) {
            let config = self.ctx.config.memory.clone();
            std::thread::spawn(move || {
                if !pending.is_cancelled() {
                    let result = memory::execute(
                        &config,
                        &pending.request.action,
                        pending.request.params.clone(),
                    );
                    pending.reply(result);
                }
            });
        } else if matches!(
            action,
            "clipboard.write" | "clipboard.paste" | "clipboard.quick_paste"
        ) {
            self.clipboard.borrow_mut().request(pending, &self.ctx);
        } else if action == "monitor.traffic_history" {
            self.request_history(pending.request.params.clone(), move |result| {
                pending.reply(result)
            });
        } else {
            let result = self.command(&pending.request.action, pending.request.params.clone());
            pending.reply(result);
        }
    }

    fn queue_action(&mut self, action: &str, params: Value) -> Value {
        let events = self.events.clone();
        let hwnd = self.hwnd.0 as usize;
        self.dispatch_request(PendingRequest::new(
            ipc::ActionRequest::new(action, params),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            move |result| send_event(&events, hwnd, Event::ActionCompleted(result)),
        ));
        json!({"queued": true})
    }

    fn external_allowed(&self, action: &str) -> Result<()> {
        orbment_mcp::check_external_action(&self.ctx.config.mcp, action)
    }
    fn menu(&self) -> Vec<MenuItem> {
        let mut items = Vec::new();
        let mut add = |key: &str, action: &str, checked: bool| {
            items.push(MenuItem {
                label: if action.is_empty() {
                    "-".into()
                } else {
                    tr(key)
                },
                action: action.into(),
                checked,
            });
        };
        add("menu.config", "app.open_config", false);
        if self.ctx.config.clipboard.enabled
            || self.ctx.config.vision.enabled
            || self.ctx.config.file_search.enabled
            || self.ctx.config.quick_ask.enabled
        {
            add("-", "", false);
        }
        if self.ctx.config.clipboard.enabled {
            add("menu.clipboard", "clipboard.toggle", false);
        }
        if self.ctx.config.vision.enabled {
            add("vision.capture", "vision.capture", false);
            add("vision.open_history", "vision.open_history", false);
        }
        if self.ctx.config.file_search.enabled {
            add("file_tools.search", "file_search.toggle", false);
        }
        if self.ctx.config.quick_ask.enabled {
            add("quickask.open", "quick_ask.open", false);
        }
        if self.ctx.config.audio.enabled
            || self.ctx.config.memory.enabled
            || self.ctx.config.utilities.enabled
        {
            add("-", "", false);
        }
        if self.ctx.config.audio.enabled {
            add("menu.switch_playback", "audio.switch_playback", false);
        }
        if self.ctx.config.memory.enabled {
            add("menu.memory_clean", "memory.clean", false);
        }
        if self.ctx.config.utilities.enabled {
            add(
                "menu.keep_awake",
                "utilities.keep_awake",
                self.utilities.borrow().is_awake(),
            );
        }
        add("-", "", false);
        add("menu.exit", "app.exit", false);
        items
    }
    fn command(&mut self, action: &str, params: Value) -> Result<Value> {
        match action {
            "vision.open_history" => {
                self.show_settings()?;
                if let Some(host) = &self.settings {
                    host.select_page("vision_pins");
                }
                return Ok(json!({"ok":true}));
            }
            "vision.pin_action" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct PinAction {
                    id: String,
                    action: String,
                }
                let request: PinAction = parameters(params)?;
                let action = match request.action.as_str() {
                    "copy_image" => "vision.copy_image",
                    "save_image" => "vision.save_image",
                    action => action,
                };
                ensure!(
                    action != "vision.pin_action"
                        && (vision::ACTIONS.contains(&action) || action == "quick_ask.capture"),
                    "Unknown pin action: {}",
                    request.action
                );
                self.vision.borrow_mut().activate_pin(&request.id)?;
                return self.command(action, Value::Null);
            }
            "vision.ocr" => return Ok(self.queue_action(action, params)),
            "vision.pin_history" => {
                let history = self
                    .vision
                    .borrow_mut()
                    .command(action, params, &self.ctx)?;
                if let Some(host) = &self.settings {
                    host.set_pin_history(history.clone());
                }
                return Ok(history);
            }
            action if vision::ACTIONS.contains(&action) => {
                if matches!(action, "vision.pin_select_text" | "vision.ocr_show") {
                    self.choose_ocr_engine()?;
                }
                let result = self.vision.borrow_mut().command(action, params, &self.ctx);
                self.vision
                    .borrow_mut()
                    .sync_recording_hooks(&mut self.audio.borrow_mut().hook, &self.ctx)?;
                self.sync_pin_history()?;
                return result;
            }
            "quick_ask.open" => {
                self.quickask
                    .borrow_mut()
                    .open_ui(ActivateTrigger::Action, &self.ctx)?;
            }
            "quick_ask.capture" => {
                self.vision.borrow_mut().request_quick_context(params)?;
            }
            "app.clear_traces" => {
                ensure!(
                    self.confirm(&tr("app.clear_traces_confirm")),
                    "{}",
                    tr("app.cancelled")
                );
                self.vision
                    .borrow_mut()
                    .release_recording_hooks(&mut self.audio.borrow_mut().hook)?;
                self.vision.borrow_mut().clear_traces(&self.ctx)?;
                self.database.execute("DELETE FROM pin_records", [])?;
                self.quickask.borrow_mut().clear_credentials(&self.ctx)?;
                autostart::set_enabled(false, &self.ctx.data_dir)?;
                self.autostart_enabled = false;
                self.store.set_json("app.autostart", json!(false))?;
                self.store
                    .set_json("vision.shell_context_menu", json!(false))?;
                self.apply_config("vision")?;
                return Ok(json!({"ok":true}));
            }
            "file_search.toggle" => {
                ensure!(
                    self.ctx.config.file_search.enabled,
                    "{}",
                    tr("file_tools.error.disabled")
                );
                if self.search_popup.is_visible() {
                    self.search_popup.hide();
                } else {
                    self.search_popup.show(&self.ctx.config.file_search)?;
                }
            }
            "disk_analyzer.open" | "disk_analyzer.open_locks" => {
                ensure!(
                    self.ctx.config.disk_analyzer.enabled,
                    "{}",
                    tr("file_tools.error.disabled")
                );
                self.show_settings()?;
                if let Some(host) = &self.settings {
                    host.select_page(if action == "disk_analyzer.open" {
                        "disk_analysis"
                    } else {
                        "file_unlock"
                    });
                }
            }
            action if file_tools::is_request(action) => {
                return Ok(self.queue_action(action, params));
            }
            "app.status" => return self.status(),
            "app.config_get" => return self.store.get(&parameters::<GetConfig>(params)?.path),
            "app.config_set" => {
                let request: SetConfig = parameters(params)?;
                return self.set_config(&request.path, request.value, true);
            }
            "clipboard.write" | "clipboard.paste" | "clipboard.quick_paste" => {
                return Ok(self.queue_action(action, params));
            }
            "clipboard.toggle" | "clipboard.open" => {
                return self
                    .clipboard
                    .borrow_mut()
                    .command(action, params, &self.ctx);
            }
            "memory.clean" => return self.clean_memory(params),
            "memory.toggle_window" => {
                if self
                    .settings
                    .as_ref()
                    .is_some_and(MainHostWindow::memory_ui_visible)
                {
                    self.settings.take();
                    self.file_tools.action(FileToolsAction::DiskClosed)?;
                } else {
                    return self.command("memory.open_analyzer", Value::Null);
                }
            }
            "app.open_task_manager" => {
                std::process::Command::new("taskmgr.exe").spawn()?;
            }
            "memory.overview"
            | "memory.port_lookup"
            | "memory.process_kill"
            | "memory.process_reap_preview"
            | "memory.process_reap" => {
                return Ok(self.queue_action(action, params));
            }
            "memory.open_analyzer"
            | "memory.open_reaper"
            | "memory.open_ports"
            | "utilities.open_status"
            | "clipboard.open_settings" => {
                self.show_settings()?;
                if let Some(host) = &self.settings {
                    host.select_page(match action {
                        "memory.open_reaper" => "memory_reaper",
                        "memory.open_ports" => "memory_ports",
                        "utilities.open_status" => "utilities_status",
                        "clipboard.open_settings" => "clipboard",
                        _ => "memory_analyzer",
                    });
                }
            }
            "memory.elevate" => {
                elevation::restart(&self.ctx.data_dir)?;
                unsafe { PostQuitMessage(0) };
            }
            "utilities.set_topmost"
            | "utilities.pick_color"
            | "utilities.keep_awake"
            | "utilities.display_brightness"
            | "utilities.brightness_up"
            | "utilities.brightness_down" => {
                return self
                    .utilities
                    .borrow_mut()
                    .command(action, params, &self.ctx);
            }
            "audio.list_devices" => {
                return Ok(serde_json::to_value(
                    self.audio.borrow().service()?.snapshot()?,
                )?);
            }
            "audio.quick_menu_playback" | "audio.quick_menu_recording" => {
                self.audio
                    .borrow()
                    .service()?
                    .request_quick_menu(action == "audio.quick_menu_recording")?;
            }
            "audio.switch_playback" | "audio.switch_recording" => {
                let request: SwitchDevice = parameters(params)?;
                let audio = self.audio.borrow();
                let service = audio.service()?;
                match (action, request.device_id) {
                    ("audio.switch_playback", Some(id)) => service.select_playback(id)?,
                    ("audio.switch_playback", None) => service.switch_playback()?,
                    (_, Some(id)) => service.select_recording(id)?,
                    (_, None) => service.switch_recording()?,
                }
                return Ok(serde_json::to_value(service.snapshot()?)?);
            }
            "audio.toggle_mic_mute" => {
                let audio = self.audio.borrow();
                audio.service()?.toggle_mic_mute()?;
                return Ok(serde_json::to_value(audio.service()?.snapshot()?)?);
            }
            "audio.route_process" => {
                let request: RouteProcess = parameters(params)?;
                ensure!(
                    usize::from(request.pid.is_some())
                        + usize::from(request.process_path.is_some())
                        + usize::from(request.process_name.is_some())
                        <= 1,
                    "pid, process_path and process_name are mutually exclusive"
                );
                self.audio.borrow().service()?.route_process(
                    request.pid,
                    request.process_path,
                    request.process_name,
                    request.device_id,
                    request.recording,
                )?;
            }
            "audio.apply_profile" => {
                self.audio
                    .borrow()
                    .service()?
                    .apply_profile(parameters::<Profile>(params)?.id)?;
            }
            "monitor.snapshot" => {
                return Ok(serde_json::to_value(
                    self.monitor.borrow().service()?.snapshot(),
                )?);
            }
            "monitor.traffic_history" => {
                let events = self.events.clone();
                let hwnd = self.hwnd.0 as usize;
                self.request_history(params, move |result| {
                    send_event(&events, hwnd, Event::MonitorHistory(result));
                });
                return Ok(json!({"queued":true}));
            }
            "audio.toggle_knob_remap" => {
                self.set_config(
                    "audio.knob_remap.intercept_volume_mute",
                    json!(!self.ctx.config.audio.knob_remap.intercept_volume_mute),
                    false,
                )?;
            }
            "monitor.toggle_taskbar" => {
                self.set_config(
                    "monitor.taskbar.enabled",
                    json!(!self.ctx.config.monitor.taskbar.enabled),
                    false,
                )?;
            }
            "monitor.toggle_floating" => {
                self.set_config(
                    "monitor.floating.enabled",
                    json!(!self.ctx.config.monitor.floating.enabled),
                    false,
                )?;
            }
            "monitor.toggle_more_info" => {
                self.set_config(
                    "monitor.floating.show_cpu_memory",
                    json!(!self.ctx.config.monitor.floating.show_cpu_memory),
                    false,
                )?;
            }
            "monitor.open_history" => {
                self.show_settings()?;
                if let Some(host) = &self.settings {
                    host.select_page("history");
                }
            }
            "monitor.open_settings" | "monitor.open_skins" => {
                self.show_settings()?;
                if let Some(host) = &self.settings {
                    host.select_page("monitor");
                }
            }
            "app.open_config" => self.show_settings()?,
            "app.about" => {
                self.show_settings()?;
                if let Some(host) = &self.settings {
                    host.select_page("about");
                }
            }
            "app.reload_config" => {
                self.store.reload()?;
                self.apply_config("")?;
            }
            "app.export_diag" => {
                let path = self.ctx.data_dir.join("diagnostics.json");
                atomic_write(&path, &serde_json::to_vec_pretty(&self.status()?)?)?;
                self.tray.notify(
                    &tr("notification.diag_exported"),
                    &path.display().to_string(),
                )?;
                return Ok(json!({"path":path}));
            }
            "app.toggle_autostart" => {
                self.set_config("app.autostart", json!(!self.autostart_enabled), false)?;
            }
            "app.exit" => unsafe {
                PostQuitMessage(0);
            },
            _ => bail!("unknown action: {action}"),
        }
        Ok(json!({"ok":true}))
    }
    fn event(&mut self, event: Event) -> Result<Option<Action>> {
        match event {
            Event::MonitorHistory(result) => {
                let history = result?;
                if let Some(host) = &self.settings {
                    host.set_monitor_history(history);
                }
            }
            Event::ActionCompleted(result) => {
                result?;
            }
            Event::Memory(event) => match event {
                MemoryEvent::Usage { load_pct } => {
                    self.memory_load = load_pct;
                    self.refresh_memory_tray()?;
                }
                MemoryEvent::Cleaned { report, .. } => {
                    if self.ctx.config.memory.show_clean_result_notification {
                        let mut message = tr("memory.clean_result").replace(
                            "{mb}",
                            &format!("{:.1}", report.released_bytes as f64 / 1_048_576.0),
                        );
                        let errors: Vec<_> = report
                            .regions
                            .iter()
                            .filter_map(|region| region.error.as_deref())
                            .collect();
                        if !errors.is_empty() {
                            message.push('\n');
                            message.push_str(&errors.join("\n"));
                        }
                        self.tray.notify_with_sound(
                            &tr("app.name"),
                            &message,
                            self.ctx.config.memory.notification_sound,
                        )?;
                    }
                }
                MemoryEvent::Failed { message, automatic } => {
                    if automatic {
                        tracing::warn!(%message, "Automatic memory cleanup failed");
                    } else {
                        self.tray.notify(&tr("app.error"), &message)?;
                    }
                }
            },
            Event::Audio(event) if self.ctx.config.audio.enabled => match event {
                AudioEvent::Notification(notification) => {
                    let title = tr(notification.title_key);
                    match notification.mode.as_str() {
                        "Banner" => self.audio.borrow_mut().osd.show(
                            &self.ctx.config.audio,
                            &format!("{title} {}", notification.message),
                        )?,
                        "SystemToast" => self.tray.notify(&title, &notification.message)?,
                        "Sound" => orbment_audio::osd::play_notification_sound()?,
                        _ => {}
                    }
                }
                AudioEvent::MicrophoneState { muted, badge_mode } => {
                    self.audio.borrow_mut().osd.set_microphone_state(
                        &self.ctx.config.audio,
                        muted,
                        &badge_mode,
                    )?;
                }
                AudioEvent::QuickMenu { recording, devices } => {
                    let menu = devices
                        .iter()
                        .map(|device| MenuItem {
                            label: device.name.clone(),
                            action: device.id.clone(),
                            checked: false,
                        })
                        .collect::<Vec<_>>();
                    match self.tray.show_menu(&menu) {
                        Ok(Some(id)) => {
                            return Ok(Some((
                                if recording {
                                    "audio.switch_recording"
                                } else {
                                    "audio.switch_playback"
                                }
                                .into(),
                                json!({"device_id":id}),
                            )));
                        }
                        result => {
                            self.audio.borrow().service()?.cancel_quick_menu()?;
                            result?;
                        }
                    }
                }
                AudioEvent::Switched(_) => {}
                AudioEvent::DevicesChanged => self.refresh_audio_tray()?,
                AudioEvent::Error(error) => self.tray.notify(&tr("app.error"), &error)?,
            },
            Event::Audio(_) => {}
            Event::Monitor(update) => {
                if self.ctx.config.monitor.enabled {
                    if self.monitor_unavailable != update.snapshot.sensors.unavailable {
                        self.monitor_unavailable = update.snapshot.sensors.unavailable.clone();
                        self.sync_settings()?;
                    }
                    for alert in &update.alerts {
                        let message = tr(alert.key)
                            .replace("{value}", &format!("{:.1}", alert.value))
                            .replace("{threshold}", &format!("{:.1}", alert.threshold));
                        self.tray.notify(&tr("monitor.alert"), &message)?;
                    }
                    self.monitor.borrow_mut().update(*update)?;
                }
            }
            Event::MonitorPosition(x, y) => {
                let mut config = self.ctx.config.monitor.floating.clone();
                config.pos_x = x;
                config.pos_y = y;
                self.store
                    .set_json("monitor.floating", serde_json::to_value(config)?)?;
                self.ctx.config = self.store.config.clone();
                self.sync_settings()?;
            }
            Event::MonitorAction(action) => {
                let name = match action {
                    1 => "monitor.open_history",
                    2 => "monitor.toggle_more_info",
                    3 => "monitor.open_settings",
                    6 => "monitor.open_skins",
                    _ => return Ok(None),
                };
                return Ok(Some((name.into(), Value::Null)));
            }
        }
        Ok(None)
    }
    fn settings_events(&mut self) {
        let events = self
            .settings
            .as_ref()
            .map(|host| host.take_events())
            .unwrap_or_default();
        let events = events.into_iter().map(|event| (true, event)).chain(
            self.search_popup
                .take_events()
                .into_iter()
                .map(|event| (false, event)),
        );
        for (from_settings, event) in events {
            let result = (|| -> Result<()> {
                match event {
                    SettingsEvent::FileTools(action) => self.file_tools_action(action)?,
                    SettingsEvent::QuickAskModels {
                        config,
                        api_key_override,
                        result,
                    } => {
                        let spawn_error = result.clone();
                        if let Err(error) = std::thread::Builder::new()
                            .name("quickask-models".into())
                            .spawn(move || {
                                let models = orbment_quickask::llm_client::fetch_models(
                                    &config,
                                    api_key_override.as_deref(),
                                )
                                .map_err(|error| error.to_string());
                                let _ = result.send(models);
                            })
                        {
                            let _ = spawn_error.send(Err(error.to_string()));
                        }
                    }
                    SettingsEvent::Action { action, params } => {
                        self.command(&action, params)?;
                    }
                    SettingsEvent::ConfigChanged { path, value } => {
                        if let Err(error) = self.set_config(&path, value, false) {
                            self.report(&error);
                            self.sync_settings()?;
                        }
                    }
                    SettingsEvent::Closed if from_settings => {
                        self.settings.take();
                        self.file_tools.action(FileToolsAction::DiskClosed)?;
                    }
                    SettingsEvent::Closed => {}
                    SettingsEvent::DeviceLost => self.rebuild_graphics()?,
                    SettingsEvent::FilesDropped(paths) => {
                        self.tray.notify(
                            &tr("app.name"),
                            &tr("settings.drop_not_supported")
                                .replace("{count}", &paths.len().to_string()),
                        )?;
                    }
                    SettingsEvent::Error(error) => self.tray.notify(&tr("app.error"), &error)?,
                }
                Ok(())
            })();
            if let Err(error) = result {
                self.report(&error);
            }
        }
    }
    fn file_tools_action(&mut self, action: FileToolsAction) -> Result<()> {
        match &action {
            FileToolsAction::Ports { port } => return self.open_file_tools_ports(*port),
            FileToolsAction::Search { query } => {
                if let Some(port) = query
                    .trim()
                    .strip_prefix(":ports ")
                    .and_then(|value| value.trim().parse::<u16>().ok())
                {
                    return self.open_file_tools_ports(port);
                }
            }
            FileToolsAction::CloseHandle { lock } => {
                ensure!(
                    lock.handle.is_some(),
                    "{}",
                    tr("file_tools.error.unavailable")
                );
            }
            _ => {}
        }
        let opened = matches!(&action, FileToolsAction::Open { .. });
        self.file_tools.action(action)?;
        if opened {
            self.search_popup.hide();
        }
        Ok(())
    }

    fn open_file_tools_ports(&mut self, port: u16) -> Result<()> {
        ensure!(self.ctx.config.memory.enabled, "{}", tr("memory.disabled"));
        self.search_popup.hide();
        self.show_settings()?;
        if let Some(host) = &self.settings {
            host.show_ports(port);
        }
        Ok(())
    }
    fn rebuild_graphics(&mut self) -> Result<()> {
        let graphics = Rc::new(D2dContext::new()?);
        self.audio
            .borrow_mut()
            .rebuild_graphics(graphics.clone(), &self.ctx)?;
        self.monitor
            .borrow_mut()
            .rebuild_graphics(graphics.clone(), &self.ctx)?;
        self.utilities
            .borrow_mut()
            .rebuild_graphics(graphics.clone(), &self.ctx)?;
        self.clipboard
            .borrow_mut()
            .rebuild_graphics(graphics.clone(), &self.ctx)?;
        if let Some(host) = &mut self.settings {
            host.rebuild_graphics(graphics.clone())?;
        }
        self.search_popup.rebuild_graphics(graphics.clone())?;
        self.vision
            .borrow_mut()
            .rebuild_graphics(graphics.clone())?;
        self.quickask
            .borrow_mut()
            .rebuild_graphics(graphics.clone())?;
        self.graphics = graphics;
        Ok(())
    }
    fn maintenance(&mut self) -> Result<()> {
        self.audio.borrow_mut().osd.retain_alive();
        let mut failure = None;
        for result in [
            self.utilities.borrow_mut().maintenance(),
            self.memory.borrow_mut().maintenance(),
            self.clipboard.borrow_mut().maintenance(&self.ctx),
            self.file_tools.maintenance(),
        ] {
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        self.search_popup
            .tick(u64::from(self.ctx.config.app.idle_destroy_seconds));
        if let Some(host) = &self.settings {
            host.refresh_file_tools();
        }
        let clipboard_device_lost = self.clipboard.borrow_mut().take_device_lost();
        let quickask_device_lost = self.quickask.borrow_mut().take_device_lost();
        if (clipboard_device_lost || quickask_device_lost)
            && let Err(error) = self.rebuild_graphics()
        {
            failure.get_or_insert(error);
        }
        let needs_ocr_choice = self.clipboard.borrow_mut().needs_ocr_choice();
        if needs_ocr_choice {
            match self.choose_ocr_engine() {
                Ok(()) => {
                    if let Err(error) = self.clipboard.borrow_mut().resume_ocr_requests() {
                        failure.get_or_insert(error);
                    }
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        self.memory.borrow_mut().set_ui_visible(
            self.settings
                .as_ref()
                .is_some_and(MainHostWindow::memory_ui_visible),
        );
        if let Some(host) = &self.settings {
            host.set_utilities_status(self.utilities.borrow().status());
        }
        if self.last_checkpoint.elapsed() >= Duration::from_secs(60) {
            if let Err(error) = db::checkpoint(&self.database) {
                failure.get_or_insert(error);
            }
            self.last_checkpoint = Instant::now();
        }
        if self.last_log_cleanup.elapsed() >= Duration::from_secs(86_400) {
            if let Err(error) = logging::cleanup_old_logs(
                &self.ctx.data_dir,
                self.ctx.config.app.log_retention_days,
            ) {
                failure.get_or_insert(error);
            }
            self.last_log_cleanup = Instant::now();
        }
        failure.map_or(Ok(()), Err)
    }
}

fn hotkey_failure_message(action: &str, chord: &str, error: &anyhow::Error) -> String {
    let label = match action {
        "clipboard.toggle" => "剪贴板历史",
        "clipboard.open_settings" => "剪贴板设置",
        "clipboard.quick_paste" => "数字快捷粘贴",
        "vision.capture" | "vision.capture_screen" => "区域截图",
        "vision.pin_last" => "贴图",
        "vision.ocr_capture" | "vision.ocr" => "文字识别",
        "vision.restore_last" => "恢复上一张关闭的贴图",
        "vision.longshot" => "滚动长截图",
        "vision.record_start" => "开始录屏",
        "vision.record_stop" => "停止录屏",
        "vision.record_edit" => "录屏编辑",
        "vision.capture_for_quickask" => "截图提问",
        "audio.switch_playback" | "audio.quick_menu_playback" => "切换播放设备",
        "audio.switch_recording" | "audio.quick_menu_recording" => "切换录音设备",
        "audio.toggle_mic_mute" => "切换麦克风静音",
        "audio.apply_profile" => "音频配置方案",
        "memory.clean" => "清理内存",
        "utilities.set_topmost" => "窗口置顶",
        "utilities.pick_color" => "屏幕取色",
        "utilities.keep_awake" => "保持唤醒",
        "utilities.brightness_up" => "提高屏幕亮度",
        "utilities.brightness_down" => "降低屏幕亮度",
        "file_search.toggle" => "文件搜索",
        "quick_ask.open" => "快速提问",
        _ => "自定义操作",
    };
    let occupied = error
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|error| {
            error.code()
                == windows::core::HRESULT::from_win32(
                    windows::Win32::Foundation::ERROR_HOTKEY_ALREADY_REGISTERED.0,
                )
        });
    let reason = if occupied {
        "已被其他程序占用"
    } else {
        "注册失败，请检查按键格式、重复绑定或其他程序占用情况"
    };
    format!("「{label}」快捷键 {chord} {reason}")
}

fn tray_left_action(
    message: u32,
    audio_tray: bool,
    memory_tray: bool,
    audio: &orbment_core::config::AudioConfig,
) -> &'static str {
    if memory_tray {
        "memory.open_analyzer"
    } else if audio_tray
        && message == WM_LBUTTONDBLCLK
        && audio.enabled
        && audio.icon_double_click == "SwitchDevice"
    {
        "audio.switch_playback"
    } else {
        "app.open_config"
    }
}

fn main() {
    if let Some(result) = client::run_if_requested() {
        if let Err(error) = result {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run() {
        show_error(&tr("app.startup_error"), &error);
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let started = Instant::now();
    let options = Options::parse()?;
    if let Some(out_dir) = &options.render_ui_previews {
        let exe = std::env::current_exe()?;
        i18n::load(
            &exe.parent()
                .context("executable parent")?
                .join("assets/locales/zh-CN.toml"),
        )?;
        return preview::render(out_dir);
    }
    if let Some(pid) = options.restart_from {
        elevation::wait_for_parent(pid)?;
    }
    let Some(_instance) = single_instance::start()? else {
        if let Some(path) = &options.pin_image {
            orbment_ui::send_pin_image(&std::path::absolute(path)?)?;
        }
        return Ok(());
    };
    let exe = std::env::current_exe()?;
    let exe_dir = exe.parent().context("executable parent")?;
    let environment = std::env::var_os("ORBMENT_DATA_DIR").map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let data_dir = std::path::absolute(resolve_data_dir(
        exe_dir,
        options.data_dir.as_deref(),
        environment.as_deref(),
        appdata.as_deref(),
    )?)?;
    i18n::load(&exe_dir.join("assets/locales/zh-CN.toml"))?;
    let store = ConfigStore::load(&data_dir)?;
    let _logging = logging::init(
        &data_dir,
        &store.config.app.log_level,
        store.config.app.log_retention_days,
    )?;
    let database = db::open(&data_dir)?;
    let event_loop = EventLoop::new()?;
    let hwnd = event_loop.hwnd();
    let graphics = Rc::new(D2dContext::new()?);
    let (sender, receiver) = mpsc::channel();
    let wake_hwnd = hwnd.0 as usize;
    let (requests, pending_requests) = ipc::request_channel(move || {
        wake(HWND(wake_hwnd as *mut std::ffi::c_void));
    });
    let audio = Rc::new(RefCell::new(AudioModule::new(
        graphics.clone(),
        hwnd,
        sender.clone(),
    )));
    let monitor = Rc::new(RefCell::new(MonitorModule::new(
        graphics.clone(),
        hwnd,
        sender.clone(),
    )));
    let memory_events = sender.clone();
    let memory = Rc::new(RefCell::new(MemoryModule::new(move |event| {
        send_event(&memory_events, wake_hwnd, Event::Memory(event));
    })));
    let utilities = Rc::new(RefCell::new(UtilitiesModule::new(graphics.clone(), hwnd)));
    let clipboard = Rc::new(RefCell::new(ClipboardModule::new(
        graphics.clone(),
        hwnd,
        started,
    )));
    let file_tools = FileTools::new(
        data_dir.clone(),
        store.config.file_search.clone(),
        store.config.disk_analyzer.clone(),
        hwnd,
    )?;
    let search_popup =
        SearchPopupWindow::new(graphics.clone(), hwnd, file_tools.state.clone(), started);
    let vision = Rc::new(RefCell::new(vision::VisionModule::new(
        graphics.clone(),
        hwnd,
    )));
    let quickask = Rc::new(RefCell::new(quickask::QuickAskModule::new(
        graphics.clone(),
        hwnd,
        started,
    )));
    let mut manager = ModuleManager::default();
    manager.register(audio.clone());
    manager.register(monitor.clone());
    manager.register(memory.clone());
    manager.register(utilities.clone());
    manager.register(clipboard.clone());
    manager.register(vision.clone());
    manager.register(quickask.clone());
    for module in file_tools.modules() {
        manager.register(Rc::new(RefCell::new(module)));
    }
    let tray = Tray::new(hwnd)?;
    let mut autostart_enabled = false;
    match autostart::status() {
        Ok(status) => {
            autostart_enabled = status.enabled;
            if store.config.app.autostart
                && (!status.enabled || !status.path_matches)
                && single_instance::is_elevated()?
            {
                match autostart::set_enabled(true, &data_dir) {
                    Ok(()) => autostart_enabled = true,
                    Err(error) => tracing::warn!("autostart registration: {error:#}"),
                }
            } else if status.enabled
                && !status.path_matches
                && let Err(error) =
                    tray.notify(&tr("app.name"), &tr("notification.autostart_moved"))
            {
                tracing::warn!("autostart notification: {error:#}");
            }
        }
        Err(error) => tracing::warn!("autostart status: {error:#}"),
    }
    let ctx = CoreContext {
        config: store.config.clone(),
        data_dir,
        ocr: std::sync::Arc::new(orbment_core::ocr::OcrRouter::default()),
    };
    let app = Rc::new(RefCell::new(App {
        store,
        ctx,
        database,
        last_checkpoint: Instant::now(),
        last_log_cleanup: Instant::now(),
        manager,
        audio,
        monitor,
        memory,
        clipboard,
        file_tools,
        vision,
        quickask,
        vision_gestures: None,
        vision_hook_config: None,
        search_popup,
        utilities,
        graphics,
        settings: None,
        hotkeys: Hotkeys::new(hwnd),
        hotkey_actions: BTreeMap::new(),
        hotkey_conflicts: Vec::new(),
        tray,
        hwnd,
        monitor_unavailable: BTreeMap::new(),
        audio_tray: None,
        memory_tray: None,
        memory_load: 0,
        last_playback: None,
        last_recording: None,
        audio_icon_recording: false,
        autostart_enabled,
        pipe: None,
        requests,
        events: sender.clone(),
    }));
    app.borrow_mut().start_modules();
    app.borrow_mut().sync_pipe()?;
    if let Some(path) = options.pin_image {
        app.borrow_mut()
            .command("vision.pin_create", json!({"image_path":path}))?;
    }
    let mut bus = ActionBus::default();
    for name in [
        "app.status",
        "app.config_get",
        "app.config_set",
        "audio.list_devices",
        "audio.switch_playback",
        "audio.switch_recording",
        "audio.route_process",
        "audio.toggle_mic_mute",
        "audio.quick_menu_playback",
        "audio.quick_menu_recording",
        "audio.apply_profile",
        "audio.toggle_knob_remap",
        "monitor.snapshot",
        "memory.overview",
        "memory.clean",
        "memory.port_lookup",
        "memory.process_kill",
        "memory.process_reap_preview",
        "memory.process_reap",
        "memory.open_analyzer",
        "memory.open_reaper",
        "memory.open_ports",
        "memory.elevate",
        "memory.toggle_window",
        "app.open_task_manager",
        "utilities.set_topmost",
        "utilities.pick_color",
        "utilities.keep_awake",
        "utilities.display_brightness",
        "utilities.brightness_up",
        "utilities.brightness_down",
        "utilities.open_status",
        "clipboard.toggle",
        "clipboard.open",
        "clipboard.write",
        "clipboard.paste",
        "clipboard.quick_paste",
        "clipboard.open_settings",
        "file_search.toggle",
        "file_search.query",
        "disk_analyzer.open",
        "disk_analyzer.open_locks",
        "disk_analyzer.top_consumers",
        "disk_analyzer.file_lock_query",
        "disk_analyzer.file_unlock",
        "monitor.traffic_history",
        "monitor.toggle_taskbar",
        "monitor.toggle_floating",
        "app.open_config",
        "app.reload_config",
        "monitor.toggle_more_info",
        "monitor.open_history",
        "monitor.open_settings",
        "monitor.open_skins",
        "app.about",
        "app.export_diag",
        "app.toggle_autostart",
        "app.exit",
    ] {
        let app = app.clone();
        bus.register(name, move |params| app.borrow_mut().command(name, params))?;
    }
    for &name in vision::ACTIONS
        .iter()
        .chain(["quick_ask.open", "quick_ask.capture", "app.clear_traces"].iter())
    {
        let app = app.clone();
        bus.register(name, move |params| app.borrow_mut().command(name, params))?;
    }
    event_loop.run(|event| {
        let result = (|| -> Result<()> {
            let audio_tray_event = matches!(&event, UiEvent::AudioTray(_));
            let memory_tray_event = matches!(&event, UiEvent::MemoryTray(_));
            let mut _menu_foreground = None;
            let action = match event {
                UiEvent::Action(action) => Some((action, Value::Null)),
                UiEvent::VolumeMute => orbment_audio::knob::action(&app.borrow().ctx.config.audio)
                    .map(|action| (action.to_owned(), Value::Null)),
                UiEvent::Hotkey(id) => app.borrow().hotkey_actions.get(&id).cloned(),
                UiEvent::TaskbarCreated => {
                    let app = app.borrow();
                    for tray in std::iter::once(&app.tray)
                        .chain(app.audio_tray.iter())
                        .chain(app.memory_tray.iter())
                    {
                        if let Err(error) = tray.readd() {
                            tracing::warn!("tray registration after TaskbarCreated: {error:#}");
                        }
                    }
                    if let Some(window) = &mut app.monitor.borrow_mut().window {
                        window.reattach()?;
                    }
                    None
                }
                UiEvent::DisplayChanged => {
                    app.borrow_mut().utilities.borrow_mut().display_changed()?;
                    None
                }
                UiEvent::Activate => Some(("app.open_config".into(), Value::Null)),
                UiEvent::PinImage(path) => {
                    Some(("vision.pin_create".into(), json!({"image_path":path})))
                }
                UiEvent::Tray(message)
                | UiEvent::AudioTray(message)
                | UiEvent::MemoryTray(message) => match message {
                    WM_CONTEXTMENU => {
                        let app = app.borrow();
                        _menu_foreground = app
                            .audio
                            .borrow()
                            .service()
                            .ok()
                            .map(|service| service.capture_menu_foreground());
                        app.tray
                            .show_menu(&app.menu())?
                            .map(|action| (action, Value::Null))
                    }
                    WM_MBUTTONUP if memory_tray_event => {
                        use orbment_core::config::MemoryTrayAction;
                        let action =
                            match app.borrow().ctx.config.memory.tray_icon.middle_click_action {
                                MemoryTrayAction::CleanMemory => "memory.clean",
                                MemoryTrayAction::ToggleWindow => "memory.toggle_window",
                                MemoryTrayAction::OpenTaskMgr => "app.open_task_manager",
                            };
                        Some((action.into(), Value::Null))
                    }
                    // Version 4 sends both button-up and NIN_SELECT for a mouse click.
                    NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONDBLCLK => {
                        let app = app.borrow();
                        Some((
                            tray_left_action(
                                message,
                                audio_tray_event,
                                memory_tray_event,
                                &app.ctx.config.audio,
                            )
                            .into(),
                            Value::Null,
                        ))
                    }
                    // "none" is the settings option that disables the middle-click action.
                    WM_MBUTTONUP => {
                        Some(app.borrow().ctx.config.app.tray_middle_click_action.clone())
                            .filter(|action| action != "none")
                            .map(|action| (action, Value::Null))
                    }
                    _ => None,
                },
                UiEvent::Wake => {
                    while let Ok(event) = receiver.try_recv() {
                        let result = app.borrow_mut().event(event);
                        match result {
                            Ok(Some((action, params))) => {
                                if let Err(error) = bus.call(&action, params) {
                                    app.borrow().report(&error);
                                }
                            }
                            Ok(None) => {}
                            Err(error) => app.borrow().report(&error),
                        }
                    }
                    // Gate on the owning UI thread; blocking analyzer work replies from a worker.
                    while let Ok(pending) = pending_requests.try_recv() {
                        if pending.is_cancelled() {
                            continue;
                        }
                        if let Err(error) = app.borrow().external_allowed(&pending.request.action) {
                            pending.reply(Err(error));
                            continue;
                        }
                        app.borrow_mut().dispatch_request(pending);
                    }
                    app.borrow_mut().settings_events();
                    let maintenance = app.borrow_mut().maintenance();
                    if let Err(error) = maintenance {
                        app.borrow().report(&error);
                    }
                    let actions = app.borrow_mut().vision_events();
                    for (action, params) in actions {
                        if let Err(error) = bus.call(&action, params) {
                            app.borrow().report(&error);
                        }
                    }
                    let (quickask, policy) = {
                        let app = app.borrow();
                        (app.quickask.clone(), app.ctx.config.mcp.clone())
                    };
                    if let Err(error) = quickask.borrow_mut().tick(&bus, &policy) {
                        app.borrow().report(&error);
                    }
                    let calls = quickask.borrow_mut().take_tool_calls();
                    for call in calls {
                        let policy = app.borrow().ctx.config.mcp.clone();
                        if let Some(request) = call.into_request(&policy) {
                            app.borrow_mut().dispatch_request(request);
                        }
                    }
                    None
                }
                UiEvent::IdleTimer => {
                    let mut app = app.borrow_mut();
                    let reclaim = {
                        let App { manager, ctx, .. } = &mut *app;
                        manager.reclaim_due(ctx, started.elapsed().as_millis() as u64)
                    };
                    if let Err(error) = reclaim {
                        app.report(&error);
                    }
                    let idle = u64::from(app.ctx.config.app.idle_destroy_seconds);
                    app.search_popup.tick(idle);
                    db::checkpoint(&app.database)?;
                    None
                }
            };
            if let Some((action, params)) = action
                && action != "none"
            {
                bus.call(&action, params)?;
            }
            Ok(())
        })();
        {
            let app = app.borrow_mut();
            let now_ms = started.elapsed().as_millis() as u64;
            app.vision.borrow_mut().sync_idle(now_ms);
            let deadline = app
                .manager
                .idle_deadline_ms(&app.ctx)
                .into_iter()
                .chain(
                    app.search_popup
                        .idle_deadline_ms(u64::from(app.ctx.config.app.idle_destroy_seconds)),
                )
                .min();
            event_loop.schedule_idle(deadline, now_ms);
        }
        if let Err(error) = result {
            app.borrow().report(&error);
        }
    })?;
    let mut app = app.borrow_mut();
    app.pipe.take();
    app.settings.take();
    app.search_popup.hide();
    let audio = app.audio.clone();
    if let Some(gestures) = &mut app.vision_gestures
        && let Err(error) = gestures.uninstall(&mut audio.borrow_mut().hook)
    {
        tracing::error!(%error, "Removing global gestures during shutdown");
    }
    if let Err(error) = app
        .vision
        .borrow_mut()
        .release_recording_hooks(&mut app.audio.borrow_mut().hook)
    {
        tracing::error!(%error, "Removing recording hooks during shutdown");
    }
    let App { manager, ctx, .. } = &mut *app;
    manager.stop_all(ctx)?;
    app.file_tools.shutdown()?;
    db::checkpoint(&app.database)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbment_core::config::{AudioConfig, VisionConfig};

    #[test]
    fn hotkey_errors_name_the_action_and_chord_without_internal_identifiers() {
        let occupied = anyhow::Error::new(windows::core::Error::from_hresult(
            windows::core::HRESULT::from_win32(
                windows::Win32::Foundation::ERROR_HOTKEY_ALREADY_REGISTERED.0,
            ),
        ))
        .context("RegisterHotKey failed")
        .context("Registering hotkey 500");
        assert_eq!(
            hotkey_failure_message("clipboard.toggle", "Alt+C", &occupied),
            "「剪贴板历史」快捷键 Alt+C 已被其他程序占用"
        );
        assert_eq!(
            hotkey_failure_message("vision.capture", "F1", &occupied),
            "「区域截图」快捷键 F1 已被其他程序占用"
        );
        assert_eq!(
            hotkey_failure_message("audio.switch_playback", "Ctrl+Alt+F11", &occupied),
            "「切换播放设备」快捷键 Ctrl+Alt+F11 已被其他程序占用"
        );
        let invalid = anyhow::anyhow!("Invalid hotkey 20: Ctrl+Unknown");
        let message = hotkey_failure_message("clipboard.toggle", "Ctrl+Unknown", &invalid);
        assert!(message.starts_with("「剪贴板历史」快捷键 Ctrl+Unknown 注册失败"));
        assert!(!message.contains("20"));
        assert!(!message.contains("已被其他程序占用"));
    }

    #[test]
    fn main_tray_always_opens_settings_and_memory_tray_opens_analyzer() {
        let mut audio = AudioConfig::default();
        for enabled in [false, true] {
            audio.enabled = enabled;
            for separate_icon in [false, true] {
                audio.tray_icon.enabled = separate_icon;
                for message in [NIN_SELECT, NIN_KEYSELECT, WM_LBUTTONDBLCLK] {
                    assert_eq!(
                        tray_left_action(message, false, false, &audio),
                        "app.open_config"
                    );
                    assert_eq!(
                        tray_left_action(message, false, true, &audio),
                        "memory.open_analyzer"
                    );
                }
            }
        }
    }

    #[test]
    fn only_audio_icon_double_click_can_switch_devices() {
        let mut audio = AudioConfig {
            enabled: true,
            icon_double_click: "SwitchDevice".into(),
            ..Default::default()
        };
        for message in [NIN_SELECT, NIN_KEYSELECT] {
            assert_eq!(
                tray_left_action(message, true, false, &audio),
                "app.open_config"
            );
        }
        assert_eq!(
            tray_left_action(WM_LBUTTONDBLCLK, true, false, &audio),
            "audio.switch_playback"
        );
        audio.enabled = false;
        assert_eq!(
            tray_left_action(WM_LBUTTONDBLCLK, true, false, &audio),
            "app.open_config"
        );
        audio.enabled = true;
        audio.icon_double_click = "OpenConfig".into();
        assert_eq!(
            tray_left_action(WM_LBUTTONDBLCLK, true, false, &audio),
            "app.open_config"
        );
    }

    #[test]
    fn default_capture_shortcuts_resolve_to_registered_actions() {
        let config = VisionConfig::default();
        assert_eq!(vision::hotkey_action("capture"), Some("vision.capture"));
        assert_eq!(vision::hotkey_action("pin"), Some("vision.pin_last"));
        for action in config.hotkeys.keys() {
            let resolved = vision::hotkey_action(action).expect("default hotkey must resolve");
            assert!(vision::ACTIONS.contains(&resolved));
        }
    }
}
