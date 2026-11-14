pub mod geometry;
pub use geometry::{CapturedImage, Point, Rect};
pub mod arrow_comment;
pub mod canvas_mark;
pub mod capture_overlay;
pub mod desktop_toolbar;
pub mod global_mouse;
pub mod long_shot;
pub mod ocr_onnx;
pub mod pin_text_select;
pub mod pin_window;
pub mod recorder;
pub mod save_template;
pub mod shell_menu;

use anyhow::{Result, ensure};
pub use capture_overlay::CaptureTextKind;
use capture_overlay::{
    CaptureAction, CaptureAndPinWindows, CaptureEvent, RecordingHud, SelectionShape,
};
use desktop_toolbar::{DesktopToolbar, ToolbarAction};
use orbment_core::{config::VisionConfig, ocr::OcrResult};
use orbment_ui::D2dContext;
use pin_window::{PinEvent, PinInfo, PinManager};
use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PinHistoryEntry {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub image_path: Option<PathBuf>,
    pub bounds: Rect,
    pub is_active: bool,
    pub created_at: String,
    pub closed_at: Option<String>,
}

#[derive(Clone, Debug)]
pub enum VisionEvent {
    CaptureReady {
        image: CapturedImage,
        rect: Rect,
        action: CaptureAction,
    },
    CaptureCancelled,
    CaptureTextCopied {
        text: String,
        kind: CaptureTextKind,
    },
    PinClosed(String),
    PinAction {
        id: String,
        action: String,
    },
    Action(String),
    RecordBegin {
        region: Rect,
        system_audio: bool,
        microphone: bool,
        encode_format: u32,
    },
    RecordPause(bool),
    RecordStop,
    RecordCancel,
    Error(String),
}

/// UI-thread owner. Capturers and UIA use separate MTA workers; UI resources stay on this thread.
pub struct VisionService {
    graphics: Rc<D2dContext>,
    pins: PinManager,
    capture: Option<CaptureAndPinWindows>,
    preview: Option<CaptureAndPinWindows>,
    toolbar: Option<DesktopToolbar>,
    recording_hud: Option<RecordingHud>,
    config: VisionConfig,
    wake: Option<isize>,
    excluded: bool,
}
impl VisionService {
    pub fn new(data_dir: &Path, graphics: Rc<D2dContext>) -> Result<Self> {
        Ok(Self {
            pins: PinManager::new(data_dir, graphics.clone())?,
            graphics,
            capture: None,
            preview: None,
            toolbar: None,
            recording_hud: None,
            config: VisionConfig::default(),
            wake: None,
            excluded: false,
        })
    }
    pub fn configure(&mut self, config: &VisionConfig) -> Result<()> {
        self.config = config.clone();
        self.pins.configure(config)?;
        self.configure_toolbar(config.desktop_toolbar_mode as u8)
    }
    pub fn capture(&self, rect: Rect) -> Result<CapturedImage> {
        capture_overlay::capture_rect(rect)
    }
    pub fn begin_capture(&mut self) -> Result<()> {
        ensure!(self.capture.is_none(), "Capture selection is already open");
        self.preview = None;
        let mut ignore = self.pins.handles();
        if let Some(toolbar) = &self.toolbar {
            ignore.push(toolbar.hwnd().0 as isize);
        }
        if let Some(hud) = &self.recording_hud {
            ignore.extend(hud.handles());
        }
        let mut capture = CaptureAndPinWindows::capture(self.graphics.clone(), &ignore)?;
        if let Some(wake) = self.wake {
            capture.set_wake_hwnd(wake);
        }
        let config = &self.config.screenshot;
        if config.ratio > 0.0 {
            capture.set_shape(SelectionShape::Ratio {
                width: config.ratio as f64,
                height: 1.0,
            });
        } else if config.enable_round_rect || config.select_mode == "RoundRect" {
            capture.set_shape(SelectionShape::Rounded { radius: 12.0 });
        } else if matches!(config.select_mode.as_str(), "Polygon" | "Freehand") {
            capture.set_shape(SelectionShape::Polygon(Vec::new()));
        }
        capture.configure(config);
        self.capture = Some(capture);
        Ok(())
    }
    pub fn set_selection_shape(&mut self, shape: SelectionShape) -> Result<()> {
        self.capture
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("No active capture"))?
            .set_shape(shape);
        Ok(())
    }
    pub fn finish_active_capture_as_pin(&mut self) -> Result<bool> {
        match &mut self.capture {
            Some(capture) => capture.finish_as_pin(),
            None => Ok(false),
        }
    }
    pub fn open_recording_hud(
        &mut self,
        region: Rect,
        system_audio: bool,
        microphone: bool,
        encode_format: u32,
    ) -> Result<()> {
        ensure!(
            self.recording_hud.is_none(),
            "Recording controls are already open"
        );
        self.close_capture();
        self.recording_hud = Some(RecordingHud::new(
            region,
            system_audio,
            microphone,
            encode_format,
            self.wake,
        )?);
        Ok(())
    }
    pub fn set_recording_active(&mut self, active: bool) {
        if let Some(hud) = &mut self.recording_hud {
            hud.set_active(active);
        }
    }
    pub fn close_recording_hud(&mut self) {
        self.recording_hud = None;
    }
    pub fn has_recording_hud(&self) -> bool {
        self.recording_hud.is_some()
    }
    pub fn close_capture(&mut self) {
        self.capture = None;
        self.preview = None;
    }
    pub fn has_active_capture(&self) -> bool {
        self.capture.is_some() || self.preview.is_some()
    }
    pub fn image_ui_visible(&self) -> bool {
        self.has_active_capture()
            || self.has_recording_hud()
            || self.pins.list().iter().any(|pin| pin.visible)
    }
    pub fn gesture_preview(&mut self, rect: Option<Rect>) -> Result<()> {
        let Some(rect) = rect else {
            self.preview = None;
            return Ok(());
        };
        if self.preview.is_none() {
            self.preview = Some(CaptureAndPinWindows::passive_capture(
                self.graphics.clone(),
                &self.pins.handles(),
            )?);
        }
        if let Some(preview) = &mut self.preview {
            preview.preview(rect);
        }
        Ok(())
    }
    pub fn pin_image(&mut self, image: CapturedImage, position: Point) -> Result<String> {
        let id = self.pins.image_pin(image, position)?;
        self.pins.set_capture_excluded(self.excluded)?;
        Ok(id)
    }
    pub fn pin_text(&mut self, text: String, html: bool, position: Point) -> Result<String> {
        let id = self.pins.text_pin(text, html, position)?;
        self.pins.set_capture_excluded(self.excluded)?;
        Ok(id)
    }
    pub fn pin_color(&mut self, rgb: [u8; 3], position: Point) -> Result<String> {
        let id = self.pins.color_pin(rgb, position)?;
        self.pins.set_capture_excluded(self.excluded)?;
        Ok(id)
    }
    pub fn pin_window(
        &mut self,
        hwnd: isize,
        roi: Option<Rect>,
        position: Point,
    ) -> Result<String> {
        let id = self.pins.window_pin(hwnd, roi, position)?;
        self.pins.set_capture_excluded(self.excluded)?;
        Ok(id)
    }
    pub fn list_pins(&self) -> Vec<PinInfo> {
        self.pins.list()
    }
    pub fn pin_history(&self) -> Result<Vec<PinHistoryEntry>> {
        self.pins.history()
    }
    pub fn restore_pin_by_id(&mut self, id: &str) -> Result<String> {
        let id = self.pins.restore_by_id(id)?;
        self.pins.set_capture_excluded(self.excluded)?;
        Ok(id)
    }
    pub fn delete_pin_history(&mut self, id: &str) -> Result<()> {
        self.pins.delete_history(id)
    }
    pub fn close_all_pins(&mut self) -> Result<()> {
        self.pins.close_all()
    }
    pub fn close_pin(&mut self, id: &str) -> Result<()> {
        self.pins.close(id)
    }
    pub fn restore(&mut self) -> Result<usize> {
        let count = self.pins.restore()?;
        self.pins.set_capture_excluded(self.excluded)?;
        Ok(count)
    }
    pub fn restore_last(&mut self) -> Result<Option<String>> {
        let id = self.pins.restore_last()?;
        self.pins.set_capture_excluded(self.excluded)?;
        Ok(id)
    }
    pub fn set_pin_group(&mut self, id: &str, group: &str) -> Result<()> {
        self.pins.set_group(id, group)
    }
    pub fn show_pins(&mut self, group: Option<&str>, visible: bool) -> Result<()> {
        self.pins.show_group(group, visible)
    }
    pub fn set_pin_controls(
        &mut self,
        id: &str,
        topmost: bool,
        locked: bool,
        penetrate: bool,
        thumbnail: bool,
        opacity: f64,
    ) -> Result<()> {
        self.pins
            .set_controls(id, topmost, locked, penetrate, thumbnail, opacity)
    }
    pub fn set_pin_ocr(&mut self, id: &str, result: OcrResult) -> Result<()> {
        self.pins.set_ocr(id, result)
    }
    pub fn set_pin_scene(&mut self, id: &str, scene: canvas_mark::Scene) -> Result<()> {
        self.pins.set_scene(id, scene)
    }
    pub fn pin_image_data(&self, id: &str) -> Result<CapturedImage> {
        self.pins.image(id)
    }
    pub fn pin_ocr_image_data(&self, id: &str) -> Result<CapturedImage> {
        self.pins.ocr_image(id)
    }
    pub fn pin_image_revision(&self, id: &str) -> Result<u64> {
        self.pins.image_revision(id)
    }
    pub fn pin_window_handle(&self, id: &str) -> Result<isize> {
        self.pins.window_handle(id)
    }
    pub fn hide_pin_for_save(&self, id: &str) -> Result<pin_window::PinVisibilityGuard> {
        self.pins.hide_for_save(id)
    }
    pub fn replace_pin_image(&mut self, id: &str, image: CapturedImage) -> Result<()> {
        self.pins.replace_image(id, image)
    }
    pub fn pin_bounds(&self, id: &str) -> Result<Rect> {
        self.pins.bounds(id)
    }
    pub fn begin_pin_annotation(&mut self, id: &str) -> Result<()> {
        self.pins.begin_annotation(id)
    }
    pub fn save_pin(&mut self, id: &str, directory: &Path) -> Result<PathBuf> {
        self.pins.save(id, directory)
    }
    pub fn configure_toolbar(&mut self, mode: u8) -> Result<()> {
        if mode == 2 {
            self.toolbar = None;
            return Ok(());
        }
        if let Some(toolbar) = &mut self.toolbar {
            toolbar.configure(mode)?;
        } else {
            let mut toolbar = DesktopToolbar::new(mode)?;
            if let Some(wake) = self.wake {
                toolbar.set_wake_hwnd(wake);
            }
            self.toolbar = Some(toolbar);
        }
        Ok(())
    }
    pub fn cleanup_shell_menu(&self) -> Result<()> {
        shell_menu::cleanup()
    }
    pub fn set_wake_hwnd(&mut self, hwnd: isize) {
        self.wake = Some(hwnd);
        self.pins.set_wake_hwnd(hwnd);
        if let Some(capture) = &mut self.capture {
            capture.set_wake_hwnd(hwnd);
        }
        if let Some(toolbar) = &mut self.toolbar {
            toolbar.set_wake_hwnd(hwnd);
        }
        if let Some(hud) = &mut self.recording_hud {
            hud.set_wake_hwnd(hwnd);
        }
    }
    pub fn set_capture_excluded(&mut self, excluded: bool) -> Result<()> {
        self.pins.set_capture_excluded(excluded)?;
        self.excluded = excluded;
        Ok(())
    }
    pub fn rebuild_graphics(&mut self, graphics: Rc<D2dContext>) -> Result<()> {
        self.close_capture();
        self.graphics = graphics.clone();
        self.pins.rebuild_graphics(graphics)
    }
    pub fn poll(&mut self) -> Vec<VisionEvent> {
        let mut events = Vec::new();
        if let Some(capture) = &mut self.capture {
            for event in capture.poll() {
                events.push(match event {
                    CaptureEvent::Selected {
                        image,
                        rect,
                        action,
                    } => VisionEvent::CaptureReady {
                        image,
                        rect,
                        action,
                    },
                    CaptureEvent::Cancelled => VisionEvent::CaptureCancelled,
                    CaptureEvent::TextCopied { text, kind } => {
                        VisionEvent::CaptureTextCopied { text, kind }
                    }
                    CaptureEvent::Error(error) => VisionEvent::Error(error),
                });
            }
            if capture.finished() {
                self.capture = None;
            }
        }
        if let Some(hud) = &mut self.recording_hud {
            events.extend(hud.poll());
            if hud.closed() {
                self.recording_hud = None;
            }
        }
        for event in self.pins.poll() {
            events.push(match event {
                PinEvent::Closed(id) => VisionEvent::PinClosed(id),
                PinEvent::Action { id, action } => VisionEvent::PinAction { id, action },
                PinEvent::Error(error) => VisionEvent::Error(error),
            });
        }
        let toolbar_events = self
            .toolbar
            .as_mut()
            .map(|toolbar| toolbar.poll())
            .unwrap_or_default();
        for event in toolbar_events {
            let result = match event {
                ToolbarAction::Capture => self.begin_capture(),
                ToolbarAction::Restore => self.restore_last().map(|_| ()),
                ToolbarAction::Record => {
                    events.push(VisionEvent::Action("vision.record_start".into()));
                    Ok(())
                }
                ToolbarAction::TogglePins => {
                    let visible = !self.pins.list().iter().any(|pin| pin.visible);
                    self.pins.show_group(None, visible)
                }
            };
            if let Err(error) = result {
                events.push(VisionEvent::Error(error.to_string()));
            }
        }
        events
    }
    pub fn cleanup(&mut self) -> Result<()> {
        self.close_capture();
        self.close_recording_hud();
        self.toolbar = None;
        self.pins.shutdown()
    }
}

/// Native Save As, shared by capture results and annotated pins. Cancellation is
/// an ordinary outcome; overwrite confirmation belongs to the shell dialog.
pub fn save_image_dialog(
    owner: windows::Win32::Foundation::HWND,
    image: &CapturedImage,
) -> Result<Option<PathBuf>> {
    use windows::{
        Win32::{
            Foundation::ERROR_CANCELLED,
            System::{
                Com::{
                    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
                    CoInitializeEx, CoTaskMemFree, CoUninitialize,
                },
                SystemInformation::GetLocalTime,
            },
            UI::Shell::{
                Common::COMDLG_FILTERSPEC, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT,
                FOS_PATHMUSTEXIST, FOS_STRICTFILETYPES, FileSaveDialog, IFileSaveDialog,
                SIGDN_FILESYSPATH,
            },
        },
        core::{HRESULT, PCWSTR, w},
    };
    image.validate()?;
    struct DialogApartment;
    impl Drop for DialogApartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _apartment = DialogApartment;
        let dialog: IFileSaveDialog =
            CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetOptions(
            dialog.GetOptions()?
                | FOS_FORCEFILESYSTEM
                | FOS_OVERWRITEPROMPT
                | FOS_PATHMUSTEXIST
                | FOS_STRICTFILETYPES,
        )?;
        dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
            pszName: w!("PNG 图片 (*.png)"),
            pszSpec: w!("*.png"),
        }])?;
        dialog.SetDefaultExtension(w!("png"))?;
        let now = GetLocalTime();
        let filename = capture_overlay::wide(&format!(
            "Orbment_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.png",
            now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
        ));
        dialog.SetFileName(PCWSTR(filename.as_ptr()))?;
        if let Err(error) = dialog.Show((!owner.is_invalid()).then_some(owner)) {
            if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                return Ok(None);
            }
            return Err(error.into());
        }
        let path = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        let decoded = path.to_string();
        CoTaskMemFree(Some(path.0.cast()));
        let path = PathBuf::from(decoded?);
        image.save_png(&path)?;
        Ok(Some(path))
    }
}
