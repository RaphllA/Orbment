mod popup;
pub use popup::TransientPopupWindow;
mod search_popup;
pub use search_popup::SearchPopupWindow;
mod panel;
pub use panel::{PopupPanel, TransientPanelWindow};
mod preview;
pub use preview::render_ui_previews;

use crate::{
    D2dContext,
    ime::{ImeInput, Imm32},
    mesh_renderer::MeshRenderer,
    settings::SettingsView,
    tray::application_icon,
    ui_popup::{ClipboardGroup, ClipboardPopupEvent, ClipboardRow, ClipboardView, PopupCommand},
};
use anyhow::{Context as _, Result, ensure};
use egui::{Event, ImeEvent, Key, Modifiers};
use orbment_core::{config::ClipboardConfig, lifecycle::UiSurfaceState};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{Common::*, *},
            Direct3D11::*,
            Dwm::*,
            Dxgi::{Common::*, *},
            Gdi::*,
        },
        System::{DataExchange::*, LibraryLoader::GetModuleHandleW, Memory::*},
        UI::{
            Controls::WM_MOUSELEAVE, HiDpi::GetDpiForWindow, Input::KeyboardAndMouse::*, Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::{Interface, PCWSTR, w},
};

const REPAINT: u32 = WM_APP + 21;
const REPAINT_TIMER: usize = 21;
type FontBytes = std::result::Result<Arc<[u8]>, String>;
static FONT_BYTES: LazyLock<FontBytes> = LazyLock::new(|| SharedFonts::read("msyh.ttc"));
static BOLD_FONT_BYTES: LazyLock<FontBytes> = LazyLock::new(|| SharedFonts::read("msyhbd.ttc"));

pub struct SharedFonts;
impl SharedFonts {
    fn read(file: &str) -> FontBytes {
        let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is unavailable")?;
        std::fs::read(PathBuf::from(root).join("Fonts").join(file))
            .map(Arc::from)
            .map_err(|error| format!("Load system font {file}: {error}"))
    }

    pub(crate) fn install(context: &egui::Context) -> Result<()> {
        // The process-owned Arc provides a genuine static borrow, with no per-window font copy.
        let bytes = FONT_BYTES
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        // Some Windows editions ship YaHei without the bold face; headings then use the regular one.
        let bold = BOLD_FONT_BYTES.as_ref().unwrap_or(bytes);
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "orbment-shared".into(),
            Arc::new(egui::FontData::from_static(bytes.as_ref())),
        );
        fonts.font_data.insert(
            "orbment-shared-bold".into(),
            Arc::new(egui::FontData::from_static(bold.as_ref())),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "orbment-shared".into());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("orbment-shared".into());
        let mut bold_chain = vec!["orbment-shared-bold".to_owned()];
        bold_chain.extend(
            fonts.families[&egui::FontFamily::Proportional]
                .iter()
                .skip(1)
                .cloned(),
        );
        fonts.families.insert(
            egui::FontFamily::Name(crate::theme::BOLD_FAMILY.into()),
            bold_chain,
        );
        context.set_fonts(fonts);
        Ok(())
    }
}

#[derive(Clone)]
pub enum SettingsEvent {
    ConfigChanged {
        path: String,
        value: Value,
    },
    Action {
        action: String,
        params: Value,
    },
    FileTools(crate::FileToolsAction),
    QuickAskModels {
        config: Box<orbment_core::config::QuickAskConfig>,
        api_key_override: Option<String>,
        result: std::sync::mpsc::Sender<Result<Vec<String>, String>>,
    },
    Closed,
    FilesDropped(Vec<PathBuf>),
    DeviceLost,
    Error(String),
}

impl std::fmt::Debug for SettingsEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigChanged { path, value } => formatter
                .debug_struct("ConfigChanged")
                .field("path", path)
                .field("value", value)
                .finish(),
            Self::Action { action, params } => formatter
                .debug_struct("Action")
                .field("action", action)
                .field("params", params)
                .finish(),
            Self::FileTools(action) => formatter.debug_tuple("FileTools").field(action).finish(),
            Self::QuickAskModels { config, .. } => formatter
                .debug_struct("QuickAskModels")
                .field("provider", &config.provider)
                .finish_non_exhaustive(),
            Self::Closed => formatter.write_str("Closed"),
            Self::FilesDropped(paths) => {
                formatter.debug_tuple("FilesDropped").field(paths).finish()
            }
            Self::DeviceLost => formatter.write_str("DeviceLost"),
            Self::Error(error) => formatter.debug_tuple("Error").field(error).finish(),
        }
    }
}

/// Called after egui and before the one Present, clipped in logical pixels.
pub type CanvasPainter = Box<dyn FnMut(&ID2D1DeviceContext) -> Result<()>>;

struct Targets {
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    d2d: ID2D1DeviceContext,
    bitmap: Option<ID2D1Bitmap1>,
    size: [u32; 2],
}
impl Targets {
    fn new(graphics: &D2dContext, hwnd: HWND, size: [u32; 2]) -> Result<Self> {
        unsafe {
            let dxgi: IDXGIDevice = graphics.d3d_device().cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let swapchain = factory.CreateSwapChainForHwnd(
                graphics.d3d_device(),
                hwnd,
                &DXGI_SWAP_CHAIN_DESC1 {
                    Width: size[0],
                    Height: size[1],
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                    ..Default::default()
                },
                None,
                None,
            )?;
            factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)?;
            let mut result = Self {
                swapchain,
                rtv: None,
                d2d: graphics
                    .d2d_device()
                    .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?,
                bitmap: None,
                size,
            };
            result.bind(graphics)?;
            Ok(result)
        }
    }

    fn bind(&mut self, graphics: &D2dContext) -> Result<()> {
        unsafe {
            let texture: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            graphics
                .d3d_device()
                .CreateRenderTargetView(&texture, None, Some(&mut self.rtv))?;
            let surface: IDXGISurface = texture.cast()?;
            let bitmap = self.d2d.CreateBitmapFromDxgiSurface(
                &surface,
                Some(&D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_IGNORE,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    ..Default::default()
                }),
            )?;
            self.d2d.SetTarget(&bitmap);
            self.bitmap = Some(bitmap);
        }
        Ok(())
    }

    fn resize(&mut self, graphics: &D2dContext, size: [u32; 2]) -> Result<()> {
        if self.size == size {
            return Ok(());
        }
        unsafe {
            graphics.immediate_context().ClearState();
            self.d2d.SetTarget(None);
            self.bitmap = None;
            self.rtv = None;
            self.swapchain.ResizeBuffers(
                0,
                size[0],
                size[1],
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }
        self.size = size;
        self.bind(graphics)
    }
}
impl Drop for Targets {
    fn drop(&mut self) {
        unsafe {
            self.d2d.SetTarget(None);
        }
    }
}

enum ViewBody {
    Settings(Box<SettingsView>),
    Clipboard(Box<ClipboardView>),
    Search(Box<crate::file_tools::SearchView>),
    Panel(Rc<RefCell<Box<dyn PopupPanel>>>),
}
impl ViewBody {
    fn settings(&self) -> Option<&SettingsView> {
        match self {
            Self::Settings(view) => Some(view),
            Self::Clipboard(_) | Self::Search(_) | Self::Panel(_) => None,
        }
    }
    fn settings_mut(&mut self) -> Option<&mut SettingsView> {
        match self {
            Self::Settings(view) => Some(view),
            Self::Clipboard(_) | Self::Search(_) | Self::Panel(_) => None,
        }
    }
}

struct PopupLifecycle {
    config: RefCell<ClipboardConfig>,
    events: RefCell<Vec<ClipboardPopupEvent>>,
    hidden: Cell<Option<Instant>>,
    last_active: Cell<HWND>,
    position: Cell<Option<POINT>>,
    fixed: Cell<bool>,
    start: Instant,
    search: bool,
    panel: bool,
}

struct View {
    graphics: Rc<D2dContext>,
    context: egui::Context,
    renderer: MeshRenderer,
    targets: Option<Targets>,
    body: ViewBody,
    canvas: Option<(egui::Rect, CanvasPainter)>,
    device_lost: bool,
}
struct Host {
    hwnd: Cell<HWND>,
    wake_hwnd: HWND,
    view: RefCell<Option<View>>,
    input: RefCell<egui::RawInput>,
    ime: Imm32,
    events: RefCell<Vec<SettingsEvent>>,
    start: Instant,
    deadline: Cell<Option<Instant>>,
    cursor: Cell<egui::CursorIcon>,
    zoom: Cell<f32>,
    file_tools_revision: Cell<Option<u64>>,
    settings_state: RefCell<Option<(String, bool)>>,
    popup: Option<Rc<PopupLifecycle>>,
}

pub struct MainHostWindow(Rc<Host>);
impl MainHostWindow {
    pub fn new(graphics: Rc<D2dContext>, config: Value, wake_hwnd: HWND) -> Result<Self> {
        Self::create(
            graphics,
            ViewBody::Settings(Box::new(SettingsView::new(config))),
            wake_hwnd,
            None,
        )
    }

    fn create(
        graphics: Rc<D2dContext>,
        body: ViewBody,
        wake_hwnd: HWND,
        popup: Option<Rc<PopupLifecycle>>,
    ) -> Result<Self> {
        let host = Self::build(graphics, body, wake_hwnd, popup)?;
        if host.0.popup.is_none() {
            host.show();
        }
        Ok(host)
    }

    /// Creates the native surface and view without showing it.
    fn build(
        graphics: Rc<D2dContext>,
        body: ViewBody,
        wake_hwnd: HWND,
        popup: Option<Rc<PopupLifecycle>>,
    ) -> Result<Self> {
        let (instance, icon, small_icon) = unsafe {
            let instance = GetModuleHandleW(None)?;
            let icon = application_icon(GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON))?;
            let small_icon =
                application_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))?;
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(host_proc),
                hInstance: instance.into(),
                hIcon: icon,
                hIconSm: small_icon,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: w!("Orbment.MainHost"),
                ..Default::default()
            };
            if RegisterClassExW(&class) == 0 {
                let error = windows::core::Error::from_win32();
                if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0)
                {
                    return Err(error.into());
                }
            }
            (instance, icon, small_icon)
        };
        let host = Self(Rc::new(Host {
            hwnd: Cell::new(HWND::default()),
            wake_hwnd,
            view: RefCell::new(None),
            input: RefCell::new(egui::RawInput::default()),
            ime: Imm32::default(),
            events: RefCell::new(Vec::new()),
            start: Instant::now(),
            deadline: Cell::new(None),
            cursor: Cell::new(egui::CursorIcon::Default),
            zoom: Cell::new(1.0),
            file_tools_revision: Cell::new(None),
            settings_state: RefCell::new(None),
            popup,
        }));
        unsafe {
            let is_popup = host.0.popup.is_some();
            let title = crate::wide(&match &body {
                ViewBody::Panel(panel) => panel.borrow().title(),
                ViewBody::Search(_) => orbment_core::i18n::tr("file_tools.search_title"),
                ViewBody::Clipboard(_) => orbment_core::i18n::tr("clipboard.title"),
                ViewBody::Settings(_) => orbment_core::i18n::tr("settings.title"),
            });
            let hwnd = CreateWindowExW(
                if is_popup {
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW
                } else {
                    WINDOW_EX_STYLE::default()
                },
                w!("Orbment.MainHost"),
                PCWSTR(title.as_ptr()),
                if is_popup {
                    WS_POPUP
                } else {
                    WS_OVERLAPPEDWINDOW
                },
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                if matches!(&body, ViewBody::Clipboard(_)) {
                    420
                } else if is_popup {
                    900
                } else {
                    1100
                },
                if is_popup { 620 } else { 780 },
                None,
                None,
                Some(instance.into()),
                Some(Rc::as_ptr(&host.0).cast()),
            )?;
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(icon.0 as isize)),
            );
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(small_icon.0 as isize)),
            );
            for message in [WM_DROPFILES, WM_COPYDATA, 0x0049] {
                ChangeWindowMessageFilterEx(hwnd, message, MSGFLT_ALLOW, None)?;
            }
            DragAcceptFiles(hwnd, true);
            let dark_mode = windows::core::BOOL(0);
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&dark_mode as *const windows::core::BOOL).cast(),
                std::mem::size_of_val(&dark_mode) as u32,
            );
            let corner = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&corner as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
                std::mem::size_of_val(&corner) as u32,
            );
            let backdrop = DWMSBT_MAINWINDOW;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                (&backdrop as *const DWM_SYSTEMBACKDROP_TYPE).cast(),
                std::mem::size_of_val(&backdrop) as u32,
            );
            let context = egui::Context::default();
            SharedFonts::install(&context)?;
            crate::theme::apply_fluent_dark(&context);
            let address = hwnd.0 as usize;
            context.set_request_repaint_callback(move |request| {
                if request.delay == Duration::MAX {
                    return;
                }
                let millis = request.delay.as_millis().min(0x7fff_ffff) as usize;
                let _ = PostMessageW(
                    Some(HWND(address as *mut _)),
                    REPAINT,
                    WPARAM(millis),
                    LPARAM(0),
                );
            });
            let size = client_size(hwnd)?;
            let renderer = MeshRenderer::new(graphics.d3d_device(), graphics.immediate_context())?;
            let targets = Targets::new(&graphics, hwnd, size)?;
            *host.0.view.borrow_mut() = Some(View {
                graphics,
                context,
                renderer,
                targets: Some(targets),
                body,
                canvas: None,
                device_lost: false,
            });
        }
        Ok(host)
    }

    pub fn hwnd(&self) -> HWND {
        self.0.hwnd.get()
    }
    pub fn is_alive(&self) -> bool {
        !self.hwnd().is_invalid()
    }
    pub fn is_visible(&self) -> bool {
        self.is_alive()
            && unsafe { IsWindowVisible(self.hwnd()).as_bool() && !IsIconic(self.hwnd()).as_bool() }
    }
    pub fn memory_ui_visible(&self) -> bool {
        self.0
            .view
            .borrow()
            .as_ref()
            .and_then(|view| view.body.settings())
            .is_some_and(|settings| self.0.memory_ui_visible(settings))
    }
    pub fn show(&self) {
        if !self.is_alive() {
            return;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd(), SW_RESTORE);
            let _ = SetForegroundWindow(self.hwnd());
        }
        self.0.request_repaint();
    }
    pub fn sync_config(&self, config: Value) {
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
            && settings.sync_config(config)
        {
            self.0.request_repaint();
        }
    }
    pub fn set_diagnostics(&self, diagnostics: Vec<String>) {
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
            && settings.diagnostics != diagnostics
        {
            settings.diagnostics = diagnostics;
            self.0.request_repaint();
        }
    }
    pub fn set_monitor_notices(&self, notices: Vec<String>) {
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
            && settings.monitor_notices != notices
        {
            settings.monitor_notices = notices;
            self.0.request_repaint();
        }
    }
    pub fn select_page(&self, page: &str) {
        let mut events = Vec::new();
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
        {
            settings.flush_pending(&mut events);
            settings.selected = page.into();
        }
        for event in events {
            self.0.emit(event);
        }
        self.0.request_repaint();
    }
    pub fn set_monitor_history(&self, history: Value) {
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
            && settings.history != history
        {
            settings.history = history;
            self.0.request_repaint();
        }
    }
    pub fn set_pin_history(&self, history: Value) {
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
            && settings.set_pin_history(history)
        {
            self.0.request_repaint();
        }
    }
    pub fn set_audio_devices(&self, devices: Value) {
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
            && settings.audio_devices != devices
        {
            settings.audio_devices = devices;
            self.0.request_repaint();
        }
    }
    pub fn set_utilities_status(&self, status: Value) {
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
            && settings.utilities_status != status
        {
            settings.utilities_status = status;
            self.0.request_repaint();
        }
    }
    pub fn set_file_tools_state(&self, state: crate::SharedFileToolsState) {
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
        {
            settings.file_tools_state = state;
        }
        self.0.file_tools_revision.set(None);
        self.refresh_file_tools();
    }
    pub fn refresh_file_tools(&self) {
        if !self.is_visible() {
            return;
        }
        let view = self.0.view.borrow();
        let Some(settings) = view.as_ref().and_then(|view| view.body.settings()) else {
            return;
        };
        if !crate::settings::is_disk_page(&settings.selected) {
            return;
        }
        let revision = settings
            .file_tools_state
            .lock()
            .ok()
            .map(|state| state.revision);
        if self.0.file_tools_revision.replace(revision) != revision {
            self.0.request_repaint();
        }
    }
    pub fn show_ports(&self, port: u16) {
        self.select_page("memory_ports");
        if let Some(settings) = self
            .0
            .view
            .borrow_mut()
            .as_mut()
            .and_then(|view| view.body.settings_mut())
        {
            settings.pending_port = Some(port);
        }
        self.show();
    }
    pub fn take_events(&self) -> Vec<SettingsEvent> {
        std::mem::take(&mut *self.0.events.borrow_mut())
    }
    pub fn set_canvas(&self, clip: egui::Rect, painter: CanvasPainter) {
        if let Some(view) = self.0.view.borrow_mut().as_mut() {
            view.canvas = Some((clip, painter));
        }
        self.0.request_repaint();
    }
    pub fn rebuild_graphics(&self, graphics: Rc<D2dContext>) -> Result<()> {
        if let Some(view) = self.0.view.borrow_mut().as_mut() {
            unsafe {
                view.graphics.immediate_context().ClearState();
            }
            view.targets.take();
            unsafe {
                view.graphics.immediate_context().Flush();
            }
            view.renderer
                .rebuild(graphics.d3d_device(), graphics.immediate_context())?;
            view.targets = Some(Targets::new(
                &graphics,
                self.hwnd(),
                client_size(self.hwnd())?,
            )?);
            view.graphics = graphics;
            view.device_lost = false;
        }
        self.0.request_repaint();
        Ok(())
    }
}
impl Drop for MainHostWindow {
    fn drop(&mut self) {
        if self.is_alive() {
            unsafe {
                let _ = DestroyWindow(self.hwnd());
            }
        }
    }
}

impl Host {
    fn memory_ui_visible(&self, settings: &SettingsView) -> bool {
        !self.hwnd.get().is_invalid()
            && unsafe {
                IsWindowVisible(self.hwnd.get()).as_bool() && !IsIconic(self.hwnd.get()).as_bool()
            }
            && matches!(
                settings.selected.as_str(),
                "memory_analyzer" | "memory_reaper" | "memory_ports"
            )
            && settings.analyzer.is_some()
    }

    fn notify_settings_state(&self, settings: Option<&SettingsView>) {
        let Some(settings) = settings else {
            return;
        };
        let visible = self.memory_ui_visible(settings);
        let mut previous = self.settings_state.borrow_mut();
        if previous.as_ref().is_some_and(|(page, was_visible)| {
            page == &settings.selected && *was_visible == visible
        }) {
            return;
        }
        *previous = Some((settings.selected.clone(), visible));
        crate::wake(self.wake_hwnd);
    }

    fn emit(&self, event: SettingsEvent) {
        if self
            .popup
            .as_ref()
            .is_some_and(|popup| !popup.search && !popup.panel)
        {
            match event {
                SettingsEvent::DeviceLost => self.emit_popup(ClipboardPopupEvent::DeviceLost),
                SettingsEvent::Error(error) => self.emit_popup(ClipboardPopupEvent::Error(error)),
                SettingsEvent::Closed => self.emit_popup(ClipboardPopupEvent::Reclaimed),
                _ => {}
            }
            return;
        }
        self.events.borrow_mut().push(event);
        crate::wake(self.wake_hwnd);
    }
    fn request_repaint(&self) {
        if !self.hwnd.get().is_invalid()
            && unsafe {
                IsWindowVisible(self.hwnd.get()).as_bool() && !IsIconic(self.hwnd.get()).as_bool()
            }
            && self
                .popup
                .as_ref()
                .is_none_or(|popup| popup.hidden.get().is_none())
        {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd.get()), None, false);
            }
        }
    }
    fn schedule(&self, delay: Duration) {
        if delay == Duration::MAX
            || self.hwnd.get().is_invalid()
            || unsafe {
                !IsWindowVisible(self.hwnd.get()).as_bool() || IsIconic(self.hwnd.get()).as_bool()
            }
            || self
                .popup
                .as_ref()
                .is_some_and(|popup| popup.hidden.get().is_some())
        {
            return;
        }
        if delay.is_zero() {
            self.request_repaint();
            return;
        }
        let deadline = Instant::now() + delay;
        if self
            .deadline
            .get()
            .is_some_and(|current| current <= deadline)
        {
            return;
        }
        self.deadline.set(Some(deadline));
        unsafe {
            SetTimer(
                Some(self.hwnd.get()),
                REPAINT_TIMER,
                delay.as_millis().clamp(1, 0x7fff_ffff) as u32,
                None,
            );
        }
    }

    fn paint(&self) -> Result<()> {
        let hwnd = self.hwnd.get();
        unsafe {
            let _ = KillTimer(Some(hwnd), REPAINT_TIMER);
        }
        self.deadline.set(None);
        if unsafe { IsIconic(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() } {
            return Ok(());
        }
        let size = client_size(hwnd)?;
        let native_scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
        self.render_frame(size, native_scale, None)
    }

    /// One egui + Direct2D frame. With `capture`, the back buffer is read back instead of presented.
    fn render_frame(
        &self,
        size: [u32; 2],
        native_scale: f32,
        capture: Option<&mut Option<image::RgbaImage>>,
    ) -> Result<()> {
        let hwnd = self.hwnd.get();
        // A native modal file picker can pump WM_PAINT while an egui frame owns the view.
        let Ok(mut view_guard) = self.view.try_borrow_mut() else {
            return Ok(());
        };
        let Some(view) = view_guard.as_mut() else {
            return Ok(());
        };
        if view.device_lost {
            return Ok(());
        }
        let targets = view
            .targets
            .as_mut()
            .context("Host targets await device recovery")?;
        targets.resize(&view.graphics, size)?;
        let scale = native_scale * view.context.zoom_factor();
        let mut input = self.input.borrow_mut().take();
        input.time = Some(self.start.elapsed().as_secs_f64());
        input.modifiers = modifiers();
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(size[0] as f32 / scale, size[1] as f32 / scale),
        ));
        input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(native_scale);
        let mut events = Vec::new();
        let mut commands = Vec::new();
        let mut close_search = false;
        let output = view.context.run(input, |context| {
            match &mut view.body {
                ViewBody::Settings(settings) => settings.draw(context, &mut events),
                ViewBody::Clipboard(clipboard) => commands = clipboard.draw(context),
                ViewBody::Search(search) => close_search = search.draw(context, &mut events),
                ViewBody::Panel(panel) => {
                    panel.borrow_mut().ui(context);
                    close_search = context.input(|input| input.key_pressed(Key::Escape));
                }
            }
            if !matches!(view.body, ViewBody::Settings(_)) {
                crate::theme::window_outline(context);
            }
        });
        self.notify_settings_state(view.body.settings());
        let mut analyzer_canvas = if view.context.memory(|memory| memory.any_popup_open()) {
            None
        } else {
            view.body.settings().and_then(|settings| {
                settings
                    .analyzer
                    .as_ref()
                    .and_then(|panel| panel.canvas(view.graphics.clone()))
                    .or_else(|| {
                        settings
                            .disk
                            .as_ref()
                            .and_then(|panel| panel.canvas(view.graphics.clone()))
                    })
            })
        };
        self.zoom.set(view.context.zoom_factor());
        for event in events {
            self.emit(event);
        }
        for (id, delta) in &output.textures_delta.set {
            view.renderer.update(*id, delta)?;
        }
        let meshes = view
            .context
            .tessellate(output.shapes, output.pixels_per_point);
        let rendered = (|| -> Result<()> {
            view.renderer.upload_pending()?;
            let target = targets.rtv.as_ref().context("Host has no render target")?;
            view.renderer
                .paint(target, size, output.pixels_per_point, &meshes)?;
            if let Some((clip, painter)) = analyzer_canvas.as_mut().or(view.canvas.as_mut()) {
                let d2d = &targets.d2d;
                unsafe {
                    d2d.SetDpi(
                        output.pixels_per_point * 96.0,
                        output.pixels_per_point * 96.0,
                    );
                    d2d.BeginDraw();
                    d2d.PushAxisAlignedClip(
                        &D2D_RECT_F {
                            left: clip.min.x,
                            top: clip.min.y,
                            right: clip.max.x,
                            bottom: clip.max.y,
                        },
                        D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    );
                    let painted = painter(d2d);
                    d2d.PopAxisAlignedClip();
                    let end = d2d.EndDraw(None, None);
                    painted?;
                    end?;
                }
            }
            if let Some(capture) = capture {
                *capture = Some(read_back(&view.graphics, &targets.swapchain, size)?);
            } else {
                unsafe {
                    targets.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
                }
            }
            Ok(())
        })();
        unsafe {
            view.graphics.immediate_context().ClearState();
        }
        view.renderer.free(&output.textures_delta.free);
        rendered?;
        if let Some(ime) = output.platform_output.ime {
            let rect = ime.cursor_rect;
            self.ime.set_caret(
                hwnd,
                RECT {
                    left: (rect.min.x * output.pixels_per_point) as i32,
                    top: (rect.min.y * output.pixels_per_point) as i32,
                    right: (rect.max.x * output.pixels_per_point) as i32,
                    bottom: (rect.max.y * output.pixels_per_point) as i32,
                },
            );
        }
        self.cursor.set(output.platform_output.cursor_icon);
        set_cursor(self.cursor.get());
        for command in output.platform_output.commands {
            match command {
                egui::OutputCommand::CopyText(text) => clipboard_write(hwnd, &text)?,
                egui::OutputCommand::OpenUrl(url) => {
                    let url = crate::wide(&url.url);
                    unsafe {
                        ShellExecuteW(
                            Some(hwnd),
                            w!("open"),
                            PCWSTR(url.as_ptr()),
                            None,
                            None,
                            SW_SHOWNORMAL,
                        );
                    }
                }
                egui::OutputCommand::CopyImage(_) => {
                    anyhow::bail!("Image clipboard output is not used by the settings editor")
                }
            }
        }
        if let Some(viewport) = output.viewport_output.get(&egui::ViewportId::ROOT) {
            self.schedule(viewport.repaint_delay);
        }
        drop(view_guard);
        self.popup_commands(commands);
        if close_search {
            self.hide_popup();
        }
        Ok(())
    }

    fn commit_on_close(&self) {
        self.ime.finish(self.hwnd.get());
        let mut events = Vec::new();
        if let Some(view) = self.view.borrow_mut().as_mut() {
            let mut input = self.input.borrow_mut().take();
            input.time = Some(self.start.elapsed().as_secs_f64());
            input
                .events
                .push(Event::Ime(ImeEvent::Preedit(String::new())));
            input.events.push(Event::Ime(ImeEvent::Disabled));
            if let ViewBody::Settings(settings) = &mut view.body {
                let _ = view
                    .context
                    .run(input, |context| settings.draw(context, &mut events));
                settings.flush_pending(&mut events);
            }
        }
        for event in events {
            self.emit(event);
        }
    }

    fn input(&self, message: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
        let hwnd = self.hwnd.get();
        let ime_events = if matches!(
            message,
            WM_CHAR
                | WM_IME_STARTCOMPOSITION
                | WM_IME_COMPOSITION
                | WM_IME_ENDCOMPOSITION
                | WM_KILLFOCUS
        ) {
            self.ime.handle(hwnd, message, wparam, lparam)
        } else {
            None
        };
        if let Some(events) = ime_events {
            self.input
                .borrow_mut()
                .events
                .extend(events.into_iter().map(|event| match event {
                    ImeInput::Enabled => Event::Ime(ImeEvent::Enabled),
                    ImeInput::Disabled => Event::Ime(ImeEvent::Disabled),
                    ImeInput::Preedit(text) => Event::Ime(ImeEvent::Preedit(text)),
                    ImeInput::Commit(text) => Event::Ime(ImeEvent::Commit(text)),
                    ImeInput::Character(text) => Event::Text(text),
                }));
            self.request_repaint();
            return true;
        }
        let scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0 * self.zoom.get();
        let position = egui::pos2(
            (lparam.0 as u16 as i16) as f32 / scale,
            ((lparam.0 >> 16) as u16 as i16) as f32 / scale,
        );
        let event = match message {
            WM_MOUSEMOVE => {
                unsafe {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut track);
                }
                Some(Event::PointerMoved(position))
            }
            WM_MOUSELEAVE => Some(Event::PointerGone),
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
            | WM_MBUTTONUP => {
                let pressed = matches!(message, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN);
                unsafe {
                    if pressed {
                        SetCapture(hwnd);
                        let _ = SetFocus(Some(hwnd));
                    } else {
                        let _ = ReleaseCapture();
                    }
                }
                Some(Event::PointerButton {
                    pos: position,
                    button: match message {
                        WM_LBUTTONDOWN | WM_LBUTTONUP => egui::PointerButton::Primary,
                        WM_RBUTTONDOWN | WM_RBUTTONUP => egui::PointerButton::Secondary,
                        _ => egui::PointerButton::Middle,
                    },
                    pressed,
                    modifiers: modifiers(),
                })
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = ((wparam.0 >> 16) as u16 as i16) as f32 / 120.0;
                Some(Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: if message == WM_MOUSEWHEEL {
                        egui::vec2(0.0, delta * 3.0)
                    } else {
                        egui::vec2(-delta * 3.0, 0.0)
                    },
                    modifiers: modifiers(),
                })
            }
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
                let modifiers = modifiers();
                if pressed && modifiers.ctrl && !crate::hotkeys::is_recording() {
                    match wparam.0 as u32 {
                        0x43 => {
                            self.input.borrow_mut().events.push(Event::Copy);
                        }
                        0x58 => {
                            self.input.borrow_mut().events.push(Event::Cut);
                        }
                        0x56 => match clipboard_read(hwnd) {
                            Ok(text) => self.input.borrow_mut().events.push(Event::Paste(text)),
                            Err(error) => self.emit(SettingsEvent::Error(error.to_string())),
                        },
                        _ => {}
                    }
                }
                if pressed
                    && modifiers.ctrl
                    && !modifiers.alt
                    && !modifiers.shift
                    && !modifiers.mac_cmd
                    && wparam.0 as u32 == VK_CONTROL.0 as u32
                    && lparam.0 & (1 << 30) == 0
                {
                    crate::hotkeys::record_ctrl_press();
                }
                if crate::hotkeys::is_recording() {
                    self.request_repaint();
                }
                key(wparam.0 as u32).map(|key| Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed,
                    repeat: lparam.0 & (1 << 30) != 0,
                    modifiers,
                })
            }
            WM_SETFOCUS | WM_KILLFOCUS => {
                self.input.borrow_mut().focused = message == WM_SETFOCUS;
                if message == WM_KILLFOCUS {
                    crate::hotkeys::set_recording(false);
                }
                Some(Event::WindowFocused(message == WM_SETFOCUS))
            }
            _ => None,
        };
        if let Some(event) = event {
            self.input.borrow_mut().events.push(event);
            self.request_repaint();
            true
        } else {
            false
        }
    }
}

unsafe extern "system" fn host_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let pointer = (*(lparam.0 as *const CREATESTRUCTW)).lpCreateParams as *const Host;
            Rc::increment_strong_count(pointer);
            (*pointer).hwnd.set(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Host;
        if !pointer.is_null() {
            Rc::increment_strong_count(pointer);
            let host = Rc::from_raw(pointer);
            match message {
                WM_ACTIVATE
                    if wparam.0 as u16 == WA_INACTIVE as u16
                        && host.popup.as_ref().is_some_and(|popup| !popup.fixed.get()) =>
                {
                    host.hide_popup();
                    return LRESULT(0);
                }
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    BeginPaint(hwnd, &mut paint);
                    let result = host.paint();
                    let _ = EndPaint(hwnd, &paint);
                    if let Err(error) = result {
                        let removed = host.view.borrow().as_ref().is_some_and(|view| {
                            view.graphics.d3d_device().GetDeviceRemovedReason().is_err()
                        });
                        if removed {
                            if let Some(view) = host.view.borrow_mut().as_mut() {
                                view.device_lost = true;
                            }
                            host.emit(SettingsEvent::DeviceLost);
                        } else {
                            host.emit(SettingsEvent::Error(error.to_string()));
                        }
                    }
                    return LRESULT(0);
                }
                WM_ERASEBKGND => return LRESULT(1),
                WM_IME_SETCONTEXT => {
                    return DefWindowProcW(hwnd, message, wparam, Imm32::context_flags(lparam));
                }
                WM_CLOSE => {
                    crate::hotkeys::set_recording(false);
                    if host.popup.is_some() {
                        host.hide_popup();
                        return LRESULT(0);
                    }
                    host.commit_on_close();
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                WM_SIZE => {
                    if let Ok(view) = host.view.try_borrow() {
                        host.notify_settings_state(
                            view.as_ref().and_then(|view| view.body.settings()),
                        );
                    }
                    host.request_repaint();
                    return LRESULT(0);
                }
                WM_SHOWWINDOW => {
                    let result = DefWindowProcW(hwnd, message, wparam, lparam);
                    if let Ok(view) = host.view.try_borrow() {
                        host.notify_settings_state(
                            view.as_ref().and_then(|view| view.body.settings()),
                        );
                    }
                    return result;
                }
                WM_DPICHANGED => {
                    let rect = &*(lparam.0 as *const RECT);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    host.request_repaint();
                    return LRESULT(0);
                }
                WM_EXITSIZEMOVE => {
                    if let Some(popup) = &host.popup {
                        let mut rect = RECT::default();
                        if GetWindowRect(hwnd, &mut rect).is_ok() {
                            popup.position.set(Some(POINT {
                                x: rect.left,
                                y: rect.top,
                            }));
                        }
                    }
                    return LRESULT(0);
                }
                WM_TIMER if wparam.0 == REPAINT_TIMER => {
                    let _ = KillTimer(Some(hwnd), REPAINT_TIMER);
                    host.deadline.set(None);
                    host.request_repaint();
                    return LRESULT(0);
                }
                REPAINT => {
                    host.schedule(Duration::from_millis(wparam.0 as u64));
                    return LRESULT(0);
                }
                WM_SETCURSOR if lparam.0 as u16 == HTCLIENT as u16 => {
                    set_cursor(host.cursor.get());
                    return LRESULT(1);
                }
                WM_DROPFILES => {
                    let drop = HDROP(wparam.0 as *mut _);
                    let count = DragQueryFileW(drop, u32::MAX, None);
                    let mut files = Vec::new();
                    for index in 0..count {
                        let length = DragQueryFileW(drop, index, None);
                        let mut buffer = vec![0_u16; length as usize + 1];
                        DragQueryFileW(drop, index, Some(&mut buffer));
                        files.push(PathBuf::from(String::from_utf16_lossy(
                            &buffer[..length as usize],
                        )));
                    }
                    DragFinish(drop);
                    host.emit(SettingsEvent::FilesDropped(files));
                    return LRESULT(0);
                }
                WM_NCDESTROY => {
                    let _ = KillTimer(Some(hwnd), REPAINT_TIMER);
                    host.hwnd.set(HWND::default());
                    if let Some(view) = host.view.borrow_mut().take() {
                        if let Some(settings) = view.body.settings()
                            && settings.disk.is_some()
                        {
                            host.emit(SettingsEvent::FileTools(crate::FileToolsAction::DiskClosed));
                        }
                        let immediate = view.graphics.immediate_context().clone();
                        immediate.ClearState();
                        drop(view);
                        immediate.Flush();
                    }
                    host.input.borrow_mut().events.clear();
                    host.emit(SettingsEvent::Closed);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Rc::from_raw(pointer));
                }
                _ => {
                    if host.input(message, wparam, lparam)
                        && !matches!(message, WM_SYSKEYDOWN | WM_SYSKEYUP)
                    {
                        return LRESULT(0);
                    }
                }
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

fn client_size(hwnd: HWND) -> Result<[u32; 2]> {
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut rect)?;
    }
    Ok([rect.right.max(1) as u32, rect.bottom.max(1) as u32])
}

/// Copies the rendered swapchain back buffer through a CPU staging texture.
fn read_back(
    graphics: &D2dContext,
    swapchain: &IDXGISwapChain1,
    size: [u32; 2],
) -> Result<image::RgbaImage> {
    unsafe {
        let source: ID3D11Texture2D = swapchain.GetBuffer(0)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        source.GetDesc(&mut desc);
        ensure!(
            desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM,
            "Unexpected back buffer format"
        );
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging = None;
        graphics
            .d3d_device()
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.context("D3D11 returned no staging texture")?;
        let context = graphics.immediate_context();
        context.CopyResource(&staging, &source);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let width = desc.Width.min(size[0]) as usize;
        let height = desc.Height.min(size[1]) as usize;
        let mut pixels = vec![0u8; width * height * 4];
        for y in 0..height {
            let row = std::slice::from_raw_parts(
                (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                width * 4,
            );
            for (dst, src) in pixels[y * width * 4..(y + 1) * width * 4]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(row.as_chunks::<4>().0)
            {
                *dst = [src[2], src[1], src[0], 255];
            }
        }
        context.Unmap(&staging, 0);
        image::RgbaImage::from_raw(width as u32, height as u32, pixels)
            .context("Back buffer dimensions mismatch")
    }
}
fn modifiers() -> Modifiers {
    let down = |key: VIRTUAL_KEY| unsafe { GetKeyState(key.0 as i32) < 0 };
    let ctrl = down(VK_CONTROL);
    Modifiers {
        alt: down(VK_MENU),
        ctrl,
        shift: down(VK_SHIFT),
        mac_cmd: crate::hotkeys::is_recording() && (down(VK_LWIN) || down(VK_RWIN)),
        command: ctrl,
    }
}
fn key(key: u32) -> Option<Key> {
    Some(match key {
        0x08 => Key::Backspace,
        0x09 => Key::Tab,
        0x0d => Key::Enter,
        0x1b => Key::Escape,
        0x20 => Key::Space,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x23 => Key::End,
        0x24 => Key::Home,
        0x25 => Key::ArrowLeft,
        0x26 => Key::ArrowUp,
        0x27 => Key::ArrowRight,
        0x28 => Key::ArrowDown,
        0x2d => Key::Insert,
        0x2e => Key::Delete,
        0x30 => Key::Num0,
        0x31 => Key::Num1,
        0x32 => Key::Num2,
        0x33 => Key::Num3,
        0x34 => Key::Num4,
        0x35 => Key::Num5,
        0x36 => Key::Num6,
        0x37 => Key::Num7,
        0x38 => Key::Num8,
        0x39 => Key::Num9,
        0x41 => Key::A,
        0x42 => Key::B,
        0x43 => Key::C,
        0x44 => Key::D,
        0x45 => Key::E,
        0x46 => Key::F,
        0x47 => Key::G,
        0x48 => Key::H,
        0x49 => Key::I,
        0x4a => Key::J,
        0x4b => Key::K,
        0x4c => Key::L,
        0x4d => Key::M,
        0x4e => Key::N,
        0x4f => Key::O,
        0x50 => Key::P,
        0x51 => Key::Q,
        0x52 => Key::R,
        0x53 => Key::S,
        0x54 => Key::T,
        0x55 => Key::U,
        0x56 => Key::V,
        0x57 => Key::W,
        0x58 => Key::X,
        0x59 => Key::Y,
        0x5a => Key::Z,
        0x70 => Key::F1,
        0x71 => Key::F2,
        0x72 => Key::F3,
        0x73 => Key::F4,
        0x74 => Key::F5,
        0x75 => Key::F6,
        0x76 => Key::F7,
        0x77 => Key::F8,
        0x78 => Key::F9,
        0x79 => Key::F10,
        0x7a => Key::F11,
        0x7b => Key::F12,
        0xba => Key::Semicolon,
        0xbb => Key::Equals,
        0xbc => Key::Comma,
        0xbd => Key::Minus,
        0xbe => Key::Period,
        0xbf => Key::Slash,
        0xc0 => Key::Backtick,
        0xdb => Key::OpenBracket,
        0xdc => Key::Backslash,
        0xdd => Key::CloseBracket,
        0xde => Key::Quote,
        _ => return None,
    })
}
fn set_cursor(cursor: egui::CursorIcon) {
    let id = match cursor {
        egui::CursorIcon::Text | egui::CursorIcon::VerticalText => IDC_IBEAM,
        egui::CursorIcon::PointingHand => IDC_HAND,
        egui::CursorIcon::ResizeHorizontal => IDC_SIZEWE,
        egui::CursorIcon::ResizeVertical => IDC_SIZENS,
        egui::CursorIcon::ResizeNwSe => IDC_SIZENWSE,
        egui::CursorIcon::ResizeNeSw => IDC_SIZENESW,
        egui::CursorIcon::Grab | egui::CursorIcon::Grabbing => IDC_SIZEALL,
        egui::CursorIcon::NotAllowed | egui::CursorIcon::NoDrop => IDC_NO,
        _ => IDC_ARROW,
    };
    unsafe {
        if cursor == egui::CursorIcon::None {
            SetCursor(None);
        } else if let Ok(cursor) = LoadCursorW(None, id) {
            SetCursor(Some(cursor));
        }
    }
}
struct Clipboard;
impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}
fn clipboard_read(hwnd: HWND) -> Result<String> {
    unsafe {
        OpenClipboard(Some(hwnd))?;
        let _clipboard = Clipboard;
        let memory = HGLOBAL(GetClipboardData(13)?.0);
        let pointer = GlobalLock(memory).cast::<u16>();
        ensure!(!pointer.is_null(), "Unable to lock clipboard text");
        let slice = std::slice::from_raw_parts(pointer, GlobalSize(memory) / 2);
        let length = slice
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(slice.len());
        let text = String::from_utf16_lossy(&slice[..length]);
        let _ = GlobalUnlock(memory);
        Ok(text)
    }
}
fn clipboard_write(hwnd: HWND, text: &str) -> Result<()> {
    unsafe {
        OpenClipboard(Some(hwnd))?;
        let _clipboard = Clipboard;
        let text = crate::wide(text);
        let memory = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2)?;
        let pointer = GlobalLock(memory).cast::<u16>();
        if pointer.is_null() {
            let _ = GlobalFree(Some(memory));
            anyhow::bail!("Unable to lock clipboard allocation");
        }
        std::ptr::copy_nonoverlapping(text.as_ptr(), pointer, text.len());
        let _ = GlobalUnlock(memory);
        let result = EmptyClipboard()
            .and_then(|()| SetClipboardData(13, Some(HANDLE(memory.0))).map(|_| ()));
        if result.is_err() {
            let _ = GlobalFree(Some(memory));
        }
        result?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_fetch_event_debug_redacts_unsaved_api_key() {
        let (result, _) = std::sync::mpsc::channel();
        let event = SettingsEvent::QuickAskModels {
            config: Box::default(),
            api_key_override: Some("private-api-key".into()),
            result,
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("QuickAskModels"));
        assert!(!debug.contains("private-api-key"));
    }
}
