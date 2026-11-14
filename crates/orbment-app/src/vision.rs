use anyhow::{Context, Result, bail, ensure};
use orbment_core::{
    OcrEngine, OcrResult,
    lifecycle::{ActivateTrigger, CoreContext, OrbmentModule, ServiceState, UiSurfaceState},
};
use orbment_quickask::{ContextImage, QuickContext};
use orbment_ui::D2dContext;
use orbment_vision::{
    CaptureTextKind, CapturedImage, Point, Rect, VisionEvent, VisionService,
    capture_overlay::{self, CaptureAction, CaptureSource},
    long_shot::{AutoScrollOptions, Axis, LongShotAction, LongShotOutcome, ScrollChannel},
    ocr_onnx::{OnnxOcr, PostprocessOptions},
    save_template::{self, SaveTemplateContext},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect},
};

pub const ACTIONS: &[&str] = &[
    "vision.capture_screen",
    "vision.ocr",
    "vision.pin_create",
    "vision.pin_list",
    "vision.pin_close",
    "vision.pin_history",
    "vision.pin_restore",
    "vision.pin_delete",
    "vision.pin_close_all",
    "vision.open_history",
    "vision.capture",
    "vision.pin_last",
    "vision.restore_last",
    "vision.annotate",
    "vision.pin_select_text",
    "vision.ocr_capture",
    "vision.ocr_show",
    "vision.capture_for_quickask",
    "vision.pin_action",
    "vision.longshot",
    "vision.longshot_cancel",
    "vision.record_start",
    "vision.record_stop",
    "vision.record_cancel",
    "vision.record_edit",
    "vision.copy_image",
    "vision.save_image",
    "vision.quick_save",
    "vision.replace_pin",
];

pub fn hotkey_action(name: &str) -> Option<&'static str> {
    match name {
        "capture" | "screenshot" | "vision.capture_screen" | "vision.capture" => {
            Some("vision.capture")
        }
        "pin" | "pin_last" | "vision.pin_last" => Some("vision.pin_last"),
        "ocr" | "vision.ocr" => Some("vision.ocr_capture"),
        "restore_last" | "vision.restore_last" => Some("vision.restore_last"),
        "longshot" | "vision.longshot" => Some("vision.longshot"),
        "record" | "record_start" | "vision.record_start" => Some("vision.record_start"),
        "record_stop" | "vision.record_stop" => Some("vision.record_stop"),
        "record_edit" | "vision.record_edit" => Some("vision.record_edit"),
        "quick_ask" | "quick_ask.capture" => Some("vision.capture_for_quickask"),
        _ => None,
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CaptureParams {
    monitor: Option<u32>,
    window: Option<isize>,
    region: Option<Rect>,
    output_path: Option<PathBuf>,
    pin: bool,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OcrParams {
    image_path: Option<PathBuf>,
    region: Option<Rect>,
    engine: Option<String>,
    capture_id: Option<u64>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PinParams {
    image_path: Option<PathBuf>,
    text: Option<String>,
    html: Option<String>,
    color: Option<String>,
    window: Option<isize>,
    region: Option<Rect>,
    x: Option<f64>,
    y: Option<f64>,
    group: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdParams {
    id: String,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LongShotParams {
    region: Option<Rect>,
    horizontal: bool,
    window_messages: bool,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RecordParams {
    region: Option<Rect>,
    system_audio: Option<bool>,
    microphone: Option<bool>,
    quick: bool,
    start_immediately: bool,
    encode_format: Option<u32>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExportParams {
    output_path: Option<PathBuf>,
    start_100ns: Option<i64>,
    end_100ns: Option<i64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ImageActionParams {
    capture_id: Option<u64>,
}

#[derive(Default)]
struct PendingImageActions {
    next: u64,
    images: std::collections::HashMap<u64, CapturedImage>,
}
impl PendingImageActions {
    fn bind(&mut self, command: (String, Value), image: &CapturedImage) -> (String, Value) {
        if !matches!(
            command.0.as_str(),
            "vision.copy_image"
                | "vision.save_image"
                | "vision.quick_save"
                | "vision.ocr_show"
                | "quick_ask.capture"
        ) {
            return command;
        }
        self.next += 1;
        self.images.insert(self.next, image.clone());
        (command.0, json!({"capture_id":self.next}))
    }
    fn take(&mut self, id: u64) -> Result<CapturedImage> {
        self.images.remove(&id).context("Capture action expired")
    }
}

pub struct OcrJob {
    engine: Arc<dyn OcrEngine>,
    path: PathBuf,
    image: Option<CapturedImage>,
}
struct OcrInputFile(PathBuf);
impl Drop for OcrInputFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl OcrJob {
    fn recognize(self) -> Result<OcrResult> {
        let _input = if self.image.is_some() {
            // Only a file exclusively created by this job may be removed.
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.path)?;
            Some(OcrInputFile(self.path.clone()))
        } else {
            None
        };
        if let Some(image) = self.image {
            image.save_png(&self.path)?;
        }
        self.engine.recognize(&self.path)
    }

    pub fn run(self) -> Result<Value> {
        Ok(serde_json::to_value(self.recognize()?)?)
    }
}

pub struct CaptureJob {
    image: CapturedImage,
    path: PathBuf,
    result: Value,
}
impl CaptureJob {
    fn run(self) -> Result<Value> {
        self.image.save_png(&self.path)?;
        Ok(self.result)
    }
}

enum Completion {
    LongShot(Rect, Result<LongShotOutcome>),
    PinOcr(String, u64, PathBuf, Result<OcrResult>),
    Ocr(Option<PathBuf>, Result<OcrResult>),
    Export(Result<PathBuf>),
    Capture(PathBuf, Result<Value>),
    QuickSaved(PathBuf, Result<PathBuf>),
    QuickContext(Result<QuickContext>),
    RequestFinished(Option<PathBuf>),
    Recorded {
        recording: Box<Result<orbment_vision::recorder::RecordedSession>>,
        path: PathBuf,
        format: orbment_vision::recorder::ExportFormat,
        effects: orbment_vision::recorder::EffectOptions,
        close_after_export: bool,
    },
}

pub struct VisionModule {
    graphics: Rc<D2dContext>,
    hwnd: usize,
    service: Option<VisionService>,
    state: ServiceState,
    last: Option<(CapturedImage, Rect)>,
    hidden_at_ms: Option<u64>,
    last_pin: Option<String>,
    counter: u64,
    offline: Option<Arc<OnnxOcr>>,
    offline_config: Value,
    shell_menu_cleaned: bool,
    tx: Sender<(thread::ThreadId, Completion)>,
    rx: Receiver<(thread::ThreadId, Completion)>,
    workers: Vec<JoinHandle<()>>,
    pending_png_paths: std::collections::BTreeSet<PathBuf>,
    longshot_cancel: Option<Arc<AtomicBool>>,
    recording: Option<orbment_vision::recorder::RecordingSession>,
    recording_quick: bool,
    recording_format: u32,
    mouse_hook: Option<orbment_ui::HookSubscription>,
    keyboard_hook: Option<orbment_ui::HookSubscription>,
    pending_capture: Option<(String, Value)>,
    image_actions: PendingImageActions,
    exported: Vec<PathBuf>,
    quick_contexts: Vec<QuickContext>,
    export_cancel: Option<Arc<AtomicBool>>,
    ocr_pins: std::collections::HashMap<String, u64>,
    editor: Option<crate::recording_editor::RecordingEditor>,
    feedback: Option<orbment_ui::OverlayFloatingWindow>,
}

impl VisionModule {
    pub fn new(graphics: Rc<D2dContext>, hwnd: HWND) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            graphics,
            hwnd: hwnd.0 as usize,
            service: None,
            state: ServiceState::Disabled,
            last: None,
            hidden_at_ms: None,
            last_pin: None,
            counter: 0,
            offline: None,
            offline_config: Value::Null,
            shell_menu_cleaned: false,
            tx,
            rx,
            workers: Vec::new(),
            pending_png_paths: Default::default(),
            longshot_cancel: None,
            recording: None,
            pending_capture: None,
            image_actions: PendingImageActions::default(),
            recording_quick: false,
            recording_format: 0,
            mouse_hook: None,
            keyboard_hook: None,
            exported: Vec::new(),
            quick_contexts: Vec::new(),
            export_cancel: None,
            ocr_pins: Default::default(),
            editor: None,
            feedback: None,
        }
    }
    pub fn service(&mut self) -> Result<&mut VisionService> {
        self.service
            .as_mut()
            .context("Vision module is unavailable")
    }
    pub fn has_active_capture(&self) -> bool {
        self.service
            .as_ref()
            .is_some_and(VisionService::has_active_capture)
    }
    pub fn close_active_capture(&mut self) {
        if let Some(service) = &mut self.service {
            service.close_capture();
        }
        self.pending_capture = None;
    }
    pub fn take_device_lost(&mut self) -> bool {
        self.editor
            .as_mut()
            .is_some_and(crate::recording_editor::RecordingEditor::take_device_lost)
    }
    pub fn activate_pin(&mut self, id: &str) -> Result<()> {
        let active = self.service()?.list_pins().iter().any(|pin| pin.id == id);
        let bounds = if active {
            self.service()?.pin_bounds(id)?
        } else {
            self.service()?
                .pin_history()?
                .into_iter()
                .find(|entry| entry.id == id)
                .context("Pin history entry was not found")?
                .bounds
        };
        self.last = Some((self.service()?.pin_image_data(id)?, bounds));
        self.hidden_at_ms = None;
        self.last_pin = active.then(|| id.into());
        Ok(())
    }
    pub fn request_ocr(&mut self, job: OcrJob, pending: orbment_core::ipc::PendingRequest) {
        let path = job.image.as_ref().map(|_| job.path.clone());
        if let Some(path) = &path {
            self.pending_png_paths.insert(path.clone());
        }
        self.spawn(move || {
            if !pending.is_cancelled() {
                pending.reply(job.run());
            }
            Completion::RequestFinished(path)
        });
    }
    pub fn request_capture(&mut self, job: CaptureJob, pending: orbment_core::ipc::PendingRequest) {
        let path = job.path.clone();
        self.pending_png_paths.insert(path.clone());
        self.spawn(move || {
            if !pending.is_cancelled() {
                pending.reply(job.run());
            }
            Completion::RequestFinished(Some(path))
        });
    }
    pub fn take_quick_contexts(&mut self) -> Vec<QuickContext> {
        std::mem::take(&mut self.quick_contexts)
    }
    pub fn take_exported(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.exported)
    }
    pub fn apply_config(&mut self, ctx: &CoreContext) -> Result<()> {
        if let Err(error) = self.configure(ctx) {
            ctx.ocr.unregister_offline();
            self.offline = None;
            self.state = ServiceState::Unavailable {
                reason: "vision_config_failed",
            };
            return Err(error);
        }
        Ok(())
    }
    fn configure(&mut self, ctx: &CoreContext) -> Result<()> {
        if !ctx.config.vision.enabled {
            if !self.shell_menu_cleaned {
                orbment_vision::shell_menu::cleanup()?;
                self.shell_menu_cleaned = true;
            }
            return self.stop_service(ctx);
        }
        let starting = self.service.is_none();
        if starting {
            self.state = ServiceState::Unavailable {
                reason: "vision_start_failed",
            };
            self.service = Some(VisionService::new(&ctx.data_dir, self.graphics.clone())?);
            self.service
                .as_mut()
                .expect("service created")
                .set_wake_hwnd(self.hwnd as isize);
            orbment_vision::recorder::cleanup_stale_sessions(
                &ctx.data_dir.join("temp/recordings"),
            )?;
        }
        let offline_config =
            json!({"idle":ctx.config.app.ocr_idle_unload_seconds, "ocr":ctx.config.vision.ocr});
        if self.offline.is_none() || self.offline_config != offline_config {
            let engine = Arc::new(
                OnnxOcr::new(
                    &ctx.data_dir,
                    Duration::from_secs(u64::from(ctx.config.app.ocr_idle_unload_seconds)),
                )
                .with_options(PostprocessOptions::from(&ctx.config.vision.ocr)),
            );
            ctx.ocr.register_offline(engine.clone());
            self.offline = Some(engine);
            self.offline_config = offline_config;
        }
        self.service()?.configure(&ctx.config.vision)?;
        if !self.shell_menu_cleaned {
            self.service()?.cleanup_shell_menu()?;
            self.shell_menu_cleaned = true;
        }
        if starting && ctx.config.vision.pin.restore_unclosed_on_startup {
            self.service()?.restore()?;
        }
        self.state = ServiceState::Running;
        Ok(())
    }
    fn ocr_input_path() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "orbment-ocr-{}-{timestamp}-{}.png",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
    fn next_path(&mut self, ctx: &CoreContext, extension: &str) -> Result<PathBuf> {
        let root = ctx.data_dir.join("media/captures");
        std::fs::create_dir_all(&root)?;
        self.counter += 1;
        let name = save_template::expand(
            &ctx.config.vision.save.manual_save_pattern,
            &SaveTemplateContext::now("", self.counter),
        );
        ensure!(
            Path::new(&name)
                .file_name()
                .is_some_and(|part| part == std::ffi::OsStr::new(&name)),
            "Capture filename template must produce a filename"
        );
        let mut path = root.join(name).with_extension(extension);
        ensure!(
            path.file_name().is_some() && path.parent() == Some(root.as_path()),
            "Capture filename extension must stay inside the capture directory"
        );
        while path.exists() || self.pending_png_paths.contains(&path) {
            self.counter += 1;
            let stem = path
                .file_stem()
                .context("capture filename")?
                .to_string_lossy();
            path = root.join(format!("{stem}_{}.{}", self.counter, extension));
        }
        Ok(path)
    }
    fn selected_rect(&self, region: Option<Rect>) -> Result<Rect> {
        region
            .or_else(|| self.last.as_ref().map(|(_, rect)| *rect))
            .context("Select a capture region first")?
            .validate()
    }
    pub fn prepare_capture(&mut self, params: Value, ctx: &CoreContext) -> Result<CaptureJob> {
        ensure!(
            self.state == ServiceState::Running,
            "Vision module is disabled or unavailable"
        );
        let request: CaptureParams = crate::parameters(params)?;
        ensure!(
            usize::from(request.monitor.is_some())
                + usize::from(request.window.is_some())
                + usize::from(request.region.is_some())
                <= 1,
            "monitor, window and region are mutually exclusive"
        );
        let (image, rect) = if let Some(hwnd) = request.window {
            let mut rect = RECT::default();
            unsafe {
                GetWindowRect(HWND(hwnd as *mut _), &mut rect)?;
            }
            (
                capture_overlay::capture_source(CaptureSource::Window { hwnd, roi: None })?,
                Rect {
                    x: rect.left as f64,
                    y: rect.top as f64,
                    width: (rect.right - rect.left) as f64,
                    height: (rect.bottom - rect.top) as f64,
                },
            )
        } else {
            let rect = if let Some(index) = request.monitor {
                capture_overlay::monitors()?
                    .get(index as usize)
                    .context("Monitor index is out of range")?
                    .bounds
            } else {
                request
                    .region
                    .unwrap_or_else(capture_overlay::virtual_desktop)
            }
            .validate()?;
            (self.service()?.capture(rect)?, rect)
        };
        let path = match request.output_path {
            Some(path) => path,
            None => self.next_path(ctx, "png")?,
        };
        ensure!(
            !self.pending_png_paths.contains(&path),
            "Capture output is already being written"
        );
        let pin_id = if request.pin {
            Some(self.service()?.pin_image(
                image.clone(),
                Point {
                    x: rect.x,
                    y: rect.y,
                },
            )?)
        } else {
            None
        };
        self.last = Some((image.clone(), rect));
        self.hidden_at_ms = None;
        self.last_pin = pin_id.clone();
        Ok(CaptureJob {
            image,
            path: path.clone(),
            result: json!({"path":path,"region":rect,"pin_id":pin_id}),
        })
    }
    pub fn prepare_ocr(&mut self, params: Value, ctx: &CoreContext) -> Result<OcrJob> {
        ensure!(
            self.state == ServiceState::Running,
            "Vision module is disabled or unavailable"
        );
        let request: OcrParams = crate::parameters(params)?;
        ensure!(
            usize::from(request.image_path.is_some())
                + usize::from(request.region.is_some())
                + usize::from(request.capture_id.is_some())
                <= 1,
            "image_path, region and capture_id are mutually exclusive"
        );
        let tier = request
            .engine
            .as_deref()
            .unwrap_or(&ctx.config.app.ocr_engine_tier);
        let engine = ctx.ocr.resolve(tier)?;
        let (path, image) = if let Some(path) = request.image_path {
            (path, None)
        } else {
            let image = if let Some(region) = request.region {
                self.service()?.capture(region.validate()?)?
            } else if let Some(id) = request.capture_id {
                self.image_actions.take(id)?
            } else {
                self.last
                    .as_ref()
                    .context("Capture an image first")?
                    .0
                    .clone()
            };
            let path = Self::ocr_input_path();
            (path, Some(image))
        };
        Ok(OcrJob {
            engine,
            path,
            image,
        })
    }
    fn pin_create(&mut self, request: PinParams) -> Result<Value> {
        ensure!(
            [
                request.image_path.is_some(),
                request.text.is_some(),
                request.html.is_some(),
                request.color.is_some(),
                request.window.is_some()
            ]
            .into_iter()
            .filter(|present| *present)
            .count()
                == 1,
            "Exactly one pin content source is required"
        );
        ensure!(
            request.region.is_none() || request.window.is_some(),
            "region requires window"
        );
        let point = Point {
            x: request.x.unwrap_or(80.0),
            y: request.y.unwrap_or(80.0),
        };
        ensure!(
            point.x.is_finite() && point.y.is_finite(),
            "Pin coordinates must be finite"
        );
        let service = self.service()?;
        let id = if let Some(path) = request.image_path {
            service.pin_image(CapturedImage::load(&path)?, point)?
        } else if let Some(text) = request.text {
            service.pin_text(text, false, point)?
        } else if let Some(html) = request.html {
            service.pin_text(html, true, point)?
        } else if let Some(color) = request.color {
            service.pin_color(parse_color(&color)?, point)?
        } else {
            service.pin_window(request.window.context("window pin")?, request.region, point)?
        };
        if let Some(group) = request.group {
            service.set_pin_group(&id, &group)?;
        }
        self.last_pin = Some(id.clone());
        Ok(json!({"id":id}))
    }
    pub fn request_quick_context(&mut self, params: Value) -> Result<()> {
        let request: ImageActionParams = crate::parameters(params)?;
        let image = if let Some(id) = request.capture_id {
            self.image_actions.take(id)?
        } else {
            self.last
                .as_ref()
                .context("Capture an image first")?
                .0
                .clone()
        };
        self.spawn(move || {
            Completion::QuickContext(image.png_bytes().map(|bytes| QuickContext {
                text: String::new(),
                source_title: String::new(),
                image: Some(ContextImage {
                    mime_type: "image/png".into(),
                    bytes,
                }),
            }))
        });
        Ok(())
    }
    pub fn pin_color(&mut self, rgb: [u8; 3], x: i32, y: i32) -> Result<()> {
        self.last_pin = Some(self.service()?.pin_color(
            rgb,
            Point {
                x: x as f64,
                y: y as f64,
            },
        )?);
        Ok(())
    }
    pub fn gesture_preview(&mut self, region: Option<Rect>) -> Result<()> {
        self.service()?.gesture_preview(region)
    }
    fn pin_clipboard_or_last(&mut self) -> Result<()> {
        let mut cursor = POINT::default();
        unsafe { GetCursorPos(&mut cursor)? };
        let point = Point {
            x: f64::from(cursor.x),
            y: f64::from(cursor.y),
        };
        let id = match read_clipboard_pin(HWND(self.hwnd as *mut _))? {
            Some(ClipboardPin::Image(image)) => self.service()?.pin_image(image, point)?,
            Some(ClipboardPin::Text(text)) => self.service()?.pin_text(text, false, point)?,
            None => {
                let image = self
                    .last
                    .as_ref()
                    .context("剪贴板没有图片或文本，请先截图或复制内容")?
                    .0
                    .clone();
                self.service()?.pin_image(image, point)?
            }
        };
        self.last_pin = Some(id);
        Ok(())
    }
    fn selected_image(&mut self, params: Value) -> Result<(CapturedImage, Option<String>)> {
        let request: ImageActionParams = crate::parameters(params)?;
        if let Some(id) = request.capture_id {
            return Ok((self.image_actions.take(id)?, None));
        }
        if let Some(id) = self.last_pin.clone() {
            return Ok((self.service()?.pin_image_data(&id)?, Some(id)));
        }
        Ok((
            self.last
                .as_ref()
                .context("Capture an image first")?
                .0
                .clone(),
            None,
        ))
    }
    pub fn command(&mut self, action: &str, params: Value, ctx: &CoreContext) -> Result<Value> {
        ensure!(
            self.state == ServiceState::Running,
            "Vision module is disabled or unavailable"
        );
        match action {
            "vision.capture_screen" => {
                let job = self.prepare_capture(params, ctx)?;
                let path = job.path.clone();
                self.pending_png_paths.insert(path.clone());
                self.spawn(move || Completion::Capture(path, job.run()));
            }
            "vision.ocr" | "vision.ocr_show" => {
                let job = self.prepare_ocr(params, ctx)?;
                let path = job.image.as_ref().map(|_| job.path.clone());
                if let Some(path) = &path {
                    self.pending_png_paths.insert(path.clone());
                }
                self.spawn(move || Completion::Ocr(path, job.recognize()));
            }
            "vision.ocr_capture" | "vision.capture_for_quickask" => {
                self.service()?.begin_capture()?;
                self.pending_capture = Some((
                    if action == "vision.ocr_capture" {
                        "vision.ocr_show"
                    } else {
                        "quick_ask.capture"
                    }
                    .into(),
                    Value::Null,
                ));
            }
            "vision.pin_create" => return self.pin_create(crate::parameters(params)?),
            "vision.pin_list" => {
                let _: std::collections::BTreeMap<String, Value> =
                    crate::parameters(params.clone())?;
                ensure!(
                    params.is_null() || params.as_object().is_some_and(|p| p.is_empty()),
                    "pin_list takes no parameters"
                );
                return Ok(serde_json::to_value(self.service()?.list_pins())?);
            }
            "vision.pin_history" => {
                return Ok(serde_json::to_value(self.service()?.pin_history()?)?);
            }
            "vision.pin_restore" => {
                let request: IdParams = crate::parameters(params)?;
                let id = self.service()?.restore_pin_by_id(&request.id)?;
                self.last_pin = Some(id.clone());
                return Ok(json!({"id":id}));
            }
            "vision.pin_delete" => {
                let request: IdParams = crate::parameters(params)?;
                self.service()?.delete_pin_history(&request.id)?;
                if self.last_pin.as_ref() == Some(&request.id) {
                    self.last_pin = None;
                }
            }
            "vision.pin_close_all" => {
                self.service()?.close_all_pins()?;
                self.last_pin = None;
            }
            "vision.pin_close" => self
                .service()?
                .close_pin(&crate::parameters::<IdParams>(params)?.id)?,
            "vision.capture" => {
                self.service()?.begin_capture()?;
                self.pending_capture = None;
            }
            "vision.pin_last" => {
                if self.longshot_cancel.is_some() {
                    // The scrolling HUD owns F2 until its stitched result is delivered.
                    return Ok(json!({"ok":true}));
                }
                if self.service()?.finish_active_capture_as_pin()? {
                    return Ok(json!({"ok":true}));
                }
                self.pin_clipboard_or_last()?;
            }
            "vision.restore_last" => {
                self.last_pin = self.service()?.restore_last()?;
            }
            "vision.pin_select_text" => self.select_pin_text(params, ctx)?,
            "vision.annotate" => self.annotate(params)?,
            "vision.longshot" => self.start_longshot(crate::parameters(params)?, ctx)?,
            "vision.longshot_cancel" => {
                if let Some(cancel) = &self.longshot_cancel {
                    cancel.store(true, Ordering::Release);
                }
            }
            "vision.record_start" => self.start_recording(crate::parameters(params)?, ctx)?,
            "vision.record_stop" => self.stop_recording(crate::parameters(params)?, ctx)?,
            "vision.record_edit" => self
                .editor
                .as_mut()
                .context("No recording is available for editing")?
                .show()?,
            "vision.save_image" => {
                let (image, pin) = self.selected_image(params)?;
                let (owner, visibility) = if let Some(id) = pin {
                    let service = self.service()?;
                    if service.list_pins().iter().any(|pin| pin.id == id) {
                        (
                            HWND(service.pin_window_handle(&id)? as *mut _),
                            Some(service.hide_pin_for_save(&id)?),
                        )
                    } else {
                        (HWND::default(), None)
                    }
                } else {
                    (HWND::default(), None)
                };
                // Teardown and restoration stay outside the overlay's message handler.
                self.close_active_capture();
                let result = orbment_vision::save_image_dialog(owner, &image);
                drop(visibility);
                let path = result?;
                if path.is_some() {
                    self.feedback = Some(orbment_ui::OverlayFloatingWindow::new(
                        self.graphics.clone(),
                        "图片已保存",
                        1600,
                    )?);
                }
                return Ok(json!({"path":path,"cancelled":path.is_none()}));
            }
            "vision.quick_save" => {
                let (image, _) = self.selected_image(params)?;
                self.close_active_capture();
                let path = self.next_path(ctx, "png")?;
                self.pending_png_paths.insert(path.clone());
                self.spawn(move || {
                    Completion::QuickSaved(path.clone(), image.save_png_unique(&path))
                });
                return Ok(json!({"pending":true}));
            }
            "vision.replace_pin" => {
                let id = if params.is_null() || params == json!({}) {
                    self.last_pin.clone().context("Create a pin first")?
                } else {
                    crate::parameters::<IdParams>(params)?.id
                };
                let Some(ClipboardPin::Image(image)) =
                    read_clipboard_pin(HWND(self.hwnd as *mut _))?
                else {
                    bail!("剪贴板中没有可用于替换的图片");
                };
                self.service()?.replace_pin_image(&id, image.clone())?;
                let bounds = self.service()?.pin_bounds(&id)?;
                self.last = Some((image, bounds));
                self.last_pin = Some(id);
                self.hidden_at_ms = None;
            }
            "vision.copy_image" => {
                let (image, _) = self.selected_image(params)?;
                crate::clipboard::copy_image(HWND(self.hwnd as *mut _), &image)?;
                self.feedback = Some(orbment_ui::OverlayFloatingWindow::new(
                    self.graphics.clone(),
                    "图片已复制到剪贴板",
                    1400,
                )?);
            }
            "vision.record_cancel" => {
                self.service()?.close_recording_hud();
                if let Some(mut editor) = self.editor.take() {
                    editor.shutdown()?;
                }
                if let Some(cancel) = &self.export_cancel {
                    cancel.store(true, Ordering::Release);
                }
                if let Some(recording) = self.recording.take() {
                    recording.cancel()?;
                }
                if self.export_cancel.is_none() {
                    self.service()?.set_capture_excluded(false)?;
                }
            }
            _ => bail!("Unknown vision action: {action}"),
        }
        Ok(json!({"ok":true}))
    }
    fn spawn(&mut self, operation: impl FnOnce() -> Completion + Send + 'static) {
        let sender = self.tx.clone();
        let hwnd = self.hwnd;
        self.workers.push(thread::spawn(move || {
            if sender.send((thread::current().id(), operation())).is_ok() {
                orbment_ui::wake(HWND(hwnd as *mut _));
            }
        }));
    }
    fn start_recording(&mut self, request: RecordParams, ctx: &CoreContext) -> Result<()> {
        ensure!(self.recording.is_none(), "A recording is already running");
        ensure!(
            self.export_cancel.is_none(),
            "Wait for the recording export to finish"
        );
        ensure!(
            self.editor.is_none(),
            "Finish or cancel the recording editor before starting another recording"
        );
        ensure!(
            self.longshot_cancel.is_none(),
            "Stop the long screenshot before recording"
        );
        let encode_format = request
            .encode_format
            .unwrap_or(ctx.config.vision.movie.encode_format);
        ensure!(encode_format <= 2, "Unknown recording format");
        self.service()?.close_recording_hud();
        if request.region.is_none() && self.last.is_none() {
            self.service()?.begin_capture()?;
            self.pending_capture = Some((
                "vision.record_start".into(),
                json!({
                    "system_audio":request.system_audio, "microphone":request.microphone,
                    "quick":request.quick, "start_immediately":request.start_immediately,
                    "encode_format":encode_format,
                }),
            ));
            return Ok(());
        }
        let region = self.selected_rect(request.region)?;
        let system_audio = request.system_audio.unwrap_or(true);
        let microphone = request.microphone.unwrap_or(false);
        self.service()?
            .open_recording_hud(region, system_audio, microphone, encode_format)?;
        if !request.quick && !request.start_immediately {
            return Ok(());
        }
        let options = orbment_vision::recorder::RecordingOptions {
            region,
            fps: ctx.config.vision.movie.fps,
            bitrate: 8_000_000,
            system_audio,
            microphone,
            show_cursor: request.quick && ctx.config.vision.movie.key_mouse.record_mouse,
            record_mouse: ctx.config.vision.movie.key_mouse.record_mouse,
            record_keyboard: ctx.config.vision.movie.key_mouse.record_keyboard,
            quick: request.quick,
        };
        self.service()?.set_capture_excluded(true)?;
        let hwnd = self.hwnd;
        match orbment_vision::recorder::RecordingSession::start_with_wake(
            &ctx.data_dir.join("temp/recordings"),
            options,
            move || orbment_ui::wake(HWND(hwnd as *mut _)),
        ) {
            Ok(recording) => self.recording = Some(recording),
            Err(error) => {
                self.service()?.set_recording_active(false);
                self.service()?.set_capture_excluded(false)?;
                return Err(error);
            }
        }
        self.recording_quick = request.quick;
        self.recording_format = encode_format;
        self.service()?.set_recording_active(true);
        Ok(())
    }
    fn stop_recording(&mut self, request: ExportParams, ctx: &CoreContext) -> Result<()> {
        use orbment_vision::recorder::{EffectOptions, ExportFormat, TrimRange};
        self.service()?.close_recording_hud();
        let quick = self.recording_quick;
        let direct = quick
            || request.output_path.is_some()
            || request.start_100ns.is_some()
            || request.end_100ns.is_some();
        let format = if quick {
            ExportFormat::Mp4
        } else {
            match self.recording_format {
                0 => ExportFormat::Mp4,
                1 => ExportFormat::Gif,
                _ => ExportFormat::WebP,
            }
        };
        let extension = match format {
            ExportFormat::Mp4 => "mp4",
            ExportFormat::Gif => "gif",
            ExportFormat::WebP => "webp",
        };
        let path = match request.output_path {
            Some(path) => path,
            None => self.next_path(ctx, extension)?,
        };
        let recording = self.recording.take().context("No recording is active")?;
        let effects = EffectOptions::from(&ctx.config.vision.movie.key_mouse);
        let cancel = Arc::new(AtomicBool::new(false));
        self.export_cancel = Some(cancel.clone());
        if !direct {
            let close_after_export = ctx.config.vision.movie.exit_record_after_encode;
            self.spawn(move || Completion::Recorded {
                recording: Box::new(recording.stop()),
                path,
                format,
                effects,
                close_after_export,
            });
            return Ok(());
        }
        self.spawn(move || {
            Completion::Export((|| {
                let mut recording = recording.stop()?;
                if cancel.load(Ordering::Acquire) {
                    recording.cancel()?;
                    bail!("Recording export cancelled");
                }
                if quick {
                    recording.export_quick(&path)?;
                } else {
                    let trim = TrimRange {
                        start_100ns: request.start_100ns.unwrap_or(0),
                        end_100ns: request
                            .end_100ns
                            .unwrap_or_else(|| recording.duration_100ns()),
                    };
                    recording.export_with_cancel(&path, format, trim, &effects, &cancel)?;
                }
                Ok(path)
            })())
        });
        Ok(())
    }
    pub fn sync_recording_hooks(
        &mut self,
        hooks: &mut orbment_ui::HookThread,
        ctx: &CoreContext,
    ) -> Result<()> {
        if self.recording.is_none() || self.recording_quick {
            return self.release_recording_hooks(hooks);
        }
        let sink = self
            .recording
            .as_ref()
            .expect("active recording")
            .event_sink();
        if !ctx.config.vision.movie.key_mouse.record_mouse
            && let Some(id) = self.mouse_hook
        {
            hooks.release_mouse(id)?;
            self.mouse_hook = None;
        }
        if !ctx.config.vision.movie.key_mouse.record_keyboard
            && let Some(id) = self.keyboard_hook
        {
            hooks.release_keyboard(id)?;
            self.keyboard_hook = None;
        }
        if ctx.config.vision.movie.key_mouse.record_mouse && self.mouse_hook.is_none() {
            self.mouse_hook = Some(hooks.acquire_mouse(Box::new(RecordingMouse(sink.clone())))?);
        }
        if ctx.config.vision.movie.key_mouse.record_keyboard && self.keyboard_hook.is_none() {
            self.keyboard_hook = Some(hooks.acquire_keyboard(Box::new(move |event| {
                let modifiers = u8::from(event.modifiers.ctrl)
                    | (u8::from(event.modifiers.shift) << 1)
                    | (u8::from(event.modifiers.alt) << 2)
                    | (u8::from(event.modifiers.win) << 3);
                sink.record(orbment_vision::recorder::InputEvent::Key {
                    virtual_key: event.virtual_key,
                    scan_code: event.scan_code,
                    down: event.down,
                    modifiers,
                });
            }))?);
        }
        Ok(())
    }
    pub fn release_recording_hooks(&mut self, hooks: &mut orbment_ui::HookThread) -> Result<()> {
        let mut failure = None;
        if let Some(id) = self.mouse_hook {
            match hooks.release_mouse(id) {
                Ok(()) => self.mouse_hook = None,
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if let Some(id) = self.keyboard_hook {
            match hooks.release_keyboard(id) {
                Ok(()) => self.keyboard_hook = None,
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn start_longshot(&mut self, request: LongShotParams, ctx: &CoreContext) -> Result<()> {
        ensure!(
            self.longshot_cancel.is_none(),
            "A long screenshot is already running"
        );
        ensure!(
            self.recording.is_none(),
            "Stop recording before a long screenshot"
        );
        ensure!(
            self.export_cancel.is_none(),
            "Wait for the recording export to finish"
        );
        if request.region.is_none() && self.last.is_none() {
            self.service()?.begin_capture()?;
            self.pending_capture = Some((
                "vision.longshot".into(),
                json!({
                    "horizontal":request.horizontal, "window_messages":request.window_messages,
                }),
            ));
            return Ok(());
        }
        let region = self.selected_rect(request.region)?;
        self.service()?.set_capture_excluded(true)?;
        let options = AutoScrollOptions {
            region,
            target: Point {
                x: region.x + region.width / 2.0,
                y: region.y + region.height / 2.0,
            },
            axis: if request.horizontal {
                Axis::Horizontal
            } else {
                Axis::Vertical
            },
            channel: if request.window_messages {
                ScrollChannel::WindowMessage
            } else {
                ScrollChannel::Input
            },
            steps: if request.horizontal { 1 } else { -1 },
            settle: Duration::from_millis(350),
            max_frames: 200,
            auto_crop_on_scroll_back: ctx.config.vision.long_shot.auto_crop_on_scroll_back,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.longshot_cancel = Some(cancel.clone());
        self.spawn(move || {
            Completion::LongShot(
                region,
                orbment_vision::long_shot::capture_automatically(&options, &cancel, |_, _| {}),
            )
        });
        Ok(())
    }
    fn select_pin_text(&mut self, params: Value, ctx: &CoreContext) -> Result<()> {
        let id = if params.is_null() || params == json!({}) {
            self.last_pin.clone().context("Create a pin first")?
        } else {
            crate::parameters::<IdParams>(params)?.id
        };
        let revision = self.service()?.pin_image_revision(&id)?;
        if self.ocr_pins.get(&id) == Some(&revision) {
            return Ok(());
        }
        let image = self.service()?.pin_ocr_image_data(&id)?;
        let path = Self::ocr_input_path();
        let job = OcrJob {
            engine: ctx.ocr.resolve(&ctx.config.app.ocr_engine_tier)?,
            path,
            image: Some(image),
        };
        self.ocr_pins.insert(id.clone(), revision);
        let path = job.path.clone();
        self.pending_png_paths.insert(path.clone());
        self.spawn(move || Completion::PinOcr(id, revision, path, job.recognize()));
        Ok(())
    }
    fn annotate(&mut self, params: Value) -> Result<()> {
        if params.is_null() || params == json!({}) {
            let id = self.last_pin.clone().context("Create a pin first")?;
            return self.service()?.begin_pin_annotation(&id);
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Annotation {
            id: Option<String>,
            scene: orbment_vision::canvas_mark::Scene,
        }
        let request: Annotation = crate::parameters(params)?;
        let scene = request.scene;
        let (image, _) = self.last.as_mut().context("Capture an image first")?;
        let rendered = scene.render(image)?;
        let id = request.id.or_else(|| self.last_pin.clone());
        if let Some(id) = id {
            self.service()?.set_pin_scene(&id, scene)?;
        }
        self.last.as_mut().expect("capture exists").0 = rendered;
        Ok(())
    }
    pub fn poll(&mut self) -> (Vec<(String, Value)>, Vec<anyhow::Error>) {
        let mut actions = Vec::new();
        let mut errors = Vec::new();
        let mut pins_changed = false;
        if self
            .feedback
            .as_ref()
            .is_some_and(|window| !window.is_alive())
        {
            self.feedback = None;
        }
        if self
            .recording
            .as_ref()
            .is_some_and(|recording| recording.is_finished())
        {
            self.recording_quick = false;
            if let Some(service) = &mut self.service {
                service.close_recording_hud();
            }
            if let Some(recording) = self.recording.take()
                && let Err(error) = recording.cancel()
            {
                errors.push(error);
            }
            if let Err(error) = self
                .service()
                .and_then(|service| service.set_capture_excluded(false))
            {
                errors.push(error);
            }
        }
        if let Some(editor) = &mut self.editor {
            match editor.tick() {
                Ok(paths) => {
                    if !paths.is_empty() {
                        match orbment_ui::OverlayFloatingWindow::new(
                            self.graphics.clone(),
                            "录屏已保存",
                            2000,
                        ) {
                            Ok(feedback) => self.feedback = Some(feedback),
                            Err(error) => errors.push(error),
                        }
                    }
                    self.exported.extend(paths);
                }
                Err(error) => errors.push(error),
            }
            if editor.is_closed() {
                self.editor.take();
            }
        }
        if let Some(service) = &mut self.service {
            for event in service.poll() {
                match event {
                    VisionEvent::CaptureReady {
                        image,
                        rect,
                        action,
                    } => {
                        pins_changed |= action == CaptureAction::Pin;
                        let pin = if action == CaptureAction::Pin {
                            Some(service.pin_image(
                                image.clone(),
                                Point {
                                    x: rect.x,
                                    y: rect.y,
                                },
                            ))
                        } else {
                            None
                        };
                        self.last = Some((image, rect));
                        self.last_pin = None;
                        self.hidden_at_ms = None;
                        if let Some(pin) = pin {
                            match pin {
                                Ok(id) => self.last_pin = Some(id),
                                Err(error) => errors.push(error),
                            }
                        }
                        let pending = self.pending_capture.take();
                        if let Some(command) = capture_completion_action(action, pending) {
                            actions.push(
                                self.image_actions
                                    .bind(command, &self.last.as_ref().unwrap().0),
                            );
                        }
                    }
                    VisionEvent::CaptureCancelled => {
                        self.pending_capture = None;
                    }
                    VisionEvent::CaptureTextCopied { text, kind } => {
                        self.pending_capture = None;
                        let result = orbment_core::utilities::copy_text(self.hwnd as isize, &text)
                            .and_then(|()| {
                                let key = match kind {
                                    CaptureTextKind::Color => "vision.color_copied",
                                    CaptureTextKind::Coordinate => "vision.coordinate_copied",
                                };
                                orbment_ui::OverlayFloatingWindow::new(
                                    self.graphics.clone(),
                                    &orbment_core::i18n::tr(key),
                                    1400,
                                )
                            });
                        match result {
                            Ok(feedback) => self.feedback = Some(feedback),
                            Err(error) => errors.push(error),
                        }
                    }
                    VisionEvent::PinClosed(id) => {
                        pins_changed = true;
                        if self.last_pin.as_ref() == Some(&id) {
                            self.last_pin = None;
                        }
                    }
                    VisionEvent::Action(action) => actions.push((action, Value::Null)),
                    VisionEvent::PinAction { id, action } => {
                        actions
                            .push(("vision.pin_action".into(), json!({"id":id,"action":action})));
                    }
                    VisionEvent::RecordBegin {
                        region,
                        system_audio,
                        microphone,
                        encode_format,
                    } => actions.push((
                        "vision.record_start".into(),
                        json!({
                            "region":region, "system_audio":system_audio,
                            "microphone":microphone, "encode_format":encode_format,
                            "start_immediately":true,
                        }),
                    )),
                    VisionEvent::RecordPause(paused) => {
                        if let Some(recording) = &self.recording {
                            recording.set_paused(paused);
                        }
                    }
                    VisionEvent::RecordStop => {
                        service.close_recording_hud();
                        actions.push(("vision.record_stop".into(), Value::Null));
                    }
                    VisionEvent::RecordCancel => {
                        service.close_recording_hud();
                        actions.push(("vision.record_cancel".into(), Value::Null));
                    }
                    VisionEvent::Error(error) => errors.push(anyhow::anyhow!(error)),
                }
            }
        }
        while let Ok((worker_id, completion)) = self.rx.try_recv() {
            // After sending, this worker only posts a wake; its UI-owned handle can be retired.
            if let Some(index) = self
                .workers
                .iter()
                .position(|worker| worker.thread().id() == worker_id)
                && self.workers.swap_remove(index).join().is_err()
            {
                errors.push(anyhow::anyhow!("Vision worker panicked"));
            }
            let result = (|| -> Result<()> {
                match completion {
                    Completion::LongShot(rect, outcome) => {
                        let cancelled = self
                            .longshot_cancel
                            .take()
                            .is_some_and(|cancel| cancel.load(Ordering::Acquire));
                        if let Err(error) = self
                            .service()
                            .and_then(|service| service.set_capture_excluded(false))
                        {
                            errors.push(error);
                        }
                        let outcome = outcome?;
                        if !cancelled && outcome.action != LongShotAction::Cancel {
                            self.last_pin = if outcome.action == LongShotAction::Pin {
                                let id = self.service()?.pin_image(
                                    outcome.image.clone(),
                                    Point {
                                        x: rect.x,
                                        y: rect.y,
                                    },
                                )?;
                                pins_changed = true;
                                Some(id)
                            } else {
                                None
                            };
                            self.last = Some((outcome.image, rect));
                            self.hidden_at_ms = None;
                            match outcome.action {
                                LongShotAction::Copy => {
                                    actions.push(self.image_actions.bind(
                                        ("vision.copy_image".into(), Value::Null),
                                        &self.last.as_ref().unwrap().0,
                                    ));
                                }
                                LongShotAction::Save => {
                                    actions.push(self.image_actions.bind(
                                        ("vision.save_image".into(), Value::Null),
                                        &self.last.as_ref().unwrap().0,
                                    ));
                                }
                                LongShotAction::Pin | LongShotAction::Cancel => {}
                            }
                        }
                    }
                    Completion::PinOcr(id, revision, path, result) => {
                        self.pending_png_paths.remove(&path);
                        if self.ocr_pins.get(&id) != Some(&revision) {
                            return Ok(());
                        }
                        self.ocr_pins.remove(&id);
                        let service = self.service()?;
                        if service.list_pins().iter().any(|pin| pin.id == id)
                            && service.pin_image_revision(&id)? == revision
                        {
                            service.set_pin_ocr(&id, result?)?;
                        }
                    }
                    Completion::Ocr(path, result) => {
                        if let Some(path) = path {
                            self.pending_png_paths.remove(&path);
                        }
                        let result = result?;
                        self.last_pin = Some(self.service()?.pin_text(
                            result.text,
                            false,
                            Point { x: 80.0, y: 80.0 },
                        )?);
                        pins_changed = true;
                    }
                    Completion::Export(path) => {
                        self.export_cancel = None;
                        if let Err(error) = self
                            .service()
                            .and_then(|service| service.set_capture_excluded(false))
                        {
                            errors.push(error);
                        }
                        self.exported.push(path?);
                        self.feedback = Some(orbment_ui::OverlayFloatingWindow::new(
                            self.graphics.clone(),
                            "录屏已保存",
                            2000,
                        )?);
                    }
                    Completion::Recorded {
                        recording,
                        path,
                        format,
                        effects,
                        close_after_export,
                    } => {
                        let cancelled = self
                            .export_cancel
                            .take()
                            .is_some_and(|cancel| cancel.load(Ordering::Acquire));
                        if let Err(error) = self
                            .service()
                            .and_then(|service| service.set_capture_excluded(false))
                        {
                            errors.push(error);
                        }
                        let mut recording = (*recording)?;
                        if cancelled {
                            recording.cancel()?;
                        } else {
                            self.editor = Some(crate::recording_editor::RecordingEditor::new(
                                self.graphics.clone(),
                                HWND(self.hwnd as *mut _),
                                recording,
                                path,
                                format,
                                effects,
                                close_after_export,
                            )?);
                        }
                    }
                    Completion::Capture(path, result) => {
                        self.pending_png_paths.remove(&path);
                        result?;
                    }
                    Completion::QuickSaved(path, result) => {
                        self.pending_png_paths.remove(&path);
                        let path = result?;
                        tracing::info!(path=%path.display(), "Capture saved");
                        self.feedback = Some(orbment_ui::OverlayFloatingWindow::new(
                            self.graphics.clone(),
                            &orbment_core::i18n::tr("vision.quick_saved"),
                            1600,
                        )?);
                    }
                    Completion::QuickContext(result) => self.quick_contexts.push(result?),
                    Completion::RequestFinished(path) => {
                        if let Some(path) = path {
                            self.pending_png_paths.remove(&path);
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                errors.push(error);
            }
        }
        let mut index = 0;
        while index < self.workers.len() {
            if self.workers[index].is_finished() {
                if self.workers.swap_remove(index).join().is_err() {
                    errors.push(anyhow::anyhow!("Vision worker panicked"));
                }
            } else {
                index += 1;
            }
        }
        if pins_changed {
            actions.push(("vision.pin_history".into(), Value::Null));
        }
        (actions, errors)
    }
    fn image_ui_visible(&self) -> bool {
        self.service
            .as_ref()
            .is_some_and(VisionService::image_ui_visible)
            || self
                .editor
                .as_ref()
                .is_some_and(crate::recording_editor::RecordingEditor::is_visible)
    }
    pub fn sync_idle(&mut self, now_ms: u64) {
        if self.last.is_none()
            || self.image_ui_visible()
            || self.recording.is_some()
            || self.longshot_cancel.is_some()
            || self.pending_capture.is_some()
        {
            self.hidden_at_ms = None;
        } else {
            self.hidden_at_ms.get_or_insert(now_ms);
        }
    }
    pub fn clear_traces(&mut self, ctx: &CoreContext) -> Result<()> {
        orbment_vision::shell_menu::cleanup()?;
        self.shell_menu_cleaned = true;
        if let Some(service) = &mut self.service {
            for pin in service.list_pins() {
                service.close_pin(&pin.id)?;
            }
        }
        self.stop_service(ctx)?;
        let pins = ctx.data_dir.join("media/pins");
        if pins.exists() {
            std::fs::remove_dir_all(pins)?;
        }
        Ok(())
    }
    pub fn rebuild_graphics(&mut self, graphics: Rc<D2dContext>) -> Result<()> {
        self.feedback = None;
        if let Some(service) = &mut self.service {
            service.rebuild_graphics(graphics.clone())?;
        }
        if let Some(editor) = &self.editor {
            editor.rebuild_graphics(graphics.clone())?;
        }
        self.graphics = graphics;
        Ok(())
    }
}

impl OrbmentModule for VisionModule {
    fn id(&self) -> &'static str {
        "vision"
    }
    fn service_state(&self) -> ServiceState {
        self.state
    }
    fn ui_state(&self) -> UiSurfaceState {
        if self.image_ui_visible() {
            UiSurfaceState::Visible
        } else if let Some(hidden_at_ms) = self.hidden_at_ms {
            UiSurfaceState::HiddenIdlePending { hidden_at_ms }
        } else if self.last.is_some() {
            UiSurfaceState::HiddenResident
        } else {
            UiSurfaceState::Unloaded
        }
    }
    fn start_service(&mut self, ctx: &CoreContext) -> Result<()> {
        self.apply_config(ctx)
    }
    fn open_ui(&mut self, _: ActivateTrigger, ctx: &CoreContext) -> Result<()> {
        self.command("vision.capture", Value::Null, ctx).map(|_| ())
    }
    fn close_ui(&mut self, _: &CoreContext) -> Result<()> {
        if let Some(service) = &mut self.service {
            service.close_capture();
        }
        if let Some(editor) = &mut self.editor {
            editor.hide();
        }
        self.pending_capture = None;
        Ok(())
    }
    fn reclaim_idle_ui_and_cache(&mut self, _: &CoreContext) -> Result<()> {
        if matches!(self.ui_state(), UiSurfaceState::HiddenIdlePending { .. }) {
            self.last.take();
            self.hidden_at_ms = None;
        }
        Ok(())
    }
    fn stop_service(&mut self, ctx: &CoreContext) -> Result<()> {
        self.feedback = None;
        let mut failure = None;
        if let Some(mut editor) = self.editor.take()
            && let Err(error) = editor.shutdown()
        {
            failure.get_or_insert(error);
        }
        if let Some(cancel) = self.longshot_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        if let Some(cancel) = self.export_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        if let Some(recording) = self.recording.take()
            && let Err(error) = recording.cancel()
        {
            failure.get_or_insert(error);
        }
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                failure.get_or_insert_with(|| anyhow::anyhow!("Vision worker panicked"));
            }
        }
        while self.rx.try_recv().is_ok() {}
        self.image_actions.images.clear();
        if let Some(mut service) = self.service.take()
            && let Err(error) = service.cleanup()
        {
            failure.get_or_insert(error);
        }
        ctx.ocr.unregister_offline();
        self.offline.take();
        self.last.take();
        self.hidden_at_ms = None;
        self.quick_contexts.clear();
        self.last_pin.take();
        self.pending_capture = None;
        self.ocr_pins.clear();
        self.pending_png_paths.clear();
        self.state = ServiceState::Disabled;
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn capture_completion_action(
    action: CaptureAction,
    pending: Option<(String, Value)>,
) -> Option<(String, Value)> {
    let command = match action {
        CaptureAction::Confirm => {
            return Some(pending.unwrap_or_else(|| ("vision.copy_image".into(), Value::Null)));
        }
        CaptureAction::Copy => "vision.copy_image",
        CaptureAction::Pin => return None,
        CaptureAction::Save => "vision.save_image",
        CaptureAction::QuickSave => "vision.quick_save",
        CaptureAction::Longshot => "vision.longshot",
        CaptureAction::Record => "vision.record_start",
        CaptureAction::Ocr => "vision.ocr_show",
        CaptureAction::QuickAsk => "quick_ask.capture",
    };
    Some((command.into(), Value::Null))
}

fn parse_color(value: &str) -> Result<[u8; 3]> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    ensure!(hex.len() == 6 && hex.is_ascii(), "Color must be #RRGGBB");
    Ok([
        u8::from_str_radix(&hex[0..2], 16)?,
        u8::from_str_radix(&hex[2..4], 16)?,
        u8::from_str_radix(&hex[4..6], 16)?,
    ])
}

struct RecordingMouse(orbment_vision::recorder::EventSink);
impl orbment_ui::MouseHookHandler for RecordingMouse {
    fn on_mouse(&mut self, event: orbment_ui::MouseHookEvent) -> bool {
        use orbment_ui::{MouseButton, MouseEventKind};
        use orbment_vision::recorder::{InputEvent, PointerAction, PointerButton};
        let button = |value| match value {
            MouseButton::Left => PointerButton::Left,
            MouseButton::Right => PointerButton::Right,
            MouseButton::Middle => PointerButton::Middle,
            MouseButton::X1 => PointerButton::X1,
            MouseButton::X2 => PointerButton::X2,
        };
        let action = match event.kind {
            MouseEventKind::Move => PointerAction::Move,
            MouseEventKind::Down(value) => PointerAction::Down(button(value)),
            MouseEventKind::Up(value) => PointerAction::Up(button(value)),
            MouseEventKind::Wheel { delta, horizontal } => {
                PointerAction::Wheel { delta, horizontal }
            }
        };
        self.0.record(InputEvent::Pointer {
            x: event.x as f64,
            y: event.y as f64,
            action,
        });
        false
    }
}

pub fn copy_recording_file(hwnd: HWND, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL},
        System::{
            DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        },
    };
    let path = std::fs::canonicalize(path)?;
    let mut data = vec![0_u8; 20];
    data[0..4].copy_from_slice(&20_u32.to_le_bytes());
    data[16..20].copy_from_slice(&1_u32.to_le_bytes());
    for unit in path.as_os_str().encode_wide().chain([0, 0]) {
        data.extend_from_slice(&unit.to_le_bytes());
    }
    struct Memory(HGLOBAL);
    impl Drop for Memory {
        fn drop(&mut self) {
            unsafe {
                let _ = GlobalFree(Some(self.0));
            }
        }
    }
    struct Open;
    impl Drop for Open {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }
    unsafe {
        let memory = Memory(GlobalAlloc(GMEM_MOVEABLE, data.len())?);
        let pointer = GlobalLock(memory.0);
        ensure!(!pointer.is_null(), "无法准备录屏文件的剪贴板数据");
        std::ptr::copy_nonoverlapping(data.as_ptr(), pointer.cast(), data.len());
        let _ = GlobalUnlock(memory.0);
        OpenClipboard(Some(hwnd))?;
        let _clipboard = Open;
        EmptyClipboard()?;
        SetClipboardData(15, Some(HANDLE(memory.0.0)))?;
        std::mem::forget(memory);
    }
    Ok(())
}

enum ClipboardPin {
    Image(CapturedImage),
    Text(String),
}

fn read_clipboard_pin(hwnd: HWND) -> Result<Option<ClipboardPin>> {
    use windows::Win32::{
        Foundation::HGLOBAL,
        Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, GetDIBits,
            GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
        },
        System::{
            DataExchange::{
                CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
                RegisterClipboardFormatW,
            },
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        },
    };
    struct Open;
    impl Drop for Open {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }
    fn bytes(format: u32, limit: usize) -> Result<Vec<u8>> {
        unsafe {
            let memory = HGLOBAL(GetClipboardData(format)?.0);
            let size = GlobalSize(memory);
            ensure!(size > 0 && size <= limit, "剪贴板内容为空或超过大小限制");
            let pointer = GlobalLock(memory);
            ensure!(!pointer.is_null(), "无法读取剪贴板内容");
            let data = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
            let _ = GlobalUnlock(memory);
            Ok(data)
        }
    }
    unsafe {
        OpenClipboard(Some(hwnd)).context("剪贴板正被占用，请稍后重试")?;
        let _clipboard = Open;
        let png = RegisterClipboardFormatW(windows::core::w!("PNG"));
        ensure!(png != 0, "无法注册 PNG 剪贴板格式");
        if IsClipboardFormatAvailable(png).is_ok() {
            use std::io::Write;
            let data = bytes(png, 128 * 1024 * 1024)?;
            let path = VisionModule::ocr_input_path();
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            let _cleanup = OcrInputFile(path.clone());
            file.write_all(&data)?;
            drop(file);
            return Ok(Some(ClipboardPin::Image(CapturedImage::load(&path)?)));
        }
        // Windows synthesizes CF_BITMAP from DIB/DIBV5, including bottom-up and indexed DIBs.
        if IsClipboardFormatAvailable(2).is_ok() {
            let bitmap = HBITMAP(GetClipboardData(2)?.0);
            let mut metadata = BITMAP::default();
            ensure!(
                GetObjectW(
                    HGDIOBJ(bitmap.0),
                    std::mem::size_of::<BITMAP>() as i32,
                    Some((&mut metadata as *mut BITMAP).cast())
                ) != 0,
                "无法读取剪贴板位图信息"
            );
            let width = u32::try_from(metadata.bmWidth)?;
            let height = u32::try_from(metadata.bmHeight)?;
            let length = u64::from(width) * u64::from(height) * 4;
            ensure!(
                width > 0 && height > 0 && length <= 128 * 1024 * 1024,
                "剪贴板图片为空或超过 128 MiB"
            );
            let mut image = CapturedImage {
                width,
                height,
                pixels: vec![0; length as usize],
            };
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: metadata.bmWidth,
                    biHeight: -metadata.bmHeight,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let dc = GetDC(None);
            ensure!(!dc.0.is_null(), "无法读取屏幕设备上下文");
            let rows = GetDIBits(
                dc,
                bitmap,
                0,
                height,
                Some(image.pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            let _ = ReleaseDC(None, dc);
            ensure!(rows == height as i32, "剪贴板图片读取不完整");
            for pixel in image.pixels.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
            return Ok(Some(ClipboardPin::Image(image)));
        }
        if IsClipboardFormatAvailable(13).is_ok() {
            let data = bytes(13, 4 * 1024 * 1024)?;
            let units: Vec<_> = data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .take_while(|unit| *unit != 0)
                .collect();
            let text = String::from_utf16_lossy(&units);
            if !text.is_empty() {
                return Ok(Some(ClipboardPin::Text(text)));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_completion_defaults_to_copy_and_never_implicitly_pins() {
        assert_eq!(
            capture_completion_action(CaptureAction::Confirm, None),
            Some(("vision.copy_image".into(), Value::Null))
        );
        let pending = Some(("vision.ocr_show".into(), Value::Null));
        assert_eq!(
            capture_completion_action(CaptureAction::Copy, pending.clone()),
            Some(("vision.copy_image".into(), Value::Null))
        );
        assert_eq!(
            capture_completion_action(CaptureAction::Confirm, pending.clone()),
            pending
        );
        assert_eq!(capture_completion_action(CaptureAction::Pin, None), None);
        assert_eq!(
            capture_completion_action(
                CaptureAction::Save,
                Some(("vision.ocr_show".into(), Value::Null))
            ),
            Some(("vision.save_image".into(), Value::Null)),
        );
        assert_eq!(
            capture_completion_action(CaptureAction::QuickSave, pending),
            Some(("vision.quick_save".into(), Value::Null)),
        );
    }
    #[test]
    fn stale_pin_ocr_completion_preserves_the_replacement_request() {
        let mut module = module();
        module.ocr_pins.insert("pin".into(), 2);
        let old_path = PathBuf::from("old-source.png");
        module.pending_png_paths.insert(old_path.clone());
        module
            .tx
            .send((
                thread::current().id(),
                Completion::PinOcr(
                    "pin".into(),
                    1,
                    old_path.clone(),
                    Err(anyhow::anyhow!("obsolete")),
                ),
            ))
            .unwrap();
        let (actions, errors) = module.poll();
        assert!(actions.is_empty());
        assert!(errors.is_empty());
        assert_eq!(module.ocr_pins.get("pin"), Some(&2));
        assert!(!module.pending_png_paths.contains(&old_path));
    }

    #[test]
    fn record_selection_arms_the_hud_unless_direct_start_is_requested() {
        let (action, params) = capture_completion_action(CaptureAction::Record, None).unwrap();
        assert_eq!(action, "vision.record_start");
        let request: RecordParams = crate::parameters(params).unwrap();
        assert!(!request.quick);
        assert!(!request.start_immediately);
        let direct: RecordParams = crate::parameters(json!({
            "start_immediately":true, "encode_format":1, "system_audio":false,
            "microphone":true,
        }))
        .unwrap();
        assert!(direct.start_immediately);
        assert_eq!(direct.encode_format, Some(1));
        assert_eq!(direct.system_audio, Some(false));
        assert_eq!(direct.microphone, Some(true));
    }

    fn image() -> CapturedImage {
        CapturedImage {
            width: 1,
            height: 1,
            pixels: vec![12, 34, 56, 255],
        }
    }

    fn module() -> VisionModule {
        VisionModule::new(Rc::new(D2dContext::new().unwrap()), HWND::default())
    }

    #[test]
    fn quick_save_writes_template_named_png_without_overwriting_previous_outputs() {
        let root = VisionModule::ocr_input_path().with_extension("captures");
        let mut ctx = CoreContext {
            config: Default::default(),
            data_dir: root.clone(),
            ocr: Arc::new(Default::default()),
        };
        ctx.config.vision.save.manual_save_pattern = "quick-output".into();
        let captures = root.join("media/captures");
        std::fs::create_dir_all(&captures).unwrap();
        let existing = captures.join("quick-output.png");
        std::fs::write(&existing, b"preserved").unwrap();
        let mut module = module();
        module.state = ServiceState::Running;
        module.last = Some((image(), Rect::default()));
        let mut outputs = Vec::new();
        for _ in 0..2 {
            let result = module
                .command("vision.quick_save", Value::Null, &ctx)
                .unwrap();
            assert_eq!(result["pending"], true);
            assert!(result.get("path").is_none());
        }
        for _ in 0..2 {
            let (_, completion) = module.rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let Completion::QuickSaved(_, result) = completion else {
                panic!("Expected a quick-save completion");
            };
            let path = result.unwrap();
            assert_eq!(path.parent(), Some(captures.as_path()));
            assert_eq!(CapturedImage::load(&path).unwrap().pixels, image().pixels);
            outputs.push(path);
        }
        assert_ne!(outputs[0], outputs[1]);
        for worker in module.workers.drain(..) {
            worker.join().unwrap();
        }
        assert_eq!(std::fs::read(existing).unwrap(), b"preserved");
        ctx.config.vision.save.manual_save_pattern = "folder/outside".into();
        module
            .command("vision.quick_save", Value::Null, &ctx)
            .unwrap();
        let (_, completion) = module.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let Completion::QuickSaved(_, result) = completion else {
            panic!("Expected a sanitized quick-save completion");
        };
        assert_eq!(result.unwrap().parent(), Some(captures.as_path()));
        for worker in module.workers.drain(..) {
            worker.join().unwrap();
        }
        ctx.config.vision.save.manual_save_pattern = "../outside".into();
        assert!(
            module
                .command("vision.quick_save", Value::Null, &ctx)
                .is_err()
        );
        assert!(!root.join("media/outside.png").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn global_pin_hotkey_is_owned_by_the_active_longshot() {
        let mut module = module();
        module.state = ServiceState::Running;
        module.longshot_cancel = Some(Arc::new(AtomicBool::new(false)));
        let ctx = CoreContext {
            config: Default::default(),
            data_dir: std::env::temp_dir(),
            ocr: Arc::new(Default::default()),
        };
        assert!(module.command("vision.pin_last", Value::Null, &ctx).is_ok());
        assert!(module.last_pin.is_none());
    }

    #[test]
    fn longshot_cancel_discards_image_and_late_cancel_wins_over_copy() {
        for (action, cancelled) in [
            (LongShotAction::Cancel, false),
            (LongShotAction::Copy, true),
        ] {
            let mut module = module();
            let original_rect = Rect {
                x: -40.0,
                y: 20.0,
                width: 1.0,
                height: 1.0,
            };
            module.last = Some((image(), original_rect));
            module.last_pin = Some("previous-pin".into());
            module.longshot_cancel = Some(Arc::new(AtomicBool::new(cancelled)));
            let mut discarded = image();
            discarded.pixels[0] = 200;
            module
                .tx
                .send((
                    thread::current().id(),
                    Completion::LongShot(
                        Rect::default(),
                        Ok(LongShotOutcome {
                            image: discarded,
                            action,
                        }),
                    ),
                ))
                .unwrap();
            let (actions, errors) = module.poll();
            assert!(actions.is_empty());
            assert_eq!(errors.len(), 1); // No service is installed by this isolated module.
            assert!(module.longshot_cancel.is_none());
            let (retained, rect) = module.last.as_ref().unwrap();
            assert_eq!(retained.pixels, image().pixels);
            assert_eq!(rect.x, original_rect.x);
            assert_eq!(module.last_pin.as_deref(), Some("previous-pin"));
        }
    }

    #[test]
    fn longshot_outputs_keep_their_image_when_other_completions_change_the_active_pin() {
        for (action, command) in [
            (LongShotAction::Save, "vision.save_image"),
            (LongShotAction::Copy, "vision.copy_image"),
        ] {
            let mut module = module();
            let stitched = CapturedImage {
                width: 1,
                height: 2,
                pixels: vec![255; 8],
            };
            module.longshot_cancel = Some(Arc::new(AtomicBool::new(false)));
            module
                .tx
                .send((
                    thread::current().id(),
                    Completion::LongShot(
                        Rect::default(),
                        Ok(LongShotOutcome {
                            image: stitched,
                            action,
                        }),
                    ),
                ))
                .unwrap();
            let (actions, errors) = module.poll();
            assert_eq!(errors.len(), 1);
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].0, command);
            assert!(module.longshot_cancel.is_none());
            module.last = Some((image(), Rect::default()));
            module.last_pin = Some("newer-text-pin".into());
            let (selected, pin) = module.selected_image(actions[0].1.clone()).unwrap();
            assert_eq!((selected.width, selected.height), (1, 2));
            assert_eq!(selected.pixels, vec![255; 8]);
            assert!(pin.is_none());
            assert!(module.selected_image(actions[0].1.clone()).is_err());
        }
    }

    #[test]
    fn deferred_quick_save_owns_the_capture_instead_of_the_next_selected_image() {
        let mut module = module();
        let command = capture_completion_action(CaptureAction::QuickSave, None).unwrap();
        let command = module.image_actions.bind(command, &image());
        module.last_pin = Some("unrelated-pin".into());
        let (selected, pin) = module.selected_image(command.1).unwrap();
        assert_eq!(selected.pixels, image().pixels);
        assert!(pin.is_none());
    }

    #[test]
    fn deferred_ocr_and_quickask_keep_the_capture_when_a_longshot_completes() {
        for (action, pending) in [
            (CaptureAction::Ocr, None),
            (CaptureAction::QuickAsk, None),
            (
                CaptureAction::Confirm,
                Some(("vision.ocr_show".into(), Value::Null)),
            ),
            (
                CaptureAction::Confirm,
                Some(("quick_ask.capture".into(), Value::Null)),
            ),
        ] {
            let mut module = module();
            module.state = ServiceState::Running;
            module.last = Some((image(), Rect::default()));
            let command = capture_completion_action(action, pending).unwrap();
            let (command, params) = module.image_actions.bind(command, &image());
            let stitched = CapturedImage {
                width: 1,
                height: 2,
                pixels: vec![255; 8],
            };
            module
                .tx
                .send((
                    thread::current().id(),
                    Completion::LongShot(
                        Rect::default(),
                        Ok(LongShotOutcome {
                            image: stitched.clone(),
                            action: LongShotAction::Copy,
                        }),
                    ),
                ))
                .unwrap();
            let (actions, errors) = module.poll();
            assert_eq!(errors.len(), 1); // This fixture has no capture-overlay service.
            assert_eq!(module.last.as_ref().unwrap().0.pixels, stitched.pixels);
            assert_eq!(actions.len(), 1);
            let (selected, _) = module.selected_image(actions[0].1.clone()).unwrap();
            assert_eq!(selected.pixels, stitched.pixels);
            module.last_pin = Some("unrelated-pin".into());

            if command == "vision.ocr_show" {
                let mut ctx = CoreContext {
                    config: Default::default(),
                    data_dir: std::env::temp_dir(),
                    ocr: Arc::new(Default::default()),
                };
                ctx.config.app.ocr_engine_tier = "OfflineOnnx".into();
                ctx.ocr
                    .register_offline(Arc::new(ImageOcr(AtomicBool::new(false))));
                module.command(&command, params, &ctx).unwrap();
                let (_, completion) = module.rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let Completion::Ocr(_, result) = completion else {
                    panic!("Expected OCR from the bound capture");
                };
                assert_eq!(result.unwrap().text, "recognized");
            } else {
                module.request_quick_context(params).unwrap();
                let (_, completion) = module.rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let Completion::QuickContext(result) = completion else {
                    panic!("Expected QuickAsk context from the bound capture");
                };
                assert_eq!(
                    result.unwrap().image.unwrap().bytes,
                    image().png_bytes().unwrap()
                );
            }
            assert!(module.image_actions.images.is_empty());
            for worker in module.workers.drain(..) {
                worker.join().unwrap();
            }
        }
    }

    struct ImageOcr(AtomicBool);
    impl OcrEngine for ImageOcr {
        fn recognize(&self, path: &Path) -> Result<OcrResult> {
            assert_eq!(CapturedImage::load(path)?.pixels, image().pixels);
            self.0.store(true, Ordering::Release);
            Ok(OcrResult {
                text: "recognized".into(),
                ..Default::default()
            })
        }
    }

    #[test]
    fn manual_ocr_uses_router_fallback_and_requires_choice_only_when_available() {
        let mut module = module();
        module.state = ServiceState::Running;
        module.last = Some((image(), Rect::default()));
        let mut ctx = CoreContext {
            config: Default::default(),
            data_dir: std::env::temp_dir(),
            ocr: Arc::new(Default::default()),
        };
        ctx.config.app.ocr_engine_tier = "Ask".into();
        for params in [
            Value::Null,
            json!({"engine":"Ask"}),
            json!({"engine":"OfflineOnnx"}),
            json!({"engine":"WindowsNative"}),
        ] {
            let job = module.prepare_ocr(params, &ctx).unwrap();
            assert_eq!(job.image.unwrap().pixels, image().pixels);
        }
        let path = PathBuf::from("external-image.png");
        let external = module
            .prepare_ocr(json!({"image_path":path}), &ctx)
            .unwrap();
        assert_eq!(external.path, path);
        assert!(external.image.is_none());
        assert!(
            module
                .prepare_ocr(json!({"engine":"invalid"}), &ctx)
                .is_err()
        );
        let offline: Arc<dyn OcrEngine> = Arc::new(ImageOcr(AtomicBool::new(false)));
        ctx.ocr.register_offline(offline.clone());
        assert!(module.prepare_ocr(Value::Null, &ctx).is_err());
        let chosen = module
            .prepare_ocr(json!({"engine":"OfflineOnnx"}), &ctx)
            .unwrap();
        assert!(Arc::ptr_eq(&chosen.engine, &offline));
    }

    #[test]
    fn ocr_job_encodes_before_recognition_and_propagates_encoding_failure() {
        let path = std::env::temp_dir().join(format!(
            "orbment-ocr-job-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = Arc::new(ImageOcr(AtomicBool::new(false)));
        let job = OcrJob {
            engine: engine.clone(),
            path: path.clone(),
            image: Some(image()),
        };
        assert!(!path.exists());
        let result = thread::spawn(move || job.run()).join().unwrap();
        assert!(!path.exists());
        assert_eq!(result.unwrap()["text"], "recognized");
        assert!(engine.0.load(Ordering::Acquire));

        engine.0.store(false, Ordering::Release);
        let failed = OcrJob {
            engine: engine.clone(),
            path: std::env::temp_dir(),
            image: Some(image()),
        };
        assert!(thread::spawn(move || failed.run()).join().unwrap().is_err());
        assert!(!engine.0.load(Ordering::Acquire));
    }

    #[test]
    fn ocr_input_cleanup_preserves_external_and_preexisting_files() {
        struct FailedOcr;
        impl OcrEngine for FailedOcr {
            fn recognize(&self, path: &Path) -> Result<OcrResult> {
                assert!(path.is_file());
                anyhow::bail!("recognition failed")
            }
        }
        let path = VisionModule::ocr_input_path();
        let generated = OcrJob {
            engine: Arc::new(FailedOcr),
            path: path.clone(),
            image: Some(image()),
        };
        assert!(generated.run().is_err());
        assert!(!path.exists());

        image().save_png(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        for generated in [false, true] {
            let job = OcrJob {
                engine: Arc::new(FailedOcr),
                path: path.clone(),
                image: generated.then(image),
            };
            assert!(job.run().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        let external = OcrJob {
            engine: Arc::new(ImageOcr(AtomicBool::new(false))),
            path: path.clone(),
            image: None,
        };
        assert!(external.run().is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn capture_request_reports_write_failure_and_retires_completed_worker() {
        let mut module = module();
        let (reply, response) = mpsc::channel();
        let pending = orbment_core::ipc::PendingRequest::new(
            orbment_core::ipc::ActionRequest::new("vision.capture_screen", Value::Null),
            Arc::new(AtomicBool::new(false)),
            move |result| {
                let _ = reply.send(result);
            },
        );
        module.request_capture(
            CaptureJob {
                image: image(),
                path: std::env::temp_dir(),
                result: json!({"ok":true}),
            },
            pending,
        );
        assert!(
            response
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_err()
        );
        let completion = module.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(module.tx.send(completion).is_ok());
        let (actions, errors) = module.poll();
        assert!(actions.is_empty());
        assert!(errors.is_empty());
        assert!(module.workers.is_empty());
        assert!(module.pending_png_paths.is_empty());
    }

    #[test]
    fn poll_preserves_actions_and_drains_completions_across_errors() {
        let mut module = module();
        module.last = Some((image(), Rect::default()));
        let region = Rect {
            x: 42.0,
            y: 73.0,
            width: 100.0,
            height: 200.0,
        };
        let context = QuickContext {
            text: "after errors".into(),
            source_title: String::new(),
            image: None,
        };
        for completion in [
            Completion::Capture(PathBuf::from("first.png"), Err(anyhow::anyhow!("first"))),
            Completion::LongShot(
                region,
                Ok(LongShotOutcome {
                    image: image(),
                    action: LongShotAction::Copy,
                }),
            ),
            Completion::Capture(PathBuf::from("last.png"), Err(anyhow::anyhow!("last"))),
            Completion::QuickContext(Ok(context)),
        ] {
            assert!(module.tx.send((thread::current().id(), completion)).is_ok());
        }
        let (actions, errors) = module.poll();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].0, "vision.copy_image");
        assert_eq!(
            module
                .selected_image(actions[0].1.clone())
                .unwrap()
                .0
                .pixels,
            image().pixels
        );
        let actual = module.last.as_ref().unwrap().1;
        assert_eq!(
            (actual.x, actual.y, actual.width, actual.height),
            (region.x, region.y, region.width, region.height)
        );
        let errors: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert_eq!(errors, ["first", "Vision module is unavailable", "last"]);
        assert_eq!(module.take_quick_contexts()[0].text, "after errors");
        let (actions, errors) = module.poll();
        assert!(actions.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn last_image_idle_deadline_uses_shared_clock_and_resets_for_active_capture() {
        let mut module = module();
        module.last = Some((image(), Rect::default()));
        module.sync_idle(10_000);
        module.sync_idle(11_000);
        assert_eq!(
            module.ui_state(),
            UiSurfaceState::HiddenIdlePending {
                hidden_at_ms: 10_000
            }
        );
        module.longshot_cancel = Some(Arc::new(AtomicBool::new(false)));
        module.sync_idle(12_000);
        assert_eq!(module.ui_state(), UiSurfaceState::HiddenResident);
        module.longshot_cancel = None;
        module.sync_idle(13_000);
        assert_eq!(
            module.ui_state(),
            UiSurfaceState::HiddenIdlePending {
                hidden_at_ms: 13_000
            }
        );
    }
}
