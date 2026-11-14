use crate::{
    CapturedImage, PinHistoryEntry, Point, Rect,
    arrow_comment::{ArrowComment, ArrowCommentEditor, EditResult, FontStyle, Orientation},
    canvas_mark::{Mark, MarkKind, MarkStyle, MosaicMode, Scene},
    capture_overlay::{
        CaptureSource, CaptureStream, Dib, FRAME_MESSAGE, PresentTarget, ToolbarIcon, UiCanvas,
        ui_rect, wide,
    },
    pin_text_select::{PinTextSelection, PointerAction},
    save_template::{self, SaveTemplateContext},
};
use anyhow::{Context, Result, ensure};
use orbment_core::{
    config::{VisionConfig, VisionMarkConfig, VisionTextConfig},
    i18n::tr,
    ocr::OcrResult,
};
use orbment_ui::D2dContext;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    time::Instant,
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        System::{Com::CoCreateGuid, LibraryLoader::GetModuleHandleW},
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::*,
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PinInfo {
    pub id: String,
    pub kind: String,
    pub group: String,
    pub title: String,
    pub bounds: Rect,
    pub scale_pct: f64,
    pub opacity_pct: f64,
    pub topmost: bool,
    pub locked: bool,
    pub penetrate: bool,
    pub thumbnail: bool,
    pub visible: bool,
}
#[derive(Clone, Debug)]
pub enum PinEvent {
    Closed(String),
    Action { id: String, action: String },
    Error(String),
}
enum Pending {
    Dirty(String),
    DirtyMeta(String),
    Close(String),
    HideOthers(String),
    Action(String, String),
    Error(String),
}

fn coalesce_dirty(events: Vec<Pending>) -> (Vec<Pending>, HashMap<String, bool>) {
    let mut pending = Vec::new();
    let mut dirty = HashMap::new();
    for event in events {
        match event {
            Pending::Dirty(id) => {
                dirty.insert(id, true);
            }
            Pending::DirtyMeta(id) => {
                dirty.entry(id).or_insert(false);
            }
            event => pending.push(event),
        }
    }
    (pending, dirty)
}
static SEQUENCE_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static IMAGE_REVISION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_image_revision() -> u64 {
    IMAGE_REVISION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

const PIN_ACTIONS: [ToolbarIcon; 7] = [
    ToolbarIcon::Pen,
    ToolbarIcon::Copy,
    ToolbarIcon::Save,
    ToolbarIcon::Ocr,
    ToolbarIcon::QuickAsk,
    ToolbarIcon::Pin,
    ToolbarIcon::Cancel,
];
const PIN_TOOLBAR_WIDTH: i32 = 324;
const PIN_TOOLBAR_HEIGHT: i32 = 108;

const PIN_KEYS: [u16; 18] = [
    VK_ESCAPE.0,
    VK_DELETE.0,
    VK_BACK.0,
    VK_LEFT.0,
    VK_UP.0,
    VK_RIGHT.0,
    VK_DOWN.0,
    b'R' as u16,
    b'L' as u16,
    b'H' as u16,
    b'V' as u16,
    b'G' as u16,
    b'I' as u16,
    b'0' as u16,
    b'C' as u16,
    b'S' as u16,
    b'Z' as u16,
    b'Y' as u16,
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum PinKeyAction {
    Close,
    DeleteSelected,
    Transform(TransformAction),
    Move(Point),
    Event(&'static str),
    Undo,
    Redo,
}

fn pin_key_action(
    key: u16,
    control: bool,
    shift: bool,
    alt: bool,
    locked: bool,
    text_input: bool,
) -> Option<PinKeyAction> {
    if text_input || alt {
        return None;
    }
    if control {
        return match key {
            k if k == u16::from(b'C') => Some(PinKeyAction::Event("vision.copy_image")),
            k if k == u16::from(b'S') => Some(PinKeyAction::Event("vision.save_image")),
            k if k == u16::from(b'V') => Some(PinKeyAction::Event("vision.replace_pin")),
            k if k == u16::from(b'Z') => Some(PinKeyAction::Undo),
            k if k == u16::from(b'Y') => Some(PinKeyAction::Redo),
            _ => None,
        };
    }
    let step = if shift { 10. } else { 1. };
    match key {
        k if k == VK_ESCAPE.0 => Some(PinKeyAction::Close),
        k if k == VK_DELETE.0 || k == VK_BACK.0 => Some(PinKeyAction::DeleteSelected),
        k if k == VK_LEFT.0 && !locked => Some(PinKeyAction::Move(Point { x: -step, y: 0. })),
        k if k == VK_RIGHT.0 && !locked => Some(PinKeyAction::Move(Point { x: step, y: 0. })),
        k if k == VK_UP.0 && !locked => Some(PinKeyAction::Move(Point { x: 0., y: -step })),
        k if k == VK_DOWN.0 && !locked => Some(PinKeyAction::Move(Point { x: 0., y: step })),
        k if k == u16::from(b'R') => Some(PinKeyAction::Transform(TransformAction::RotateRight)),
        k if k == u16::from(b'L') => Some(PinKeyAction::Transform(TransformAction::RotateLeft)),
        k if k == u16::from(b'H') => Some(PinKeyAction::Transform(TransformAction::FlipHorizontal)),
        k if k == u16::from(b'V') => Some(PinKeyAction::Transform(TransformAction::FlipVertical)),
        k if k == u16::from(b'G') => Some(PinKeyAction::Transform(TransformAction::Grayscale)),
        k if k == u16::from(b'I') => Some(PinKeyAction::Transform(TransformAction::Invert)),
        k if k == u16::from(b'0') => Some(PinKeyAction::Transform(TransformAction::Reset)),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct PinTransform {
    quarter_turns: u8,
    flip_horizontal: bool,
    flip_vertical: bool,
    grayscale: bool,
    invert: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransformAction {
    RotateRight,
    RotateLeft,
    FlipHorizontal,
    FlipVertical,
    Grayscale,
    Invert,
    Reset,
}

impl PinTransform {
    fn change(&mut self, action: TransformAction) {
        match action {
            TransformAction::RotateRight => self.quarter_turns = (self.quarter_turns % 4 + 1) % 4,
            TransformAction::RotateLeft => self.quarter_turns = (self.quarter_turns % 4 + 3) % 4,
            TransformAction::FlipHorizontal if self.quarter_turns.is_multiple_of(2) => {
                self.flip_horizontal = !self.flip_horizontal;
            }
            TransformAction::FlipVertical if !self.quarter_turns.is_multiple_of(2) => {
                self.flip_horizontal = !self.flip_horizontal;
            }
            TransformAction::FlipHorizontal | TransformAction::FlipVertical => {
                self.flip_vertical = !self.flip_vertical;
            }
            TransformAction::Grayscale => self.grayscale = !self.grayscale,
            TransformAction::Invert => self.invert = !self.invert,
            TransformAction::Reset => *self = Self::default(),
        }
    }

    fn dimensions(self, width: u32, height: u32) -> (u32, u32) {
        if self.quarter_turns.is_multiple_of(2) {
            (width, height)
        } else {
            (height, width)
        }
    }

    fn map_point(self, mut point: Point, width: f64, height: f64) -> Point {
        if self.flip_horizontal {
            point.x = width - point.x;
        }
        if self.flip_vertical {
            point.y = height - point.y;
        }
        match self.quarter_turns % 4 {
            1 => Point {
                x: height - point.y,
                y: point.x,
            },
            2 => Point {
                x: width - point.x,
                y: height - point.y,
            },
            3 => Point {
                x: point.y,
                y: width - point.x,
            },
            _ => point,
        }
    }

    fn unmap_point(self, point: Point, width: f64, height: f64) -> Point {
        let mut point = match self.quarter_turns % 4 {
            1 => Point {
                x: point.y,
                y: height - point.x,
            },
            2 => Point {
                x: width - point.x,
                y: height - point.y,
            },
            3 => Point {
                x: width - point.y,
                y: point.x,
            },
            _ => point,
        };
        if self.flip_horizontal {
            point.x = width - point.x;
        }
        if self.flip_vertical {
            point.y = height - point.y;
        }
        point
    }

    fn apply(self, image: CapturedImage) -> Result<CapturedImage> {
        // Geometry only permutes pixels, so the image helpers can retain BGRA bytes.
        let mut pixels = image::RgbaImage::from_raw(image.width, image.height, image.pixels)
            .context("Pin image dimensions invalid")?;
        if self.flip_horizontal {
            image::imageops::flip_horizontal_in_place(&mut pixels);
        }
        if self.flip_vertical {
            image::imageops::flip_vertical_in_place(&mut pixels);
        }
        pixels = match self.quarter_turns % 4 {
            1 => image::imageops::rotate90(&pixels),
            2 => image::imageops::rotate180(&pixels),
            3 => image::imageops::rotate270(&pixels),
            _ => pixels,
        };
        if self.grayscale || self.invert {
            for pixel in pixels.pixels_mut() {
                if self.grayscale {
                    let luma = ((u32::from(pixel[0]) * 114
                        + u32::from(pixel[1]) * 587
                        + u32::from(pixel[2]) * 299
                        + 500)
                        / 1000) as u8;
                    pixel.0[..3].fill(luma);
                }
                if self.invert {
                    for channel in &mut pixel.0[..3] {
                        *channel = 255 - *channel;
                    }
                }
            }
        }
        Ok(CapturedImage {
            width: pixels.width(),
            height: pixels.height(),
            pixels: pixels.into_raw(),
        })
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PinDocument {
    #[serde(flatten)]
    scene: Scene,
    #[serde(default)]
    transform: PinTransform,
}

#[derive(Default)]
struct TemporaryVisibility {
    depth: Cell<usize>,
    visible: Cell<[bool; 2]>,
    alive: Cell<bool>,
}

/// Temporarily hides a pin without borrowing its manager or changing saved visibility.
#[must_use]
pub struct PinVisibilityGuard {
    windows: [HWND; 2],
    visibility: Rc<TemporaryVisibility>,
}

impl PinVisibilityGuard {
    fn hide(windows: [HWND; 2], visibility: Rc<TemporaryVisibility>) -> Self {
        if visibility.depth.get() == 0 {
            visibility.visible.set(windows.map(|hwnd| unsafe {
                IsWindow(Some(hwnd)).as_bool() && IsWindowVisible(hwnd).as_bool()
            }));
        }
        visibility.depth.set(visibility.depth.get() + 1);
        unsafe {
            let _ = KillTimer(Some(windows[0]), 2);
            for hwnd in windows.into_iter().rev() {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
        Self {
            windows,
            visibility,
        }
    }
}

impl Drop for PinVisibilityGuard {
    fn drop(&mut self) {
        let remaining = self.visibility.depth.get() - 1;
        self.visibility.depth.set(remaining);
        if remaining != 0 || !self.visibility.alive.get() {
            return;
        }
        for (hwnd, visible) in self.windows.into_iter().zip(self.visibility.visible.get()) {
            if visible && unsafe { IsWindow(Some(hwnd)).as_bool() } {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
            }
        }
        if self.visibility.visible.get()[1]
            && self
                .windows
                .iter()
                .all(|hwnd| unsafe { IsWindow(Some(*hwnd)).as_bool() })
        {
            unsafe {
                SetTimer(Some(self.windows[0]), 2, 75, None);
            }
        }
    }
}

#[derive(PartialEq)]
struct PinRenderKey {
    marks: Vec<Mark>,
    selected: Option<u64>,
    pending: Option<(MarkKind, MarkStyle)>,
    editor: Option<(u64, ArrowComment)>,
    highlights: Vec<Rect>,
    width: u32,
    height: u32,
    penetrate: bool,
    transform: PinTransform,
}

struct PinRenderCache {
    key: PinRenderKey,
    image: CapturedImage,
    dib: Option<Dib>,
}

struct MarkEditDrag {
    origin: Point,
    handle: Option<usize>,
    original: Mark,
}

struct PinState {
    info: PinInfo,
    image: CapturedImage,
    transform: PinTransform,
    image_revision: u64,
    text: Option<String>,
    scene: Scene,
    ocr: Option<PinTextSelection>,
    editor: Option<ArrowCommentEditor>,
    target: Option<PresentTarget>,
    render_cache: Option<PinRenderCache>,
    capture_excluded: bool,
    stream: Option<CaptureStream>,
    events: Rc<RefCell<Vec<Pending>>>,
    owner: HWND,
    show_taskbar: bool,
    text_selectable: bool,
    text_popup: bool,
    comments: bool,
    drag: Option<(Point, Point)>,
    last_drag_x: f64,
    last_direction: f64,
    reversals: u8,
    shake_start: Instant,
    tool: String,
    mark_points: Vec<Point>,
    edit_drag: Option<MarkEditDrag>,
    wake: Option<isize>,
    mark_config: VisionMarkConfig,
    pending_auto_mosaics: Vec<Rect>,
    host: HWND,
    toolbar: HWND,
    toolbar_hover: Option<usize>,
    keys_down: [bool; PIN_KEYS.len()],
    temporary_visibility: Rc<TemporaryVisibility>,
    closing: bool,
}
struct PinWindow {
    hwnd: HWND,
    toolbar: HWND,
    state: Box<RefCell<PinState>>,
}
impl PinWindow {
    fn show(&self, visible: bool) {
        self.state.borrow_mut().info.visible = visible;
        if self.state.borrow().temporary_visibility.depth.get() != 0 {
            if !visible {
                self.state
                    .borrow()
                    .temporary_visibility
                    .visible
                    .set([false; 2]);
            }
            return;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
            let _ = ShowWindow(self.toolbar, SW_HIDE);
        }
    }
}
impl Drop for PinWindow {
    fn drop(&mut self) {
        self.state.borrow().temporary_visibility.alive.set(false);
        unsafe {
            for hwnd in [self.toolbar, self.hwnd] {
                if !hwnd.is_invalid() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
    }
}

pub struct PinManager {
    pins: HashMap<String, PinWindow>,
    db: Connection,
    data: PathBuf,
    graphics: Rc<D2dContext>,
    owner: HWND,
    events: Rc<RefCell<Vec<Pending>>>,
    config: VisionConfig,
    counter: u64,
    wake: Option<isize>,
}
impl PinManager {
    pub fn new(data: &Path, graphics: Rc<D2dContext>) -> Result<Self> {
        std::fs::create_dir_all(data.join("media/pins"))?;
        let db = orbment_core::db::open(data)?;
        Ok(Self {
            pins: HashMap::new(),
            db,
            data: data.into(),
            graphics,
            owner: HWND::default(),
            events: Rc::new(RefCell::new(Vec::new())),
            config: VisionConfig::default(),
            counter: 0,
            wake: None,
        })
    }
    pub fn configure(&mut self, config: &VisionConfig) -> Result<()> {
        self.config = config.clone();
        for pin in self.pins.values() {
            let mut state = pin.state.borrow_mut();
            state.text_selectable = config.pin.text_selectable_default;
            state.text_popup = config.pin.show_text_popup_menu;
            state.comments = config.mark.arrow.double_click_comment;
            state.mark_config = config.mark.clone();
            state.show_taskbar = config.pin.show_taskbar_icon_when_unpinned;
            state.styles(pin.hwnd)?;
            state.position_toolbar()?;
            unsafe {
                let _ = InvalidateRect(Some(pin.toolbar), None, false);
            }
        }
        Ok(())
    }
    pub fn list(&self) -> Vec<PinInfo> {
        let mut pins = self
            .pins
            .values()
            .map(|p| p.state.borrow().info.clone())
            .collect::<Vec<_>>();
        pins.sort_by(|a, b| a.id.cmp(&b.id));
        pins
    }
    pub fn handles(&self) -> Vec<isize> {
        self.pins
            .values()
            .flat_map(|p| [p.hwnd.0 as isize, p.toolbar.0 as isize])
            .chain((!self.owner.is_invalid()).then_some(self.owner.0 as isize))
            .collect()
    }
    pub fn window_handle(&self, id: &str) -> Result<isize> {
        Ok(self.pins.get(id).context("Pin not found")?.hwnd.0 as isize)
    }
    pub fn image_revision(&self, id: &str) -> Result<u64> {
        Ok(self
            .pins
            .get(id)
            .context("Pin not found")?
            .state
            .borrow()
            .image_revision)
    }

    pub fn ocr_image(&self, id: &str) -> Result<CapturedImage> {
        let pin = self.pins.get(id).context("Pin not found")?;
        let state = pin.state.borrow();
        state
            .scene
            .render_preview(&state.image, state.editor.as_ref())
    }

    pub fn hide_for_save(&self, id: &str) -> Result<PinVisibilityGuard> {
        let pin = self.pins.get(id).context("Pin not found")?;
        let visibility = pin.state.borrow().temporary_visibility.clone();
        Ok(PinVisibilityGuard::hide(
            [pin.hwnd, pin.toolbar],
            visibility,
        ))
    }

    pub fn replace_image(&mut self, id: &str, image: CapturedImage) -> Result<()> {
        image.validate()?;
        let pin = self.pins.get(id).context("Pin not found")?;
        {
            let mut state = pin.state.borrow_mut();
            state.replace_image(image);
            unsafe {
                if GetCapture() == pin.hwnd {
                    let _ = ReleaseCapture();
                }
            }
            state.draw(pin.hwnd)?;
            if state.text_selectable {
                state
                    .events
                    .borrow_mut()
                    .push(Pending::Action(id.into(), "vision.pin_select_text".into()));
                if let Some(wake) = state.wake {
                    orbment_ui::wake(HWND(wake as *mut _));
                }
            }
        }
        self.persist(id)
    }

    pub fn image(&self, id: &str) -> Result<CapturedImage> {
        if let Some(pin) = self.pins.get(id) {
            return pin.state.borrow().full_resolution_image();
        }
        let (path, scene): (String, Option<String>) = self.db.query_row(
            "SELECT image_rel_path,annotations_json FROM pin_records WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let image = CapturedImage::load(&self.safe_image_path(&path)?)?;
        let document: PinDocument = scene
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default();
        document.transform.apply(document.scene.render(&image)?)
    }
    pub fn history(&self) -> Result<Vec<PinHistoryEntry>> {
        let mut statement = self.db.prepare(
            "SELECT id,pin_kind,title,image_rel_path,pos_x,pos_y,width,height,created_at,closed_at
             FROM pin_records ORDER BY created_at DESC,id DESC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(3)?,
                    Rect {
                        x: row.get::<_, f64>(4)?,
                        y: row.get::<_, f64>(5)?,
                        width: row.get::<_, f64>(6)?,
                        height: row.get::<_, f64>(7)?,
                    },
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, kind, title, path, bounds, created_at, closed_at)| {
                let active = self
                    .pins
                    .get(&id)
                    .map(|pin| pin.state.borrow().info.clone());
                Ok(PinHistoryEntry {
                    image_path: path.map(|path| self.safe_image_path(&path)).transpose()?,
                    bounds: active.as_ref().map_or(bounds, |pin| pin.bounds),
                    is_active: active.is_some(),
                    id,
                    kind,
                    title,
                    created_at,
                    closed_at,
                })
            })
            .collect()
    }
    pub fn restore_by_id(&mut self, id: &str) -> Result<String> {
        if let Some(pin) = self.pins.get(id) {
            pin.show(true);
            unsafe {
                SetWindowPos(
                    pin.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )?;
            }
            pin.state.borrow_mut().info.topmost = true;
            pin.state.borrow().styles(pin.hwnd)?;
            self.persist_meta(id)?;
        } else {
            self.restore_id(id)?;
        }
        Ok(id.into())
    }
    pub fn delete_history(&mut self, id: &str) -> Result<()> {
        let path: Option<String> = self.db.query_row(
            "SELECT image_rel_path FROM pin_records WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        self.pins.remove(id);
        self.release_owner_if_empty();
        if let Some(path) = path {
            match std::fs::remove_file(self.safe_image_path(&path)?) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.db
            .execute("DELETE FROM pin_records WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn close_all(&mut self) -> Result<()> {
        for id in self.pins.keys().cloned().collect::<Vec<_>>() {
            self.close(&id)?;
        }
        Ok(())
    }
    fn cascade(&self, bounds: Rect) -> Result<Rect> {
        let monitors = crate::capture_overlay::monitors()?;
        let monitor = monitors
            .iter()
            .max_by(|a, b| {
                let area = |m: &crate::capture_overlay::Monitor| {
                    bounds
                        .intersection(m.bounds)
                        .map_or(0., |r| r.width * r.height)
                };
                area(a).total_cmp(&area(b))
            })
            .context("No desktop monitor")?;
        let occupied = self
            .pins
            .values()
            .filter_map(|pin| {
                let state = pin.state.borrow();
                state.info.visible.then_some(Point {
                    x: state.info.bounds.x,
                    y: state.info.bounds.y,
                })
            })
            .collect::<Vec<_>>();
        Ok(cascade_bounds(bounds, monitor.bounds, &occupied))
    }
    pub fn bounds(&self, id: &str) -> Result<Rect> {
        Ok(self
            .pins
            .get(id)
            .context("Pin not found")?
            .state
            .borrow()
            .info
            .bounds)
    }
    pub fn set_group(&mut self, id: &str, group: &str) -> Result<()> {
        let pin = self.pins.get(id).context("Pin not found")?;
        pin.state.borrow_mut().info.group = group.into();
        self.persist_meta(id)
    }
    pub fn set_ocr(&mut self, id: &str, result: OcrResult) -> Result<()> {
        let pin = self.pins.get(id).context("Pin not found")?;
        {
            let mut state = pin.state.borrow_mut();
            state.ocr = Some(PinTextSelection::new(result));
            for region in std::mem::take(&mut state.pending_auto_mosaics) {
                state.add_auto_mosaic(region);
            }
            state.draw(pin.hwnd)?;
        }
        self.persist(id)
    }
    pub fn set_scene(&mut self, id: &str, scene: Scene) -> Result<()> {
        let pin = self.pins.get(id).context("Pin not found")?;
        pin.state.borrow_mut().scene = scene;
        pin.state.borrow_mut().draw(pin.hwnd)?;
        self.persist(id)
    }
    pub fn begin_annotation(&mut self, id: &str) -> Result<()> {
        let pin = self.pins.get(id).context("Pin not found")?;
        pin.state.borrow_mut().tool = "rectangle".into();
        Ok(())
    }
    pub fn set_wake_hwnd(&mut self, hwnd: isize) {
        self.wake = Some(hwnd);
        for pin in self.pins.values() {
            pin.state.borrow_mut().wake = Some(hwnd);
        }
    }
    pub fn set_capture_excluded(&self, excluded: bool) -> Result<()> {
        for pin in self.pins.values() {
            let mut state = pin.state.borrow_mut();
            unsafe {
                SetWindowDisplayAffinity(
                    pin.hwnd,
                    if excluded {
                        WDA_EXCLUDEFROMCAPTURE
                    } else {
                        WDA_NONE
                    },
                )?;
                state.capture_excluded = excluded;
                state.styles(pin.hwnd)?;
                if excluded {
                    let _ = ShowWindow(pin.toolbar, SW_HIDE);
                    let _ = KillTimer(Some(pin.hwnd), 2);
                    if GetCapture() == pin.hwnd {
                        let _ = ReleaseCapture();
                    }
                }
            }
        }
        Ok(())
    }
    pub fn image_pin(&mut self, image: CapturedImage, position: Point) -> Result<String> {
        self.create(image, position, "image", None, None)
    }
    pub fn text_pin(&mut self, text: String, html: bool, position: Point) -> Result<String> {
        let plain = if html {
            html2text::from_read(text.as_bytes(), 90)?
        } else {
            text.clone()
        };
        let image = text_card(&plain, None, card_scale(position)?)?;
        self.create(image, position, "text", Some(text), None)
    }
    pub fn color_pin(&mut self, rgb: [u8; 3], position: Point) -> Result<String> {
        let title = crate::capture_overlay::format_color(rgb, "%HEX\n%RGB\n%HSL");
        let image = text_card(&title, Some(rgb), card_scale(position)?)?;
        self.create(
            image,
            position,
            "color",
            Some(format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])),
            None,
        )
    }
    pub fn window_pin(
        &mut self,
        hwnd: isize,
        roi: Option<Rect>,
        position: Point,
    ) -> Result<String> {
        ensure!(
            unsafe { IsWindow(Some(HWND(hwnd as *mut _))).as_bool() },
            "Source window no longer exists"
        );
        let source = CaptureSource::Window { hwnd, roi };
        let image = crate::capture_overlay::capture_source(source.clone())?;
        let stream = CaptureStream::start(source)?;
        self.create(image, position, "window_roi", None, Some(stream))
    }
    fn create(
        &mut self,
        image: CapturedImage,
        position: Point,
        kind: &str,
        text: Option<String>,
        stream: Option<CaptureStream>,
    ) -> Result<String> {
        image.validate()?;
        Rect {
            x: position.x,
            y: position.y,
            width: image.width as f64,
            height: image.height as f64,
        }
        .validate()?;
        let id = format!("{:?}", unsafe { CoCreateGuid()? });
        let mut info = PinInfo {
            id: id.clone(),
            kind: kind.into(),
            group: "default".into(),
            title: text
                .as_deref()
                .unwrap_or("Orbment")
                .chars()
                .take(80)
                .collect(),
            bounds: Rect {
                x: position.x,
                y: position.y,
                width: image.width as f64,
                height: image.height as f64,
            },
            scale_pct: 100.0,
            opacity_pct: self.config.pin.default_opacity_pct as f64,
            topmost: self.config.pin.stays_on_top,
            locked: false,
            penetrate: false,
            thumbnail: false,
            visible: true,
        };
        info.bounds = self.cascade(info.bounds)?;
        self.spawn(info, image, text, PinDocument::default(), stream)?;
        if let Err(error) = self.persist(&id) {
            self.pins.remove(&id);
            self.release_owner_if_empty();
            return Err(error);
        }
        Ok(id)
    }
    fn spawn(
        &mut self,
        info: PinInfo,
        image: CapturedImage,
        text: Option<String>,
        document: PinDocument,
        stream: Option<CaptureStream>,
    ) -> Result<()> {
        if self.owner.is_invalid() {
            self.owner = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    w!("STATIC"),
                    w!("OrbmentPinOwner"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(GetModuleHandleW(None)?.into()),
                    None,
                )?
            };
        }
        let result = self.spawn_native(info, image, text, document, stream);
        if result.is_err() {
            self.release_owner_if_empty();
        }
        result
    }
    fn release_owner_if_empty(&mut self) {
        if self.pins.is_empty() && !self.owner.is_invalid() {
            let owner = std::mem::take(&mut self.owner);
            unsafe {
                let _ = DestroyWindow(owner);
            }
        }
    }
    fn spawn_native(
        &mut self,
        info: PinInfo,
        image: CapturedImage,
        text: Option<String>,
        document: PinDocument,
        stream: Option<CaptureStream>,
    ) -> Result<()> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                style: CS_DBLCLKS | CS_DROPSHADOW,
                lpfnWndProc: Some(pin_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: w!("OrbmentPinWindow"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let live = stream.is_some();
            let ex = WS_EX_TOOLWINDOW | WS_EX_LAYERED;
            let bounds = info.bounds;
            let title = wide(&info.title);
            let hwnd = CreateWindowExW(
                ex,
                w!("OrbmentPinWindow"),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                bounds.x as i32,
                bounds.y as i32,
                bounds.width as i32,
                bounds.height as i32,
                Some(self.owner),
                None,
                Some(instance.into()),
                None,
            )?;
            let target = if live {
                match PresentTarget::new(self.graphics.clone(), hwnd, image.width, image.height) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        let _ = DestroyWindow(hwnd);
                        return Err(e);
                    }
                }
            } else {
                None
            };
            let state = Box::new(RefCell::new(PinState {
                info,
                image,
                transform: document.transform,
                image_revision: next_image_revision(),
                text,
                scene: document.scene,
                ocr: None,
                editor: None,
                target,
                render_cache: None,
                capture_excluded: false,
                stream,
                events: self.events.clone(),
                owner: self.owner,
                show_taskbar: self.config.pin.show_taskbar_icon_when_unpinned,
                text_selectable: self.config.pin.text_selectable_default,
                text_popup: self.config.pin.show_text_popup_menu,
                comments: self.config.mark.arrow.double_click_comment,
                drag: None,
                last_drag_x: 0.0,
                last_direction: 0.0,
                reversals: 0,
                shake_start: Instant::now(),
                tool: String::new(),
                mark_points: Vec::new(),
                edit_drag: None,
                wake: self.wake,
                mark_config: self.config.mark.clone(),
                pending_auto_mosaics: Vec::new(),
                host: hwnd,
                toolbar: HWND::default(),
                toolbar_hover: None,
                keys_down: [false; PIN_KEYS.len()],
                temporary_visibility: Rc::new(TemporaryVisibility {
                    alive: Cell::new(true),
                    ..Default::default()
                }),
                closing: false,
            }));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*state as *const _ as isize);
            let mut pin = PinWindow {
                hwnd,
                toolbar: HWND::default(),
                state,
            };
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(pin_toolbar_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_HAND)?,
                lpszClassName: w!("OrbmentPinActions"),
                ..Default::default()
            });
            pin.toolbar = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("OrbmentPinActions"),
                w!("Orbment"),
                WS_POPUP,
                0,
                0,
                PIN_TOOLBAR_WIDTH,
                PIN_TOOLBAR_HEIGHT,
                Some(hwnd),
                None,
                Some(instance.into()),
                None,
            )?;
            SetWindowLongPtrW(pin.toolbar, GWLP_USERDATA, &*pin.state as *const _ as isize);
            pin.state.borrow_mut().toolbar = pin.toolbar;
            SetWindowDisplayAffinity(pin.toolbar, WDA_EXCLUDEFROMCAPTURE)?;
            {
                let mut state = pin.state.borrow_mut();
                state.styles(hwnd)?;
                if let Some(stream) = &state.stream {
                    stream.set_wake_window(hwnd);
                }
                state.draw(hwnd)?;
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let enabled = DWMNCRP_ENABLED;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_NCRENDERING_POLICY,
                &enabled as *const _ as *const _,
                4,
            );
            let id = pin.state.borrow().info.id.clone();
            if pin.state.borrow().text_selectable
                && matches!(pin.state.borrow().info.kind.as_str(), "image" | "long_shot")
            {
                self.events
                    .borrow_mut()
                    .push(Pending::Action(id.clone(), "vision.pin_select_text".into()));
                if let Some(wake) = self.wake {
                    orbment_ui::wake(HWND(wake as *mut _));
                }
            }
            self.pins.insert(id, pin);
        }
        Ok(())
    }
    fn persist(&self, id: &str) -> Result<()> {
        let pin = self.pins.get(id).context("Pin not found")?;
        let state = pin.state.borrow();
        let relative = format!("media/pins/{id}.png");
        let temporary = self.data.join(format!("media/pins/{id}.tmp"));
        state.image.save_png(&temporary)?;
        std::fs::rename(&temporary, self.data.join(&relative))?;
        drop(state);
        self.persist_meta(id)
    }
    fn persist_meta(&self, id: &str) -> Result<()> {
        let relative = format!("media/pins/{id}.png");
        if !self.data.join(&relative).try_exists()? {
            return self.persist(id);
        }
        let pin = self.pins.get(id).context("Pin not found")?;
        let state = pin.state.borrow();
        let info = &state.info;
        let monitor = crate::capture_overlay::monitors()?.into_iter().find(|m| {
            m.bounds.contains(Point {
                x: info.bounds.x,
                y: info.bounds.y,
            })
        });
        let document = PinDocument {
            scene: state.scene.clone(),
            transform: state.transform,
        };
        self.db.execute("INSERT INTO pin_records(id,pin_kind,group_name,title,image_rel_path,text_content,annotations_json,monitor_name,monitor_dpi,pos_x,pos_y,width,height,scale_pct,opacity_pct,is_topmost,is_locked,is_penetrate,is_thumbnail,is_active_pin,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,1,strftime('%Y-%m-%dT%H:%M:%fZ','now')) ON CONFLICT(id) DO UPDATE SET pin_kind=excluded.pin_kind,group_name=excluded.group_name,title=excluded.title,image_rel_path=excluded.image_rel_path,text_content=excluded.text_content,annotations_json=excluded.annotations_json,monitor_name=excluded.monitor_name,monitor_dpi=excluded.monitor_dpi,pos_x=excluded.pos_x,pos_y=excluded.pos_y,width=excluded.width,height=excluded.height,scale_pct=excluded.scale_pct,opacity_pct=excluded.opacity_pct,is_topmost=excluded.is_topmost,is_locked=excluded.is_locked,is_penetrate=excluded.is_penetrate,is_thumbnail=excluded.is_thumbnail,is_active_pin=1,closed_at=NULL",
            params![id,info.kind,info.group,info.title,relative,state.text,serde_json::to_string(&document)?,monitor.as_ref().map(|m|m.name.as_str()),monitor.as_ref().map_or(96,|m|m.dpi),info.bounds.x as i32,info.bounds.y as i32,info.bounds.width as u32,info.bounds.height as u32,info.scale_pct,info.opacity_pct,info.topmost,info.locked,info.penetrate,info.thumbnail])?;
        Ok(())
    }
    pub fn save(&mut self, id: &str, directory: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(directory)?;
        self.counter += 1;
        let pin = self.pins.get(id).context("Pin not found")?;
        let state = pin.state.borrow();
        let filename = save_template::expand(
            &self.config.save.manual_save_pattern,
            &SaveTemplateContext::now(&state.info.title, self.counter),
        );
        let path = directory.join(filename);
        state.full_resolution_image()?.save_png_unique(&path)
    }
    pub fn close(&mut self, id: &str) -> Result<()> {
        let pin = self.pins.get(id).context("Pin not found")?;
        let finished = pin.state.borrow_mut().finish_editor(pin.hwnd);
        let persistence = finished.and_then(|_| self.persist(id))
            .and_then(|_| {
                self.db.execute("UPDATE pin_records SET is_active_pin=0,closed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",[id])?;
                Ok(())
            });
        if let Err(error) = persistence {
            {
                let mut state = pin.state.borrow_mut();
                state.closing = false;
                state.temporary_visibility.visible.set([true, false]);
            }
            pin.show(true);
            return Err(error);
        }
        self.pins.remove(id);
        self.release_owner_if_empty();
        let history = self.config.pin.max_history_pins;
        let mut statement=self.db.prepare("SELECT id,image_rel_path FROM pin_records WHERE is_active_pin=0 ORDER BY closed_at DESC LIMIT -1 OFFSET ?1")?;
        let old = statement
            .query_map([history], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (id, path) in old {
            if let Some(path) = path {
                let path = self.safe_image_path(&path)?;
                if path.exists() {
                    std::fs::remove_file(path)?;
                }
            }
            self.db
                .execute("DELETE FROM pin_records WHERE id=?1", [id])?;
        }
        Ok(())
    }
    fn safe_image_path(&self, path: &str) -> Result<PathBuf> {
        let relative = Path::new(path);
        ensure!(
            !relative.is_absolute()
                && relative
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "Pin image path escapes data directory"
        );
        Ok(self.data.join(relative))
    }
    fn restore_id(&mut self, id: &str) -> Result<()> {
        let (mut info,path,text,scene,dpi)=self.db.query_row("SELECT pin_kind,group_name,title,pos_x,pos_y,width,height,scale_pct,opacity_pct,is_topmost,is_locked,is_penetrate,is_thumbnail,image_rel_path,text_content,annotations_json,monitor_dpi FROM pin_records WHERE id=?1",[id],|row|Ok((PinInfo{id:id.into(),kind:row.get(0)?,group:row.get(1)?,title:row.get::<_,Option<String>>(2)?.unwrap_or_default(),bounds:Rect{x:row.get::<_,i32>(3)? as f64,y:row.get::<_,i32>(4)? as f64,width:row.get::<_,u32>(5)? as f64,height:row.get::<_,u32>(6)? as f64},scale_pct:row.get(7)?,opacity_pct:row.get(8)?,topmost:row.get(9)?,locked:row.get(10)?,penetrate:row.get(11)?,thumbnail:row.get(12)?,visible:true},row.get::<_,Option<String>>(13)?,row.get::<_,Option<String>>(14)?,row.get::<_,Option<String>>(15)?,row.get::<_,u32>(16)?)))?;
        let image = CapturedImage::load(
            &self.safe_image_path(&path.context("Pin has no persisted image")?)?,
        )?;
        let document = scene
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default();
        let monitors = crate::capture_overlay::monitors()?;
        let monitor = monitors
            .iter()
            .find(|m| m.bounds.intersection(info.bounds).is_some())
            .unwrap_or(&monitors[0]);
        let factor = monitor.dpi as f64 / dpi.max(1) as f64;
        info.scale_pct = (info.scale_pct * factor).clamp(10.0, 800.0);
        info.bounds.width = (info.bounds.width * factor).min(monitor.bounds.width);
        info.bounds.height = (info.bounds.height * factor).min(monitor.bounds.height);
        info.bounds.x = info.bounds.x.clamp(
            monitor.bounds.x,
            monitor.bounds.x + monitor.bounds.width - info.bounds.width,
        );
        info.bounds.y = info.bounds.y.clamp(
            monitor.bounds.y,
            monitor.bounds.y + monitor.bounds.height - info.bounds.height,
        );
        info.bounds = self.cascade(info.bounds)?;
        // HWND values are session-local. A restored live pin is its last persisted frame.
        if info.kind == "window_roi" {
            info.kind = "image".into();
        }
        self.spawn(info, image, text, document, None)?;
        if let Err(error) = self.persist_meta(id) {
            self.pins.remove(id);
            self.release_owner_if_empty();
            return Err(error);
        }
        Ok(())
    }
    pub fn restore(&mut self) -> Result<usize> {
        let ids = self
            .db
            .prepare("SELECT id FROM pin_records WHERE is_active_pin=1 ORDER BY created_at")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut restored = 0;
        for id in ids {
            if !self.pins.contains_key(&id) {
                self.restore_id(&id)?;
                restored += 1;
            }
        }
        Ok(restored)
    }
    pub fn restore_last(&mut self) -> Result<Option<String>> {
        let id = self
            .db
            .query_row(
                "SELECT id FROM pin_records WHERE is_active_pin=0 ORDER BY closed_at DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(id) = &id {
            self.restore_id(id)?;
        }
        Ok(id)
    }
    pub fn show_group(&mut self, group: Option<&str>, visible: bool) -> Result<()> {
        for pin in self.pins.values() {
            if group.is_none_or(|group| pin.state.borrow().info.group == group) {
                pin.show(visible);
            }
        }
        Ok(())
    }
    pub fn set_controls(
        &mut self,
        id: &str,
        topmost: bool,
        locked: bool,
        penetrate: bool,
        thumbnail: bool,
        opacity: f64,
    ) -> Result<()> {
        ensure!(
            opacity.is_finite() && (5.0..=100.0).contains(&opacity),
            "Pin opacity must be 5–100 percent"
        );
        let pin = self.pins.get(id).context("Pin not found")?;
        {
            let mut state = pin.state.borrow_mut();
            state.info.topmost = topmost;
            state.info.locked = locked;
            state.info.penetrate = penetrate;
            state.info.thumbnail = thumbnail;
            state.info.opacity_pct = opacity;
            state.styles(pin.hwnd)?;
            state.draw(pin.hwnd)?;
        }
        self.persist_meta(id)
    }
    pub fn rebuild_graphics(&mut self, graphics: Rc<D2dContext>) -> Result<()> {
        self.graphics = graphics;
        for pin in self.pins.values() {
            let mut state = pin.state.borrow_mut();
            if state.stream.is_some() {
                state.target = None;
                state.target = Some(PresentTarget::new(
                    self.graphics.clone(),
                    pin.hwnd,
                    state.image.width,
                    state.image.height,
                )?);
                state.draw(pin.hwnd)?;
            }
        }
        Ok(())
    }
    pub fn poll(&mut self) -> Vec<PinEvent> {
        let (pending, dirty) = coalesce_dirty(std::mem::take(&mut *self.events.borrow_mut()));
        let mut events = Vec::new();
        for event in pending {
            match event {
                Pending::Dirty(_) | Pending::DirtyMeta(_) => unreachable!(),
                Pending::Close(id) => {
                    if self.pins.contains_key(&id) {
                        match self.close(&id) {
                            Ok(()) => events.push(PinEvent::Closed(id)),
                            Err(error) => {
                                events.push(PinEvent::Error(error.to_string()));
                                if !self.pins.contains_key(&id) {
                                    events.push(PinEvent::Closed(id));
                                }
                            }
                        }
                    }
                }
                Pending::HideOthers(id) => {
                    let others_visible = self
                        .pins
                        .iter()
                        .any(|(other, pin)| other != &id && pin.state.borrow().info.visible);
                    for (other, pin) in &self.pins {
                        if other != &id {
                            pin.show(!others_visible);
                        }
                    }
                }
                Pending::Action(id, action) => events.push(PinEvent::Action { id, action }),
                Pending::Error(error) => events.push(PinEvent::Error(error)),
            }
        }
        for (id, image_changed) in dirty {
            if self.pins.contains_key(&id) {
                let result = if image_changed {
                    self.persist(&id)
                } else {
                    self.persist_meta(&id)
                };
                if let Err(error) = result {
                    events.push(PinEvent::Error(error.to_string()));
                }
            }
        }
        events
    }
    pub fn shutdown(&mut self) -> Result<()> {
        for id in self.pins.keys().cloned().collect::<Vec<_>>() {
            let pin = &self.pins[&id];
            pin.state.borrow_mut().finish_editor(pin.hwnd)?;
            self.persist(&id)?;
        }
        self.pins.clear();
        self.release_owner_if_empty();
        orbment_core::db::checkpoint(&self.db)
    }
}
impl Drop for PinManager {
    fn drop(&mut self) {
        self.pins.clear();
        self.release_owner_if_empty();
    }
}

fn cascade_bounds(mut bounds: Rect, monitor: Rect, occupied: &[Point]) -> Rect {
    let max_x = (monitor.x + monitor.width - bounds.width).max(monitor.x);
    let max_y = (monitor.y + monitor.height - bounds.height).max(monitor.y);
    bounds.x = bounds.x.clamp(monitor.x, max_x);
    bounds.y = bounds.y.clamp(monitor.y, max_y);
    let clear = |x: f64, y: f64| occupied.iter().all(|p| (p.x - x).hypot(p.y - y) >= 16.);
    for _ in 0..=occupied.len() {
        if clear(bounds.x, bounds.y) {
            return bounds;
        }
        bounds.x = if bounds.x + 28. <= max_x {
            bounds.x + 28.
        } else {
            monitor.x
        };
        bounds.y = if bounds.y + 28. <= max_y {
            bounds.y + 28.
        } else {
            monitor.y
        };
    }
    // At an edge the diagonal can cycle; scan distinct in-bounds slots instead.
    let columns = ((max_x - monitor.x) / 28.).floor() as usize + 1;
    let rows = ((max_y - monitor.y) / 28.).floor() as usize + 1;
    for index in 0..columns.saturating_mul(rows).min(occupied.len() + 1) {
        let x = monitor.x + (index % columns) as f64 * 28.;
        let y = monitor.y + (index / columns) as f64 * 28.;
        if clear(x, y) {
            bounds.x = x;
            bounds.y = y;
            break;
        }
    }
    bounds
}

fn card_scale(position: Point) -> Result<f64> {
    ensure!(
        position.x.is_finite() && position.y.is_finite(),
        "Invalid card position"
    );
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: position.x.round() as i32,
                y: position.y.round() as i32,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let (mut x, mut y) = (0, 0);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y)?;
        Ok((f64::from(x) / 96.).max(1.))
    }
}

fn text_card(text: &str, color: Option<[u8; 3]>, scale: f64) -> Result<CapturedImage> {
    ensure!(
        scale.is_finite() && (1.0..=8.0).contains(&scale),
        "Invalid card DPI scale"
    );
    let width = (480. * scale).round() as u32;
    let padding = (16. * scale).round() as i32;
    let swatch = if color.is_some() {
        (120. * scale).round() as i32
    } else {
        0
    };
    let measure = Dib::new(1, 1)?;
    unsafe {
        let font = CreateFontW(
            -(22. * scale).round() as i32,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Microsoft YaHei UI"),
        );
        ensure!(!font.is_invalid(), "Cannot create card font");
        let result = (|| -> Result<CapturedImage> {
            let mut value = wide(text);
            value.pop();
            let mut calc_rect = RECT {
                right: width as i32 - 2 * padding,
                ..Default::default()
            };
            let old = SelectObject(measure.dc, HGDIOBJ(font.0));
            let measured = DrawTextW(
                measure.dc,
                &mut value,
                &mut calc_rect,
                DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL,
            );
            SelectObject(measure.dc, old);
            ensure!(value.is_empty() || measured > 0, "Cannot measure card text");
            let height = (calc_rect.bottom - calc_rect.top + 2 * padding)
                .max((120. * scale).round() as i32)
                .checked_add(swatch)
                .context("Card text height overflow")? as u32;
            // Reject oversized input rather than silently discarding wrapped lines.
            ensure!(
                height <= 32_768 && u64::from(width) * u64::from(height) <= 64 * 1024 * 1024,
                "Card text exceeds the bitmap size limit"
            );
            let dib = Dib::new(width, height)?;
            let pixels =
                std::slice::from_raw_parts_mut(dib.bits, width as usize * height as usize * 4);
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.copy_from_slice(&[245, 248, 255, 255]);
            }
            if let Some([r, g, b]) = color {
                for pixel in pixels[..swatch as usize * width as usize * 4]
                    .as_chunks_mut::<4>()
                    .0
                {
                    pixel.copy_from_slice(&[b, g, r, 255]);
                }
            }
            let old = SelectObject(dib.dc, HGDIOBJ(font.0));
            SetBkMode(dib.dc, TRANSPARENT);
            SetTextColor(dib.dc, COLORREF(0x202020));
            let mut rect = RECT {
                left: padding,
                top: swatch + padding,
                right: width as i32 - padding,
                bottom: height as i32 - padding,
            };
            DrawTextW(
                dib.dc,
                &mut value,
                &mut rect,
                DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL,
            );
            SelectObject(dib.dc, old);
            let mut image = dib.image();
            for pixel in image.pixels.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
            Ok(image)
        })();
        let _ = DeleteObject(HGDIOBJ(font.0));
        result
    }
}

impl PinState {
    fn full_resolution_image(&self) -> Result<CapturedImage> {
        self.transform.apply(
            self.scene
                .render_preview(&self.image, self.editor.as_ref())?,
        )
    }

    fn image_dimensions(&self) -> (u32, u32) {
        self.transform
            .dimensions(self.image.width, self.image.height)
    }

    fn display_scale(&self) -> f64 {
        if self.info.thumbnail {
            (160. / self.image.width.max(self.image.height) as f64).min(1.)
        } else {
            self.info.scale_pct / 100.
        }
    }

    fn clear_image_geometry(&mut self) {
        self.scene = Scene::default();
        self.ocr = None;
        self.editor = None;
        self.mark_points.clear();
        self.edit_drag = None;
        self.pending_auto_mosaics.clear();
        self.render_cache = None;
    }

    fn replace_image(&mut self, image: CapturedImage) {
        self.stream = None;
        if self.target.take().is_some() {
            // UpdateLayeredWindow requires clearing the attribute-based live presentation mode.
            unsafe {
                let style = GetWindowLongPtrW(self.host, GWL_EXSTYLE);
                SetWindowLongPtrW(self.host, GWL_EXSTYLE, style & !(WS_EX_LAYERED.0 as isize));
                SetWindowLongPtrW(self.host, GWL_EXSTYLE, style);
            }
        }
        self.clear_image_geometry();
        self.image = image;
        self.image_revision = next_image_revision();
        self.transform = PinTransform::default();
        self.text = None;
        self.info.kind = "image".into();
        self.tool.clear();
        self.drag = None;
    }

    fn update_frame(&mut self, frame: CapturedImage) -> bool {
        let dimensions_changed =
            frame.width != self.image.width || frame.height != self.image.height;
        if !dimensions_changed && frame.pixels == self.image.pixels {
            return false;
        }
        if dimensions_changed {
            self.clear_image_geometry();
        }
        self.ocr = None;
        self.image_revision = next_image_revision();
        self.image = frame;
        self.render_cache = None;
        true
    }

    fn change_transform(&mut self, action: TransformAction) -> Result<()> {
        self.finish_editor(self.host)?;
        self.finish_mark_edit();
        self.mark_points.clear();
        self.drag = None;
        unsafe {
            if GetCapture() == self.host {
                let _ = ReleaseCapture();
            }
        }
        self.transform.change(action);
        self.render_cache = None;
        self.draw(self.host)?;
        self.queue_dirty_meta();
        Ok(())
    }

    fn input_suspended(&self) -> bool {
        self.closing
            || !self.info.visible
            || self.capture_excluded
            || self.temporary_visibility.depth.get() != 0
    }

    fn handle_key(&mut self, action: PinKeyAction) -> Result<()> {
        match action {
            PinKeyAction::Close => self.queue_close(),
            PinKeyAction::DeleteSelected => {
                if self.scene.selected().is_some() {
                    self.finish_mark_edit();
                    self.scene.delete_selected();
                    unsafe {
                        if GetCapture() == self.host {
                            let _ = ReleaseCapture();
                        }
                    }
                    self.draw(self.host)?;
                    self.queue_dirty();
                }
            }
            PinKeyAction::Transform(action) => self.change_transform(action)?,
            PinKeyAction::Move(delta) => {
                self.info.bounds.x += delta.x;
                self.info.bounds.y += delta.y;
                unsafe {
                    SetWindowPos(
                        self.host,
                        None,
                        self.info.bounds.x as i32,
                        self.info.bounds.y as i32,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    )?;
                }
                self.position_toolbar()?;
                self.queue_dirty_meta();
            }
            PinKeyAction::Event(action) => self
                .events
                .borrow_mut()
                .push(Pending::Action(self.info.id.clone(), action.into())),
            PinKeyAction::Undo | PinKeyAction::Redo => {
                self.finish_mark_edit();
                if action == PinKeyAction::Undo {
                    self.scene.undo();
                } else {
                    self.scene.redo();
                }
                self.draw(self.host)?;
                self.queue_dirty();
            }
        }
        Ok(())
    }

    fn hover_keyboard_allowed(&self) -> bool {
        if self.input_suspended() || self.editor.is_some() || !self.pointer_over_pin() {
            return false;
        }
        unsafe {
            let mut info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            if GetGUIThreadInfo(0, &mut info).is_err() || info.flags.0 & 0x1c != 0 {
                return false;
            }
            // Focused pins receive WM_KEYDOWN; polling must not dispatch a second action.
            if info.hwndFocus == self.host
                || info.hwndFocus == self.toolbar
                || !info.hwndCaret.is_invalid()
            {
                return false;
            }
            let mut class = [0u16; 128];
            let length = GetClassNameW(info.hwndFocus, &mut class).max(0) as usize;
            let class = String::from_utf16_lossy(&class[..length]).to_ascii_lowercase();
            if class.contains("edit") || class == "orbmentpinwindow" || class == "orbmentpinactions"
            {
                return false;
            }
            if info.hwndFocus.is_invalid() {
                return true;
            }
            // Keep keyboard handling out of other controls on our UI thread.
            GetWindowThreadProcessId(info.hwndFocus, None)
                != GetWindowThreadProcessId(self.host, None)
        }
    }

    fn poll_hover_keys(&mut self) -> Result<()> {
        let pressed = PIN_KEYS.map(|key| unsafe { GetAsyncKeyState(i32::from(key)) < 0 });
        let prior = std::mem::replace(&mut self.keys_down, pressed);
        if pressed
            .iter()
            .zip(prior)
            .all(|(down, before)| !*down || before)
            || !self.hover_keyboard_allowed()
        {
            return Ok(());
        }
        let control = unsafe { GetAsyncKeyState(i32::from(VK_CONTROL.0)) < 0 };
        let shift = unsafe { GetAsyncKeyState(i32::from(VK_SHIFT.0)) < 0 };
        let alt = unsafe { GetAsyncKeyState(i32::from(VK_MENU.0)) < 0 };
        for (index, key) in PIN_KEYS.iter().enumerate() {
            if pressed[index]
                && !prior[index]
                && let Some(action) = pin_key_action(
                    *key,
                    control,
                    shift,
                    alt,
                    self.info.locked,
                    self.editor.is_some(),
                )
            {
                self.handle_key(action)?;
                if self.closing {
                    break;
                }
            }
        }
        Ok(())
    }

    fn queue_dirty(&self) {
        self.events
            .borrow_mut()
            .push(Pending::Dirty(self.info.id.clone()));
    }
    fn queue_dirty_meta(&self) {
        self.events
            .borrow_mut()
            .push(Pending::DirtyMeta(self.info.id.clone()));
    }
    fn styles(&self, hwnd: HWND) -> Result<()> {
        unsafe {
            let mut ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            ex &= !WS_EX_NOACTIVATE.0;
            ex |= WS_EX_LAYERED.0;
            if self.info.penetrate || self.capture_excluded {
                ex |= WS_EX_TRANSPARENT.0;
            } else {
                ex &= !WS_EX_TRANSPARENT.0;
            }
            let taskbar = self.show_taskbar && !self.info.topmost;
            if taskbar {
                ex = (ex & !WS_EX_TOOLWINDOW.0) | WS_EX_APPWINDOW.0;
            } else {
                ex = (ex & !WS_EX_APPWINDOW.0) | WS_EX_TOOLWINDOW.0;
            }
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex as isize);
            SetWindowLongPtrW(
                hwnd,
                GWLP_HWNDPARENT,
                if taskbar { 0 } else { self.owner.0 as isize },
            );
            SetWindowPos(
                hwnd,
                Some(if self.info.topmost {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                }),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )?;
            if !self.toolbar.is_invalid() {
                let toolbar_topmost =
                    GetWindowLongPtrW(self.toolbar, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0;
                if toolbar_topmost != self.info.topmost {
                    SetWindowPos(
                        self.toolbar,
                        Some(if self.info.topmost {
                            HWND_TOPMOST
                        } else {
                            HWND_NOTOPMOST
                        }),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    )?;
                }
            }
            Ok(())
        }
    }
    fn prepare_render_cache(&mut self) -> Result<()> {
        let scale = self.display_scale();
        let (width, height) = self.image_dimensions();
        let key = PinRenderKey {
            marks: self.scene.marks().to_vec(),
            selected: self.scene.selected(),
            pending: self.pending_mark().map(|kind| (kind, self.mark_style())),
            editor: self
                .editor
                .as_ref()
                .map(|editor| (editor.mark_id, editor.preview())),
            highlights: self
                .ocr
                .as_ref()
                .map(|ocr| ocr.highlight_rects())
                .unwrap_or_default(),
            width: (width as f64 * scale).max(1.) as u32,
            height: (height as f64 * scale).max(1.) as u32,
            penetrate: self.info.penetrate,
            transform: self.transform,
        };
        if self
            .render_cache
            .as_ref()
            .is_some_and(|cache| cache.key == key)
        {
            return Ok(());
        }
        let mut preview = self.scene.clone();
        if let Some((kind, style)) = &key.pending {
            preview.add(kind.clone(), style.clone());
            preview.clear_selection();
        }
        let mut image = preview.render_with_selection(&self.image, self.editor.as_ref())?;
        for rect in &key.highlights {
            if let Some(rect) =
                rect.intersection(ui_rect(0., 0., image.width as f64, image.height as f64))
            {
                for y in rect.y as usize..(rect.y + rect.height) as usize {
                    for x in rect.x as usize..(rect.x + rect.width) as usize {
                        let pixel = &mut image.pixels[(y * image.width as usize + x) * 4..][..4];
                        pixel[0] /= 2;
                        pixel[1] = (pixel[1] as u16 / 2 + 110).min(255) as u8;
                        pixel[2] = (pixel[2] as u16 / 2 + 110).min(255) as u8;
                    }
                }
            }
        }
        image = self.transform.apply(image)?;
        if key.width != image.width || key.height != image.height {
            let rgba = image::RgbaImage::from_raw(image.width, image.height, image.rgba())
                .context("Pin image dimensions invalid")?;
            let scaled = image::imageops::resize(
                &rgba,
                key.width,
                key.height,
                image::imageops::FilterType::Triangle,
            );
            image = CapturedImage::from_rgba(key.width, key.height, scaled.into_raw())?;
        }
        if key.penetrate {
            for y in 0..key.height {
                for x in 0..key.width {
                    if x < 3 || y < 3 || x + 3 >= key.width || y + 3 >= key.height {
                        let i = (y as usize * key.width as usize + x as usize) * 4;
                        image.pixels[i..i + 4].copy_from_slice(&[0, 130, 255, 255]);
                    }
                }
            }
        }
        let dib = if self.target.is_none() {
            let dib = Dib::new(key.width, key.height)?;
            unsafe {
                let pixels = std::slice::from_raw_parts_mut(dib.bits, image.pixels.len());
                pixels.copy_from_slice(&image.pixels);
                for pixel in pixels.as_chunks_mut::<4>().0 {
                    let alpha = pixel[3] as u16;
                    for channel in &mut pixel[..3] {
                        *channel = (*channel as u16 * alpha / 255) as u8;
                    }
                }
            }
            Some(dib)
        } else {
            None
        };
        self.render_cache = Some(PinRenderCache { key, image, dib });
        Ok(())
    }
    fn draw(&mut self, hwnd: HWND) -> Result<()> {
        if let Some(editor) = &self.editor
            && let Some(mark) = self.scene.mark(editor.mark_id)
        {
            let anchor = Point {
                x: mark.bounds().x,
                y: mark.bounds().y + mark.style.font.size / 2.0,
            };
            let points = mark
                .points()
                .unwrap_or(std::slice::from_ref(&anchor))
                .iter()
                .map(|point| {
                    self.transform.map_point(
                        *point,
                        self.image.width as f64,
                        self.image.height as f64,
                    )
                })
                .collect::<Vec<_>>();
            editor.reposition(hwnd, &points, Point::default(), self.display_scale());
        }
        self.prepare_render_cache()?;
        let cache = self
            .render_cache
            .as_ref()
            .expect("prepared pin render cache");
        let image = &cache.image;
        let (width, height) = (image.width, image.height);
        self.info.bounds.width = width as f64;
        self.info.bounds.height = height as f64;
        let opacity = (if self.info.penetrate {
            self.info.opacity_pct.min(45.0)
        } else {
            self.info.opacity_pct
        }) / 100.0;
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                self.info.bounds.x as i32,
                self.info.bounds.y as i32,
                width as i32,
                height as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
            if let Some(target) = &mut self.target {
                SetLayeredWindowAttributes(hwnd, COLORREF(0), (opacity * 255.0) as u8, LWA_ALPHA)?;
                target.draw(image)?;
            } else {
                let dib = cache.dib.as_ref().expect("static pin bitmap");
                let pos = POINT {
                    x: self.info.bounds.x as i32,
                    y: self.info.bounds.y as i32,
                };
                let size = SIZE {
                    cx: width as i32,
                    cy: height as i32,
                };
                let origin = POINT::default();
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: (opacity * 255.0) as u8,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                UpdateLayeredWindow(
                    hwnd,
                    None,
                    Some(&pos),
                    Some(&size),
                    Some(dib.dc),
                    Some(&origin),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                )?;
            }
        }
        self.position_toolbar()?;
        self.draw_toolbar()?;
        Ok(())
    }
    fn finish_editor(&mut self, hwnd: HWND) -> Result<()> {
        if let Some(mut editor) = self.editor.take() {
            if let EditResult::Commit(comment) =
                editor.handle_message(hwnd, WM_KILLFOCUS, WPARAM(0), LPARAM(0))
            {
                self.scene.apply_comment(editor.mark_id, comment);
            }
            self.scene.commit_edit();
            self.styles(hwnd)?;
            self.queue_dirty();
        }
        Ok(())
    }
    fn position_toolbar(&self) -> Result<()> {
        if self.toolbar.is_invalid() {
            return Ok(());
        }
        let scale = self.toolbar_scale();
        let width = (f64::from(PIN_TOOLBAR_WIDTH) * scale).ceil() as i32;
        let height = (f64::from(PIN_TOOLBAR_HEIGHT) * scale).ceil() as i32;
        unsafe {
            let monitor = MonitorFromWindow(self.host, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            ensure!(
                GetMonitorInfoW(monitor, &mut info).as_bool(),
                "Pin toolbar monitor unavailable"
            );
            let gap = (4. * scale).round() as i32;
            let x = (self.info.bounds.x as i32).clamp(
                info.rcWork.left,
                (info.rcWork.right - width).max(info.rcWork.left),
            );
            let below = (self.info.bounds.y + self.info.bounds.height) as i32 + gap;
            let y = if below + height <= info.rcWork.bottom {
                below
            } else {
                (self.info.bounds.y as i32 - height - gap).max(info.rcWork.top)
            };
            SetWindowPos(
                self.toolbar,
                None,
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )?;
        }
        Ok(())
    }
    fn track_toolbar(&mut self, hwnd: HWND) -> Result<()> {
        unsafe {
            if self.input_suspended() || self.info.penetrate {
                return Ok(());
            }
            if !IsWindowVisible(self.toolbar).as_bool() {
                self.keys_down = PIN_KEYS.map(|key| GetAsyncKeyState(i32::from(key)) < 0);
                let above = GetWindow(self.host, GW_HWNDPREV).unwrap_or(if self.info.topmost {
                    HWND_TOPMOST
                } else {
                    HWND_TOP
                });
                // Show in the owner's existing stack without raising the owner.
                let flags =
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
                if above == self.toolbar {
                    SetWindowPos(self.toolbar, None, 0, 0, 0, 0, flags | SWP_NOZORDER)?;
                } else {
                    let insert = if !self.info.topmost
                        && !above.is_invalid()
                        && GetWindowLongPtrW(above, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0
                    {
                        HWND_TOP
                    } else {
                        above
                    };
                    SetWindowPos(self.toolbar, Some(insert), 0, 0, 0, 0, flags)?;
                }
                SetTimer(Some(self.host), 2, 75, None);
            }
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
        }
        Ok(())
    }
    fn pointer_over_pin(&self) -> bool {
        unsafe {
            let mut point = POINT::default();
            if GetCursorPos(&mut point).is_err() {
                return false;
            }
            let under = WindowFromPoint(point);
            under == self.host || under == self.toolbar
        }
    }
    fn hide_toolbar_outside(&mut self) {
        if !self.pointer_over_pin() {
            unsafe {
                let _ = ShowWindow(self.toolbar, SW_HIDE);
                let _ = KillTimer(Some(self.host), 2);
            }
            self.toolbar_hover = None;
        }
    }
    fn queue_close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.info.visible = false;
        self.temporary_visibility.visible.set([false; 2]);
        unsafe {
            let _ = KillTimer(Some(self.host), 2);
            let _ = ShowWindow(self.toolbar, SW_HIDE);
            let _ = ShowWindow(self.host, SW_HIDE);
        }
        self.events
            .borrow_mut()
            .push(Pending::Close(self.info.id.clone()));
    }
    fn toolbar_scale(&self) -> f64 {
        (f64::from(unsafe { GetDpiForWindow(self.host) }) / 96.).max(1.25)
    }
    fn toolbar_cell(&self, index: usize) -> Rect {
        let scale = self.toolbar_scale();
        ui_rect(
            (14. + index as f64 * 44.) * scale,
            10. * scale,
            40. * scale,
            40. * scale,
        )
    }
    fn toolbar_hit(&self, point: Point) -> Option<usize> {
        (0..PIN_ACTIONS.len()).find(|i| self.toolbar_cell(*i).contains(point))
    }
    fn draw_toolbar(&self) -> Result<()> {
        if self.toolbar.is_invalid() {
            return Ok(());
        }
        let scale = self.toolbar_scale();
        let mut ui = UiCanvas::new(
            (f64::from(PIN_TOOLBAR_WIDTH) * scale).ceil() as u32,
            (f64::from(PIN_TOOLBAR_HEIGHT) * scale).ceil() as u32,
        )?;
        ui.panel(ui_rect(4. * scale, 4. * scale, 316. * scale, 52. * scale));
        for (index, icon) in PIN_ACTIONS.iter().enumerate() {
            let rect = self.toolbar_cell(index);
            let selected = (*icon == ToolbarIcon::Pin && self.info.topmost)
                || (*icon == ToolbarIcon::Pen
                    && (!self.tool.is_empty() || self.scene.selected().is_some()));
            if selected || self.toolbar_hover == Some(index) {
                ui.rounded(
                    rect,
                    6. * scale,
                    if selected { 0xffdbeafe } else { 0xffe2e8f0 },
                    None,
                );
            }
            ui.icon_scaled(
                *icon,
                rect.x + 8. * scale,
                rect.y + 8. * scale,
                scale,
                if selected {
                    0xff1677ff
                } else if *icon == ToolbarIcon::Cancel {
                    0xffdc2626
                } else {
                    0xff334155
                },
            );
        }
        if let Some(index) = self.toolbar_hover {
            let title = match PIN_ACTIONS[index] {
                ToolbarIcon::Pen => "标注与编辑",
                ToolbarIcon::Pin => {
                    if self.info.topmost {
                        "取消置顶"
                    } else {
                        "置顶贴图"
                    }
                }
                ToolbarIcon::Cancel => "关闭贴图 (Esc)",
                icon => icon.title(),
            };
            let rect = self.toolbar_cell(index);
            ui.tooltip(
                title,
                Point {
                    x: rect.x + rect.width / 2.,
                    y: 108. * scale,
                },
                scale,
            )?;
        }
        ui.present_layered(self.toolbar)
    }
    fn begin_mark_edit(&mut self, point: Point) -> bool {
        if self.tool == "eraser" {
            return false;
        }
        let selected_handle = self
            .scene
            .selected_mark()
            .and_then(|mark| mark.handle_at(point, 8.));
        if selected_handle.is_none() && self.scene.select(point, 8.).is_none() {
            return false;
        }
        let mark = self.scene.selected_mark().expect("selected mark").clone();
        let handle = selected_handle.or_else(|| mark.handle_at(point, 8.));
        self.sync_mark_style(&mark);
        self.scene.begin_edit();
        self.edit_drag = Some(MarkEditDrag {
            origin: point,
            handle,
            original: mark,
        });
        true
    }

    fn drag_mark(&mut self, point: Point) -> bool {
        let Some(drag) = &self.edit_drag else {
            return false;
        };
        self.scene.edit_selected(|mark| {
            // Resize from the press snapshot so crossing a handle's opposite edge stays stable.
            *mark = drag.original.clone();
            if point == drag.origin {
                return;
            }
            if let Some(handle) = drag.handle {
                let anchor = drag.original.control_points()[handle];
                mark.resize_by_handle(
                    handle,
                    Point {
                        x: anchor.x + point.x - drag.origin.x,
                        y: anchor.y + point.y - drag.origin.y,
                    },
                );
            } else {
                mark.translate(point.x - drag.origin.x, point.y - drag.origin.y);
            }
        });
        true
    }

    fn finish_mark_edit(&mut self) -> bool {
        if self.edit_drag.take().is_none() {
            return false;
        }
        self.scene.commit_edit();
        if let Some(mark) = self.scene.selected_mark().cloned() {
            self.sync_mark_style(&mark);
        }
        true
    }

    fn sync_mark_style(&mut self, mark: &Mark) {
        let tool = match &mark.kind {
            MarkKind::Rectangle { .. } | MarkKind::RoundedRectangle { .. } => "rectangle",
            MarkKind::Ellipse { .. } => "ellipse",
            MarkKind::Spotlight { .. } => "spotlight",
            MarkKind::Brush { .. } => "pen",
            MarkKind::Highlighter { .. } => "highlighter",
            MarkKind::Arrow { .. } => "arrow",
            MarkKind::Polyline { .. } => "polyline",
            MarkKind::Magnifier { .. } => "magnifier",
            MarkKind::Text { .. } => "text",
            MarkKind::Watermark { .. } => "watermark",
            MarkKind::Sequence { .. } => "sequence",
            MarkKind::Mosaic { .. } => "mosaic",
            MarkKind::AutoMosaic { .. } => "auto_mosaic",
            MarkKind::Eraser { .. } => "eraser",
        };
        let font = &mark.style.font;
        let text = VisionTextConfig {
            font: font.family.clone(),
            size: font.size as f32,
            bold: font.bold,
            italic: font.italic,
            outline: font.outline > 0.,
            outline_width: font.outline as f32,
            outline_color_argb: format!("{:08x}", font.outline_argb),
            background: font.background_argb.is_some(),
            background_color_argb: format!("{:08x}", font.background_argb.unwrap_or(0xffffffff)),
        };
        self.mark_config.common_color_argb = format!("{:08x}", mark.style.argb);
        let stroke = match tool {
            "rectangle" | "ellipse" => &mut self.mark_config.geometry,
            "arrow" => &mut self.mark_config.arrow,
            "polyline" => &mut self.mark_config.broken_line,
            "highlighter" => &mut self.mark_config.marker,
            _ => &mut self.mark_config.pencil,
        };
        stroke.width = mark.style.width as f32;
        stroke.filling = mark.style.fill;
        stroke.text = text.clone();
        match tool {
            "text" => self.mark_config.text = text,
            "watermark" => {
                self.mark_config.watermark.style = text;
                self.mark_config.watermark.opacity_pct = (mark.style.argb >> 24) * 100 / 255;
            }
            "sequence" => self.mark_config.serial.text = text,
            "spotlight" => self.mark_config.high_light.border_width = mark.style.width as f32,
            _ => {}
        }
        self.tool = tool.into();
    }

    fn mark_handle_cursor(&self, mark: &Mark, handle: usize) -> PCWSTR {
        let direction = match &mark.kind {
            MarkKind::Arrow { .. } | MarkKind::Polyline { .. } => return IDC_SIZEALL,
            MarkKind::Magnifier { .. } | MarkKind::Sequence { .. } => Point { x: 1., y: 0. },
            _ => match handle {
                0 | 4 => Point { x: 1., y: 1. },
                2 | 6 => Point { x: 1., y: -1. },
                1 | 5 => Point { x: 0., y: 1. },
                _ => Point { x: 1., y: 0. },
            },
        };
        let map = |point| {
            self.transform
                .map_point(point, self.image.width as f64, self.image.height as f64)
        };
        let origin = map(Point::default());
        let direction = map(direction);
        let (dx, dy) = (direction.x - origin.x, direction.y - origin.y);
        if dx == 0. {
            IDC_SIZENS
        } else if dy == 0. {
            IDC_SIZEWE
        } else if dx * dy > 0. {
            IDC_SIZENWSE
        } else {
            IDC_SIZENESW
        }
    }

    fn pointer_cursor(&self, point: Point, alt: bool) -> PCWSTR {
        if let Some(drag) = &self.edit_drag {
            return drag.handle.map_or(IDC_SIZEALL, |handle| {
                self.mark_handle_cursor(&drag.original, handle)
            });
        }
        if !alt && self.tool != "eraser" && self.mark_points.is_empty() {
            if let Some(mark) = self.scene.selected_mark()
                && let Some(handle) = mark.handle_at(point, 8.)
            {
                return self.mark_handle_cursor(mark, handle);
            }
            if self.scene.hit_test_or_selected(point, 8.).is_some() {
                return IDC_SIZEALL;
            }
        }
        if self.text_selectable
            && self
                .ocr
                .as_ref()
                .is_some_and(|ocr| ocr.text_cursor(point, alt))
        {
            IDC_IBEAM
        } else if !alt && !self.tool.is_empty() {
            IDC_CROSS
        } else {
            IDC_ARROW
        }
    }

    fn point(&self, lparam: LPARAM) -> Point {
        self.source_point(Point {
            x: (lparam.0 as u16 as i16) as f64,
            y: ((lparam.0 >> 16) as u16 as i16) as f64,
        })
    }

    fn source_point(&self, point: Point) -> Point {
        let scale = self.display_scale();
        self.transform.unmap_point(
            Point {
                x: point.x / scale,
                y: point.y / scale,
            },
            self.image.width as f64,
            self.image.height as f64,
        )
    }
    fn menu(&mut self, hwnd: HWND, text_selection: bool) -> Result<()> {
        self.finish_editor(hwnd)?;
        self.finish_mark_edit();
        unsafe {
            let menu = CreatePopupMenu()?;
            let mut entries: Vec<(usize, &str)> = if text_selection {
                vec![
                    (1, "vision.copy_selected"),
                    (2, "vision.copy_all"),
                    (3, "vision.highlight_selection"),
                ]
            } else {
                vec![
                    (10, "vision.pin_close"),
                    (11, "vision.pin_topmost"),
                    (12, "vision.pin_lock"),
                    (13, "vision.pin_click_through"),
                    (14, "vision.pin_thumbnail"),
                    (15, "vision.pin_hide_others"),
                    (24, "vision.copy_full_resolution"),
                    (25, "vision.save_image"),
                    (26, "vision.create_pin"),
                    (27, "vision.replace_pin"),
                    (60, "vision.pin_rotate_right"),
                    (61, "vision.pin_rotate_left"),
                    (62, "vision.pin_flip_horizontal"),
                    (63, "vision.pin_flip_vertical"),
                    (64, "vision.pin_grayscale"),
                    (65, "vision.pin_invert"),
                    (66, "vision.pin_reset_transforms"),
                    (20, "vision.ocr"),
                    (21, "vision.quick_ask"),
                    (22, "vision.record"),
                    (23, "vision.long_capture"),
                    (33, "vision.mark_select"),
                    (34, "vision.mark_undo"),
                    (35, "vision.mark_redo"),
                ]
            };
            if !text_selection {
                let mut tools = vec![
                    (30, "vision.mark_rectangle", "geometry"),
                    (40, "vision.mark_ellipse", "geometry"),
                    (41, "vision.mark_spotlight", "highLight"),
                    (31, "vision.mark_arrow", "arrow"),
                    (42, "vision.mark_polyline", "brokenLine"),
                    (32, "vision.mark_pen", "pencil"),
                    (43, "vision.mark_highlighter", "marker"),
                    (44, "vision.mark_magnifier", "magnifier"),
                    (45, "vision.mark_text", "text"),
                    (46, "vision.mark_watermark", "watermark"),
                    (47, "vision.mark_sequence", "serial"),
                    (48, "vision.mark_mosaic", "mosaic"),
                    (49, "vision.mark_auto_mosaic", "autoMosaic"),
                    (50, "vision.mark_eraser", "eraser"),
                ];
                let order = self
                    .mark_config
                    .edit_item_order
                    .iter()
                    .flatten()
                    .collect::<Vec<_>>();
                tools.sort_by_key(|(_, _, group)| {
                    order
                        .iter()
                        .position(|item| item.as_str() == *group)
                        .unwrap_or(usize::MAX)
                });
                entries.extend(tools.into_iter().map(|(id, key, _)| (id, key)));
            }
            for (id, key) in entries {
                let title = match id {
                    10 => "关闭贴图 (双击 / Esc)".into(),
                    25 => "另存为...".into(),
                    _ => tr(key),
                };
                let shortcut = match id {
                    24 => "Ctrl+C",
                    25 => "Ctrl+S",
                    27 => "Ctrl+V",
                    60 => "R",
                    61 => "L",
                    62 => "H",
                    63 => "V",
                    64 => "G",
                    65 => "I",
                    66 => "0",
                    _ => "",
                };
                let label = wide(&if shortcut.is_empty() {
                    title
                } else {
                    format!("{title}\t{shortcut}")
                });
                let checked =
                    (id == 64 && self.transform.grayscale) || (id == 65 && self.transform.invert);
                let flags = if checked {
                    MF_STRING | MF_CHECKED
                } else {
                    MF_STRING
                };
                if let Err(e) = AppendMenuW(menu, flags, id, PCWSTR(label.as_ptr())) {
                    let _ = DestroyMenu(menu);
                    return Err(e.into());
                }
            }
            let mut cursor = POINT::default();
            GetCursorPos(&mut cursor)?;
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                cursor.x,
                cursor.y,
                Some(0),
                hwnd,
                None,
            )
            .0 as usize;
            let _ = DestroyMenu(menu);
            match command {
                1 => {
                    if let Some(ocr) = &self.ocr {
                        ocr.copy_selected(hwnd.0 as isize)?;
                    }
                }
                2 => {
                    if let Some(ocr) = &self.ocr {
                        ocr.copy_all(hwnd.0 as isize)?;
                    }
                }
                3 => {
                    if let Some(ocr) = &self.ocr {
                        self.scene.highlight(&ocr.highlight_rects(), 0x80ffff00);
                    }
                }
                10 => self.queue_close(),
                11 => self.info.topmost = !self.info.topmost,
                12 => self.info.locked = !self.info.locked,
                13 => self.info.penetrate = !self.info.penetrate,
                14 => self.info.thumbnail = !self.info.thumbnail,
                15 => self
                    .events
                    .borrow_mut()
                    .push(Pending::HideOthers(self.info.id.clone())),
                20..=27 => {
                    let action = match command {
                        20 => "vision.pin_select_text",
                        21 => "quick_ask.capture",
                        22 => "vision.record_start",
                        23 => "vision.longshot",
                        24 => "vision.copy_image",
                        25 => "vision.save_image",
                        27 => "vision.replace_pin",
                        _ => "vision.pin_last",
                    };
                    self.events
                        .borrow_mut()
                        .push(Pending::Action(self.info.id.clone(), action.into()));
                }
                30 => self.tool = "rectangle".into(),
                31 => self.tool = "arrow".into(),
                32 => self.tool = "pen".into(),
                33 => self.tool.clear(),
                34 => {
                    self.scene.undo();
                }
                35 => {
                    self.scene.redo();
                }
                40 => self.tool = "ellipse".into(),
                41 => self.tool = "spotlight".into(),
                42 => self.tool = "polyline".into(),
                43 => self.tool = "highlighter".into(),
                44 => self.tool = "magnifier".into(),
                45 => self.tool = "text".into(),
                46 => self.tool = "watermark".into(),
                47 => self.tool = "sequence".into(),
                48 => self.tool = "mosaic".into(),
                49 => self.tool = "auto_mosaic".into(),
                50 => self.tool = "eraser".into(),
                60..=66 => {
                    let action = match command {
                        60 => TransformAction::RotateRight,
                        61 => TransformAction::RotateLeft,
                        62 => TransformAction::FlipHorizontal,
                        63 => TransformAction::FlipVertical,
                        64 => TransformAction::Grayscale,
                        65 => TransformAction::Invert,
                        _ => TransformAction::Reset,
                    };
                    return self.change_transform(action);
                }
                _ => {}
            }
            self.styles(hwnd)?;
            self.draw(hwnd)?;
            if matches!(command, 3 | 34 | 35) {
                self.queue_dirty();
            } else if matches!(command, 11..=14) {
                self.queue_dirty_meta();
            }
            Ok(())
        }
    }
    fn pending_mark(&self) -> Option<MarkKind> {
        let first = *self.mark_points.first()?;
        let last = *self.mark_points.last()?;
        let bounds = Rect::from_points(first, last);
        if self.mark_points.len() < 2
            && !matches!(
                self.tool.as_str(),
                "text" | "watermark" | "sequence" | "magnifier"
            )
        {
            return None;
        }
        Some(match self.tool.as_str() {
            "rectangle" => MarkKind::Rectangle { bounds },
            "ellipse" => MarkKind::Ellipse { bounds },
            "spotlight" => MarkKind::Spotlight {
                bounds,
                outside_argb: u32::from_str_radix(&self.mark_config.high_light.mask_color_argb, 16)
                    .expect("validated spotlight color"),
            },
            "arrow" => MarkKind::Arrow {
                points: vec![first, last],
                comment: Some(ArrowComment::from(&self.mark_config.arrow)),
            },
            "polyline" => MarkKind::Polyline {
                points: self.mark_points.clone(),
                comment: Some(ArrowComment::from(&self.mark_config.broken_line)),
            },
            "pen" => MarkKind::Brush {
                points: self.mark_points.clone(),
            },
            "highlighter" => MarkKind::Highlighter {
                points: self.mark_points.clone(),
            },
            "eraser" => MarkKind::Eraser {
                points: self.mark_points.clone(),
            },
            "magnifier" => MarkKind::Magnifier {
                center: last,
                radius: self.mark_config.magnifier.radius as f64,
                zoom: self.mark_config.magnifier.scale as f64,
                source: first,
            },
            "text" => MarkKind::Text {
                position: first,
                text: String::new(),
                angle: 0.0,
            },
            "watermark" => {
                let text = self.mark_config.watermark.text.clone();
                let angle = (self.mark_config.watermark.angle as f64).to_radians();
                if self.mark_config.watermark.tiled {
                    MarkKind::Watermark {
                        bounds: if bounds.width < 1.0 || bounds.height < 1.0 {
                            Rect {
                                x: 0.0,
                                y: 0.0,
                                width: self.image.width as f64,
                                height: self.image.height as f64,
                            }
                        } else {
                            bounds
                        },
                        text,
                        angle,
                        spacing: self.mark_config.watermark.spacing as f64,
                    }
                } else {
                    MarkKind::Text {
                        position: first,
                        text: save_template::expand_text(
                            &text,
                            &SaveTemplateContext::now(&self.info.title, 0),
                        ),
                        angle,
                    }
                }
            }
            "sequence" => {
                let prior = if self.mark_config.serial.global_count {
                    SEQUENCE_COUNTER.load(std::sync::atomic::Ordering::Relaxed)
                } else {
                    self.scene
                        .marks()
                        .iter()
                        .filter_map(|mark| match mark.kind {
                            MarkKind::Sequence { number, .. } => Some(number),
                            _ => None,
                        })
                        .max()
                        .unwrap_or(0)
                };
                MarkKind::Sequence {
                    center: first,
                    number: prior.saturating_add(1).max(self.mark_config.serial.start),
                    radius: self.mark_config.serial.text.size as f64,
                }
            }
            "mosaic" => MarkKind::Mosaic {
                bounds,
                mode: mosaic_mode(&self.mark_config.mosaic.mode),
                strength: self.mark_config.mosaic.strength as f64,
            },
            "auto_mosaic" => MarkKind::AutoMosaic {
                regions: self.auto_mosaic_regions(bounds),
                mode: mosaic_mode(&self.mark_config.auto_mosaic.mode),
                strength: self.mark_config.auto_mosaic.strength as f64,
            },
            _ => return None,
        })
    }
    fn commit_mark(&mut self, hwnd: HWND) -> Result<()> {
        if self.tool == "auto_mosaic" && self.ocr.is_none() {
            if let (Some(first), Some(last)) = (self.mark_points.first(), self.mark_points.last()) {
                self.pending_auto_mosaics
                    .push(Rect::from_points(*first, *last));
            }
            self.events.borrow_mut().push(Pending::Action(
                self.info.id.clone(),
                "vision.pin_select_text".into(),
            ));
        } else if let Some(kind) = self.pending_mark() {
            if let MarkKind::Sequence { number, .. } = &kind
                && self.mark_config.serial.global_count
            {
                SEQUENCE_COUNTER.store(*number, std::sync::atomic::Ordering::Relaxed);
            }
            let inline = matches!(self.tool.as_str(), "text" | "watermark");
            if inline {
                self.scene.begin_edit();
            }
            let style = self.mark_style();
            let font = style.font.clone();
            let text = match &kind {
                MarkKind::Text { text, .. } | MarkKind::Watermark { text, .. } => text.clone(),
                _ => String::new(),
            };
            let id = self.scene.add(kind, style);
            if inline {
                self.editor = Some(ArrowCommentEditor::new(
                    id,
                    ArrowComment {
                        text,
                        font,
                        orientation: Orientation::Flat,
                    },
                ));
                self.tool.clear();
                self.styles(hwnd)?;
                unsafe {
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(Some(hwnd));
                }
            }
        }
        self.mark_points.clear();
        Ok(())
    }
    fn mark_style(&self) -> MarkStyle {
        let stroke = match self.tool.as_str() {
            "rectangle" | "ellipse" => &self.mark_config.geometry,
            "arrow" => &self.mark_config.arrow,
            "polyline" => &self.mark_config.broken_line,
            "highlighter" => &self.mark_config.marker,
            _ => &self.mark_config.pencil,
        };
        let mut style = MarkStyle {
            argb: u32::from_str_radix(&self.mark_config.common_color_argb, 16)
                .expect("validated annotation color"),
            width: stroke.width as f64,
            fill: stroke.filling,
            font: FontStyle::from(&stroke.text),
        };
        match self.tool.as_str() {
            "text" => style.font = FontStyle::from(&self.mark_config.text),
            "watermark" => {
                style.font = FontStyle::from(&self.mark_config.watermark.style);
                style.argb = (style.argb & 0xffffff)
                    | ((self.mark_config.watermark.opacity_pct * 255 / 100) << 24);
            }
            "sequence" => style.font = FontStyle::from(&self.mark_config.serial.text),
            "spotlight" => style.width = self.mark_config.high_light.border_width as f64,
            _ => {}
        }
        style
    }
    fn auto_mosaic_regions(&self, bounds: Rect) -> Vec<Rect> {
        self.ocr
            .as_ref()
            .map(|ocr| {
                ocr.lines
                    .iter()
                    .filter_map(|line| {
                        let r = line.bounds;
                        Rect {
                            x: r.x - 2.0,
                            y: r.y - 2.0,
                            width: r.width + 4.0,
                            height: r.height + 4.0,
                        }
                        .intersection(bounds)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    fn add_auto_mosaic(&mut self, bounds: Rect) {
        let regions = self.auto_mosaic_regions(bounds);
        if !regions.is_empty() {
            self.scene.add(
                MarkKind::AutoMosaic {
                    regions,
                    mode: mosaic_mode(&self.mark_config.auto_mosaic.mode),
                    strength: self.mark_config.auto_mosaic.strength as f64,
                },
                self.mark_style(),
            );
        }
    }
}

unsafe extern "system" fn pin_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<PinState>;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let Ok(mut state) = (*ptr).try_borrow_mut() else {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        };
        let result = (|| -> Result<Option<LRESULT>> {
            if state.input_suspended()
                && matches!(
                    message,
                    WM_KEYDOWN
                        | WM_SYSKEYDOWN
                        | WM_CHAR
                        | WM_IME_COMPOSITION
                        | WM_LBUTTONDOWN
                        | WM_LBUTTONDBLCLK
                        | WM_LBUTTONUP
                        | WM_RBUTTONUP
                        | WM_MOUSEWHEEL
                        | WM_MOUSEMOVE
                        | WM_MOUSELEAVE
                )
            {
                return Ok(Some(LRESULT(0)));
            }
            if let Some(editor) = &mut state.editor {
                let id = editor.mark_id;
                match editor.handle_message(hwnd, message, wparam, lparam) {
                    EditResult::Changed => {
                        state.draw(hwnd)?;
                        return Ok(Some(LRESULT(0)));
                    }
                    EditResult::Commit(comment) => {
                        state.scene.apply_comment(id, comment);
                        state.scene.commit_edit();
                        state.editor = None;
                        state.styles(hwnd)?;
                        state.draw(hwnd)?;
                        state.queue_dirty();
                        return Ok(Some(LRESULT(0)));
                    }
                    EditResult::Cancel => {
                        state.scene.cancel_edit();
                        state.editor = None;
                        state.styles(hwnd)?;
                        state.draw(hwnd)?;
                        return Ok(Some(LRESULT(0)));
                    }
                    EditResult::Unhandled => {}
                }
            }
            match message {
                WM_MOUSEACTIVATE => return Ok(Some(LRESULT(MA_ACTIVATE as isize))),
                WM_ERASEBKGND => return Ok(Some(LRESULT(1))),
                WM_PAINT => {
                    let mut ps = PAINTSTRUCT::default();
                    BeginPaint(hwnd, &mut ps);
                    let result = state.draw(hwnd);
                    let _ = EndPaint(hwnd, &ps);
                    result?;
                }
                FRAME_MESSAGE => {
                    if let Some(stream) = &state.stream {
                        let frame = stream.latest();
                        let error = stream.take_error();
                        if let Some(error) = error {
                            state.events.borrow_mut().push(Pending::Error(error));
                        }
                        if let Some(frame) = frame
                            && state.update_frame(frame)
                        {
                            state.draw(hwnd)?;
                        }
                    }
                }
                WM_CLOSE => state.queue_close(),
                WM_KEYDOWN => {
                    if state.editor.is_some() {
                        return Ok(None);
                    }
                    if GetFocus() != state.host && !state.hover_keyboard_allowed() {
                        return Ok(None);
                    }
                    let key = wparam.0 as u16;
                    if let Some(index) = PIN_KEYS.iter().position(|candidate| *candidate == key) {
                        state.keys_down[index] = true;
                    }
                    let Some(action) = pin_key_action(
                        key,
                        GetKeyState(VK_CONTROL.0 as i32) < 0,
                        GetKeyState(VK_SHIFT.0 as i32) < 0,
                        GetKeyState(VK_MENU.0 as i32) < 0,
                        state.info.locked,
                        state.editor.is_some(),
                    ) else {
                        return Ok(None);
                    };
                    state.handle_key(action)?;
                }
                WM_KEYUP => {
                    if let Some(index) = PIN_KEYS
                        .iter()
                        .position(|key| usize::from(*key) == wparam.0)
                    {
                        state.keys_down[index] = false;
                    }
                }
                WM_TIMER if wparam.0 == 2 => {
                    if state.input_suspended() {
                        let _ = KillTimer(Some(hwnd), 2);
                    } else {
                        state.poll_hover_keys()?;
                        state.hide_toolbar_outside();
                    }
                }
                WM_RBUTTONUP => state.menu(hwnd, false)?,
                WM_LBUTTONDBLCLK => {
                    let point = state.point(lparam);
                    let comments = state.comments;
                    let editor = state
                        .scene
                        .double_click_text(point, 8.0)
                        .or_else(|| state.scene.double_click_comment(point, 8.0, comments));
                    if let Some(editor) = editor {
                        state.scene.begin_edit();
                        state.editor = Some(editor);
                        state.styles(hwnd)?;
                        let _ = SetForegroundWindow(hwnd);
                        let _ = SetFocus(Some(hwnd));
                        state.draw(hwnd)?;
                    } else {
                        state.queue_close();
                    }
                }
                WM_LBUTTONDOWN => {
                    let point = state.point(lparam);
                    let alt = GetKeyState(VK_MENU.0 as i32) < 0;
                    if !alt && state.begin_mark_edit(point) {
                        SetCapture(hwnd);
                        state.draw(hwnd)?;
                        return Ok(Some(LRESULT(0)));
                    }
                    let (selection, highlight_changed) = if state.text_selectable {
                        state
                            .ocr
                            .as_mut()
                            .map(|ocr| {
                                let before = ocr.highlight_rects();
                                let action = ocr.pointer_down(point, alt);
                                (Some(action), before != ocr.highlight_rects())
                            })
                            .unwrap_or((None, false))
                    } else {
                        (None, false)
                    };
                    if highlight_changed {
                        state.draw(hwnd)?;
                    }
                    if matches!(selection, Some(PointerAction::SelectText)) {
                        SetCapture(hwnd);
                    } else if !alt && !state.tool.is_empty() {
                        state.scene.clear_selection();
                        state.mark_points = vec![point];
                        SetCapture(hwnd);
                    } else if !state.info.locked {
                        let mut cursor = POINT::default();
                        GetCursorPos(&mut cursor)?;
                        state.drag = Some((
                            Point {
                                x: cursor.x as f64,
                                y: cursor.y as f64,
                            },
                            Point {
                                x: state.info.bounds.x,
                                y: state.info.bounds.y,
                            },
                        ));
                        state.shake_start = Instant::now();
                        state.reversals = 0;
                        state.last_drag_x = cursor.x as f64;
                        SetCapture(hwnd);
                    }
                    state.draw(hwnd)?;
                }
                WM_MOUSEMOVE => {
                    state.track_toolbar(hwnd)?;
                    let point = state.point(lparam);
                    let alt = GetKeyState(VK_MENU.0 as i32) < 0;
                    if !state.mark_points.is_empty() {
                        state.mark_points.push(point);
                        state.draw(hwnd)?;
                    } else if state.drag_mark(point) {
                        state.draw(hwnd)?;
                    } else if let Some((start, origin)) = state.drag {
                        let mut cursor = POINT::default();
                        GetCursorPos(&mut cursor)?;
                        state.info.bounds.x = origin.x + cursor.x as f64 - start.x;
                        state.info.bounds.y = origin.y + cursor.y as f64 - start.y;
                        SetWindowPos(
                            hwnd,
                            None,
                            state.info.bounds.x as i32,
                            state.info.bounds.y as i32,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        )?;
                        state.position_toolbar()?;
                        let delta = cursor.x as f64 - state.last_drag_x;
                        if delta.abs() > 12.0 {
                            if delta.signum() != state.last_direction {
                                state.reversals += 1;
                            }
                            state.last_direction = delta.signum();
                            state.last_drag_x = cursor.x as f64;
                        }
                        if state.reversals >= 4 && state.shake_start.elapsed().as_millis() < 900 {
                            state
                                .events
                                .borrow_mut()
                                .push(Pending::HideOthers(state.info.id.clone()));
                            state.reversals = 0;
                        }
                    } else if state.text_selectable
                        && let Some(ocr) = &mut state.ocr
                    {
                        let previous = ocr.highlight_rects();
                        ocr.pointer_move(point, alt);
                        let changed = previous != ocr.highlight_rects();
                        if changed {
                            state.draw(hwnd)?;
                        }
                    }
                    SetCursor(Some(LoadCursorW(None, state.pointer_cursor(point, alt))?));
                }
                WM_SETCURSOR
                    if !state.input_suspended()
                        && state.editor.is_none()
                        && lparam.0 as u16 as u32 == HTCLIENT =>
                {
                    let mut point = POINT::default();
                    GetCursorPos(&mut point)?;
                    ensure!(
                        ScreenToClient(hwnd, &mut point).as_bool(),
                        "Pin cursor position unavailable"
                    );
                    let point = state.source_point(Point {
                        x: f64::from(point.x),
                        y: f64::from(point.y),
                    });
                    let alt = GetKeyState(VK_MENU.0 as i32) < 0;
                    SetCursor(Some(LoadCursorW(None, state.pointer_cursor(point, alt))?));
                    return Ok(Some(LRESULT(1)));
                }
                WM_CANCELMODE | WM_CAPTURECHANGED => {
                    let editing = state.edit_drag.take().is_some();
                    if editing {
                        state.scene.cancel_edit();
                    }
                    let drawing = !state.mark_points.is_empty();
                    state.mark_points.clear();
                    if state.drag.take().is_some() {
                        state.queue_dirty_meta();
                    }
                    if message == WM_CANCELMODE && GetCapture() == hwnd {
                        let _ = ReleaseCapture();
                    }
                    if editing || drawing {
                        state.draw(hwnd)?;
                    }
                }
                WM_MOUSELEAVE => {
                    SetTimer(Some(hwnd), 2, 180, None);
                }
                WM_LBUTTONUP => {
                    let _ = ReleaseCapture();
                    let point = state.point(lparam);
                    if state.drag_mark(point) && state.finish_mark_edit() {
                        state.draw(hwnd)?;
                        state.queue_dirty();
                    }
                    if !state.mark_points.is_empty() {
                        state.commit_mark(hwnd)?;
                        state.draw(hwnd)?;
                        state.queue_dirty();
                    }
                    if state.drag.take().is_some() {
                        state.queue_dirty_meta();
                    } else {
                        let (action, highlight_changed) = state
                            .ocr
                            .as_mut()
                            .map(|ocr| {
                                let before = ocr.highlight_rects();
                                let action = ocr.pointer_up();
                                (Some(action), before != ocr.highlight_rects())
                            })
                            .unwrap_or((None, false));
                        if highlight_changed {
                            state.draw(hwnd)?;
                        }
                        if matches!(action, Some(PointerAction::SelectionMenu)) && state.text_popup
                        {
                            state.menu(hwnd, true)?;
                        }
                    }
                }
                WM_MOUSEWHEEL => {
                    let delta = ((wparam.0 >> 16) as u16 as i16) as f64 / 120.0;
                    if GetKeyState(VK_CONTROL.0 as i32) < 0 {
                        state.info.opacity_pct =
                            (state.info.opacity_pct + delta * 5.0).clamp(5.0, 100.0);
                    } else {
                        state.info.scale_pct =
                            (state.info.scale_pct * 1.1f64.powf(delta)).clamp(10.0, 800.0);
                        state.info.thumbnail = false;
                    }
                    state.draw(hwnd)?;
                    state.queue_dirty_meta();
                }
                WM_DPICHANGED => {
                    let rect = *(lparam.0 as *const RECT);
                    state.info.bounds.x = rect.left as f64;
                    state.info.bounds.y = rect.top as f64;
                    state.info.scale_pct =
                        (rect.right - rect.left) as f64 / state.image_dimensions().0 as f64 * 100.0;
                    state.draw(hwnd)?;
                    state.queue_dirty_meta();
                }
                _ => return Ok(None),
            }
            Ok(Some(LRESULT(0)))
        })();
        if let Some(wake) = state.wake
            && !state.events.borrow().is_empty()
        {
            orbment_ui::wake(HWND(wake as *mut _));
        }
        match result {
            Ok(Some(value)) => value,
            Ok(None) => {
                drop(state);
                DefWindowProcW(
                    hwnd,
                    message,
                    wparam,
                    if message == WM_IME_SETCONTEXT {
                        orbment_ui::ime::Imm32::context_flags(lparam)
                    } else {
                        lparam
                    },
                )
            }
            Err(error) => {
                state
                    .events
                    .borrow_mut()
                    .push(Pending::Error(error.to_string()));
                if let Some(wake) = state.wake {
                    orbment_ui::wake(HWND(wake as *mut _));
                }
                LRESULT(0)
            }
        }
    }
}

fn mosaic_mode(value: &str) -> MosaicMode {
    match value {
        "Pixel" => MosaicMode::Pixel,
        "Blur" => MosaicMode::Gaussian,
        _ => unreachable!("validated mosaic mode"),
    }
}

unsafe extern "system" fn pin_toolbar_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<PinState>;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let Ok(mut state) = (*ptr).try_borrow_mut() else {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        };
        let result = (|| -> Result<bool> {
            match message {
                WM_MOUSEMOVE | WM_MOUSELEAVE | WM_LBUTTONUP | WM_RBUTTONUP
                    if state.input_suspended() =>
                {
                    return Ok(true);
                }
                WM_MOUSEACTIVATE => return Ok(false),
                WM_ERASEBKGND => return Ok(true),
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    BeginPaint(hwnd, &mut paint);
                    let result = state.draw_toolbar();
                    let _ = EndPaint(hwnd, &paint);
                    result?;
                }
                WM_MOUSEMOVE => {
                    state.track_toolbar(hwnd)?;
                    let point = Point {
                        x: (lparam.0 as u16 as i16) as f64,
                        y: ((lparam.0 >> 16) as u16 as i16) as f64,
                    };
                    let hover = state.toolbar_hit(point);
                    if hover != state.toolbar_hover {
                        state.toolbar_hover = hover;
                        state.draw_toolbar()?;
                    }
                }
                WM_MOUSELEAVE => {
                    state.toolbar_hover = None;
                    state.draw_toolbar()?;
                    SetTimer(Some(state.host), 2, 180, None);
                }
                WM_LBUTTONUP => {
                    let host = state.host;
                    state.finish_editor(host)?;
                    let point = Point {
                        x: (lparam.0 as u16 as i16) as f64,
                        y: ((lparam.0 >> 16) as u16 as i16) as f64,
                    };
                    if let Some(index) = state.toolbar_hit(point) {
                        match PIN_ACTIONS[index] {
                            ToolbarIcon::Pen => state.menu(host, false)?,
                            ToolbarIcon::Pin => {
                                state.info.topmost = !state.info.topmost;
                                state.styles(host)?;
                                state.draw_toolbar()?;
                                state.queue_dirty_meta();
                            }
                            ToolbarIcon::Cancel => state.queue_close(),
                            icon => {
                                let action = match icon {
                                    ToolbarIcon::Copy => "vision.copy_image",
                                    ToolbarIcon::Save => "vision.save_image",
                                    ToolbarIcon::Ocr => "vision.pin_select_text",
                                    ToolbarIcon::QuickAsk => "quick_ask.capture",
                                    _ => unreachable!(),
                                };
                                state
                                    .events
                                    .borrow_mut()
                                    .push(Pending::Action(state.info.id.clone(), action.into()));
                            }
                        }
                    }
                }
                WM_CLOSE => state.queue_close(),
                _ => return Ok(false),
            }
            Ok(true)
        })();
        let handled = match result {
            Ok(value) => value,
            Err(error) => {
                state
                    .events
                    .borrow_mut()
                    .push(Pending::Error(error.to_string()));
                true
            }
        };
        if !state.events.borrow().is_empty()
            && let Some(wake) = state.wake
        {
            orbment_ui::wake(HWND(wake as *mut _));
        }
        drop(state);
        if message == WM_MOUSEACTIVATE {
            LRESULT(MA_NOACTIVATE as isize)
        } else if message == WM_ERASEBKGND {
            LRESULT(1)
        } else if handled {
            LRESULT(0)
        } else {
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }
}

// Native window tests share the desktop foreground window and mouse capture.
#[cfg(test)]
pub(crate) static NATIVE_WINDOW_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cascading_pins_avoids_overlap_and_wraps_inside_negative_coordinate_monitor() {
        let monitor = ui_rect(-1920., -100., 1920., 1080.);
        let bounds = ui_rect(-400., 200., 200., 100.);
        let next = cascade_bounds(
            bounds,
            monitor,
            &[Point {
                x: bounds.x,
                y: bounds.y,
            }],
        );
        assert_eq!((next.x, next.y), (-372., 228.));
        let edge = ui_rect(-200., 880., 200., 100.);
        let occupied = [
            Point { x: -200., y: 880. },
            Point {
                x: -1920.,
                y: -100.,
            },
        ];
        let wrapped = cascade_bounds(edge, monitor, &occupied);
        assert!(
            occupied
                .iter()
                .all(|p| (wrapped.x - p.x).hypot(wrapped.y - p.y) >= 16.)
        );
        assert!(wrapped.x >= monitor.x && wrapped.y >= monitor.y);
        assert!(wrapped.x + wrapped.width <= 0. && wrapped.y + wrapped.height <= 980.);
    }

    #[test]
    fn history_roundtrip_preserves_annotations_and_independent_active_pins() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let directory = std::env::temp_dir().join(format!("orbment-pin-history-{:?}", unsafe {
            CoCreateGuid()?
        }));
        let mut manager = PinManager::new(&directory, Rc::new(D2dContext::new()?))?;
        let image = CapturedImage {
            width: 20,
            height: 20,
            pixels: vec![255; 20 * 20 * 4],
        };
        let first = manager.image_pin(image.clone(), Point { x: 30., y: 30. })?;
        let mut scene = Scene::default();
        scene.add(
            MarkKind::Rectangle {
                bounds: ui_rect(3., 3., 10., 10.),
            },
            MarkStyle {
                argb: 0xfff5222d,
                fill: true,
                ..Default::default()
            },
        );
        manager.set_scene(&first, scene)?;
        let annotated = manager.image(&first)?;
        assert_ne!(annotated.pixels, image.pixels);
        manager.close(&first)?;
        let history = manager.history()?;
        assert_eq!(history.len(), 1);
        assert!(!history[0].is_active);
        assert!(history[0].closed_at.is_some());
        let persisted = history[0].image_path.clone().unwrap();
        assert!(persisted.exists());
        assert_eq!(manager.image(&first)?.pixels, annotated.pixels);
        assert_eq!(manager.restore_by_id(&first)?, first);
        manager.restore_by_id(&first)?;
        assert_eq!(manager.list().len(), 1);
        let bounds = manager.bounds(&first)?;
        let second = manager.image_pin(
            image,
            Point {
                x: bounds.x,
                y: bounds.y,
            },
        )?;
        let other = manager.bounds(&second)?;
        assert!((other.x - bounds.x).hypot(other.y - bounds.y) >= 16.);
        assert!(manager.history()?.iter().all(|entry| entry.is_active));
        manager.close_all()?;
        assert!(manager.list().is_empty());
        assert!(manager.history()?.iter().all(|entry| !entry.is_active));
        manager.delete_history(&first)?;
        assert!(!persisted.exists());
        assert_eq!(manager.history()?.len(), 1);
        manager.restore_by_id(&second)?;
        manager.delete_history(&second)?;
        assert!(manager.list().is_empty());
        assert!(manager.history()?.is_empty());
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn pin_toolbar_starts_hidden_and_double_click_destroys_both_windows() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let directory = std::env::temp_dir().join(format!("orbment-pin-close-{:?}", unsafe {
            CoCreateGuid()?
        }));
        let mut manager = PinManager::new(&directory, Rc::new(D2dContext::new()?))?;
        let id = manager.image_pin(
            CapturedImage {
                width: 2,
                height: 2,
                pixels: vec![255; 16],
            },
            Point { x: 20., y: 20. },
        )?;
        let (hwnd, toolbar) = {
            let pin = &manager.pins[&id];
            (pin.hwnd, pin.toolbar)
        };
        assert!(!unsafe { IsWindowVisible(toolbar).as_bool() });
        manager.set_controls(&id, false, false, false, false, 100.)?;
        assert_eq!(
            unsafe { GetWindowLongPtrW(toolbar, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0,
            0
        );
        unsafe {
            SendMessageW(hwnd, WM_LBUTTONDBLCLK, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        assert!(!unsafe { IsWindowVisible(hwnd).as_bool() });
        let events = manager.poll();
        assert!(
            events
                .iter()
                .any(|event| matches!(event,PinEvent::Closed(closed) if closed==&id))
        );
        assert!(manager.list().is_empty());
        assert!(!unsafe { IsWindow(Some(hwnd)).as_bool() });
        assert!(!unsafe { IsWindow(Some(toolbar)).as_bool() });
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use orbment_core::ocr::{OcrBoundingBox, OcrLine, OcrWord};

    fn annotation_pin() -> Result<(PathBuf, PinManager, String)> {
        let directory =
            std::env::temp_dir().join(format!("orbment-pin-edit-{:?}", unsafe { CoCreateGuid()? }));
        let mut manager = PinManager::new(&directory, Rc::new(D2dContext::new()?))?;
        let id = manager.image_pin(
            CapturedImage {
                width: 256,
                height: 192,
                pixels: vec![255; 256 * 192 * 4],
            },
            Point { x: 40., y: 40. },
        )?;
        manager.pins[&id].state.borrow_mut().info.scale_pct = 100.;
        Ok((directory, manager, id))
    }

    #[test]
    fn failed_close_keeps_annotations_and_windows_available_for_retry() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        for fail_image_write in [true, false] {
            let (directory, mut manager, id) = annotation_pin()?;
            let (hwnd, toolbar, mark) = {
                let pin = &manager.pins[&id];
                let mut state = pin.state.borrow_mut();
                let mark = state.scene.add(
                    MarkKind::Rectangle {
                        bounds: ui_rect(20., 20., 60., 40.),
                    },
                    MarkStyle {
                        fill: true,
                        ..Default::default()
                    },
                );
                (
                    pin.hwnd,
                    pin.toolbar,
                    state.scene.mark(mark).unwrap().clone(),
                )
            };
            let annotated = manager.image(&id)?.pixels;
            let temporary = directory.join(format!("media/pins/{id}.tmp"));
            if fail_image_write {
                std::fs::create_dir(&temporary)?;
            } else {
                manager.db.execute_batch(
                    "CREATE TRIGGER reject_pin_close BEFORE UPDATE OF is_active_pin ON pin_records
                     WHEN NEW.is_active_pin=0
                     BEGIN SELECT RAISE(ABORT, 'history write failed'); END;",
                )?;
            }
            unsafe {
                SendMessageW(hwnd, WM_CLOSE, Some(WPARAM(0)), Some(LPARAM(0)));
            }
            assert!(!unsafe { IsWindowVisible(hwnd).as_bool() });
            let events = manager.poll();
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, PinEvent::Error(_)))
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, PinEvent::Closed(_)))
            );
            let pin = &manager.pins[&id];
            assert_eq!(pin.hwnd, hwnd);
            assert!(unsafe { IsWindow(Some(hwnd)).as_bool() });
            assert!(unsafe { IsWindow(Some(toolbar)).as_bool() });
            assert!(unsafe { IsWindowVisible(hwnd).as_bool() });
            {
                let state = pin.state.borrow();
                assert!(!state.closing);
                assert!(state.info.visible);
                assert_eq!(state.scene.marks(), std::slice::from_ref(&mark));
            }
            assert_eq!(manager.image(&id)?.pixels, annotated);
            assert!(manager.history()?.iter().all(|entry| entry.is_active));
            if fail_image_write {
                std::fs::remove_dir(&temporary)?;
            } else {
                manager.db.execute_batch("DROP TRIGGER reject_pin_close")?;
            }
            unsafe {
                SendMessageW(hwnd, WM_CLOSE, Some(WPARAM(0)), Some(LPARAM(0)));
            }
            let events = manager.poll();
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, PinEvent::Error(_)))
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, PinEvent::Closed(closed) if closed == &id))
            );
            assert!(manager.list().is_empty());
            assert!(!unsafe { IsWindow(Some(hwnd)).as_bool() });
            assert!(!unsafe { IsWindow(Some(toolbar)).as_bool() });
            assert!(manager.history()?.iter().all(|entry| !entry.is_active));
            assert_eq!(manager.image(&id)?.pixels, annotated);
            manager.restore_by_id(&id)?;
            assert_eq!(
                manager.pins[&id].state.borrow().scene.marks(),
                std::slice::from_ref(&mark)
            );
            drop(manager);
            std::fs::remove_dir_all(directory)?;
        }
        Ok(())
    }

    fn mark_pointer(hwnd: HWND, message: u32, point: Point) {
        let packed = u32::from(point.x as i16 as u16) | (u32::from(point.y as i16 as u16) << 16);
        unsafe {
            // Synthetic pointer messages must not inherit modifiers from the live desktop.
            let mut keyboard = [0; 256];
            GetKeyboardState(&mut keyboard).unwrap();
            SetKeyboardState(&[0; 256]).unwrap();
            SendMessageW(
                hwnd,
                message,
                Some(WPARAM(0)),
                Some(LPARAM(packed as isize)),
            );
            SetKeyboardState(&keyboard).unwrap();
            let state = &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<PinState>);
            let state = state.borrow();
            let events = state.events.borrow();
            let errors: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Pending::Error(error) => Some(error),
                    _ => None,
                })
                .collect();
            assert!(
                errors.is_empty(),
                "pointer message {message:#x}: {errors:?}"
            );
        }
    }

    #[test]
    fn selected_handles_preempt_drawing_and_overlap_with_one_resize_undo() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let (directory, manager, id) = annotation_pin()?;
        let pin = &manager.pins[&id];
        let (mark_id, original) = {
            let mut state = pin.state.borrow_mut();
            let mark_id = state.scene.add(
                MarkKind::Rectangle {
                    bounds: ui_rect(40., 40., 80., 60.),
                },
                MarkStyle {
                    argb: 0xffcc2244,
                    width: 5.,
                    ..Default::default()
                },
            );
            state.scene.add(
                MarkKind::Rectangle {
                    bounds: ui_rect(30., 30., 20., 20.),
                },
                MarkStyle {
                    fill: true,
                    ..Default::default()
                },
            );
            assert_eq!(
                state.scene.select(Point { x: 120., y: 70. }, 8.),
                Some(mark_id)
            );
            state.tool = "pen".into();
            (mark_id, state.scene.mark(mark_id).unwrap().clone())
        };
        let press = Point { x: 36., y: 40. };
        mark_pointer(pin.hwnd, WM_LBUTTONDOWN, press);
        mark_pointer(pin.hwnd, WM_LBUTTONUP, press);
        {
            let state = pin.state.borrow();
            assert_eq!(state.scene.selected(), Some(mark_id));
            assert_eq!(state.scene.mark(mark_id), Some(&original));
            assert!(state.mark_points.is_empty() && state.edit_drag.is_none());
            assert_eq!(state.mark_style(), original.style);
        }
        mark_pointer(pin.hwnd, WM_LBUTTONDOWN, press);
        mark_pointer(pin.hwnd, WM_MOUSEMOVE, Point { x: 140., y: 130. });
        mark_pointer(pin.hwnd, WM_MOUSEMOVE, Point { x: 30., y: 30. });
        mark_pointer(pin.hwnd, WM_LBUTTONUP, Point { x: 20., y: 20. });
        {
            let mut state = pin.state.borrow_mut();
            assert_eq!(state.scene.marks().len(), 2);
            assert_eq!(
                state.scene.mark(mark_id).unwrap().bounds(),
                ui_rect(24., 20., 96., 80.)
            );
            assert!(state.scene.undo());
            assert_eq!(state.scene.mark(mark_id), Some(&original));
            assert!(state.scene.undo());
            assert_eq!(state.scene.marks(), &[original]);
            state.tool = "pen".into();
        }
        let edge = Point { x: 40., y: 70. };
        mark_pointer(pin.hwnd, WM_LBUTTONDOWN, edge);
        mark_pointer(pin.hwnd, WM_LBUTTONUP, edge);
        {
            let state = pin.state.borrow();
            assert_eq!(state.scene.selected(), Some(mark_id));
            assert_eq!(state.scene.marks().len(), 1);
            assert!(state.mark_points.is_empty());
        }
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn mark_translation_cancellation_deletion_and_eraser_keep_distinct_gestures() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let (directory, manager, id) = annotation_pin()?;
        let pin = &manager.pins[&id];
        let original = {
            let mut state = pin.state.borrow_mut();
            state.scene.add(
                MarkKind::Rectangle {
                    bounds: ui_rect(40., 40., 80., 60.),
                },
                MarkStyle::default(),
            );
            state.info.locked = true;
            state.tool = "arrow".into();
            state.scene.selected_mark().unwrap().clone()
        };
        let center = Point { x: 80., y: 70. };
        mark_pointer(pin.hwnd, WM_LBUTTONDOWN, center);
        mark_pointer(pin.hwnd, WM_MOUSEMOVE, Point { x: 90., y: 80. });
        mark_pointer(pin.hwnd, WM_LBUTTONUP, Point { x: 95., y: 90. });
        {
            let mut state = pin.state.borrow_mut();
            assert_eq!(state.scene.marks().len(), 1);
            assert_eq!(state.scene.marks()[0].bounds(), ui_rect(55., 60., 80., 60.));
            assert!(state.scene.undo());
            assert_eq!(state.scene.marks(), std::slice::from_ref(&original));
            state.scene.select(Point { x: 40., y: 70. }, 8.);
        }
        mark_pointer(pin.hwnd, WM_LBUTTONDOWN, center);
        mark_pointer(pin.hwnd, WM_MOUSEMOVE, Point { x: 100., y: 90. });
        mark_pointer(pin.hwnd, WM_CANCELMODE, Point::default());
        {
            let mut state = pin.state.borrow_mut();
            assert!(state.edit_drag.is_none());
            assert_eq!(state.scene.marks(), std::slice::from_ref(&original));
            for key in [VK_DELETE.0, VK_BACK.0] {
                state.scene.select(Point { x: 40., y: 70. }, 8.);
                let action = pin_key_action(key, false, false, false, true, false).unwrap();
                state.handle_key(action)?;
                assert!(state.scene.marks().is_empty());
                assert!(!state.closing);
                assert!(state.scene.undo());
                assert_eq!(state.scene.marks(), std::slice::from_ref(&original));
                state.handle_key(action)?;
                assert_eq!(state.scene.marks().len(), 1);
                assert!(!state.closing);
            }
            state.scene.select(Point { x: 40., y: 70. }, 8.);
            state.tool = "eraser".into();
        }
        mark_pointer(pin.hwnd, WM_LBUTTONDOWN, center);
        mark_pointer(pin.hwnd, WM_MOUSEMOVE, Point { x: 90., y: 70. });
        mark_pointer(pin.hwnd, WM_LBUTTONUP, Point { x: 90., y: 70. });
        {
            let state = pin.state.borrow();
            assert_eq!(state.scene.marks().len(), 2);
            assert_eq!(state.scene.marks()[0], original);
            assert!(matches!(
                state.scene.marks()[1].kind,
                MarkKind::Eraser { .. }
            ));
            assert!(state.edit_drag.is_none());
        }
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn selection_decorations_invalidate_display_only_and_cursors_follow_transforms() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let (directory, manager, id) = annotation_pin()?;
        {
            let mut state = manager.pins[&id].state.borrow_mut();
            state.scene.add(
                MarkKind::Rectangle {
                    bounds: ui_rect(40., 40., 80., 60.),
                },
                MarkStyle::default(),
            );
            let corner = Point { x: 40., y: 40. };
            let top = Point { x: 80., y: 40. };
            let center = Point { x: 80., y: 70. };
            assert_eq!(state.pointer_cursor(corner, false), IDC_SIZENWSE);
            assert_eq!(state.pointer_cursor(top, false), IDC_SIZENS);
            assert_eq!(state.pointer_cursor(center, false), IDC_SIZEALL);
            assert_eq!(state.pointer_cursor(corner, true), IDC_ARROW);
            state.prepare_render_cache()?;
            let clean = state.full_resolution_image()?;
            assert_ne!(
                state.render_cache.as_ref().unwrap().image.pixels,
                clean.pixels
            );
            state.scene.clear_selection();
            state.prepare_render_cache()?;
            assert_eq!(
                state.render_cache.as_ref().unwrap().image.pixels,
                clean.pixels
            );
            assert_eq!(state.full_resolution_image()?.pixels, clean.pixels);
            state.scene.select(corner, 8.);
            state.transform.quarter_turns = 1;
            assert_eq!(state.pointer_cursor(top, false), IDC_SIZEWE);
            assert_eq!(state.pointer_cursor(corner, false), IDC_SIZENESW);
            state.transform.flip_horizontal = true;
            assert_eq!(state.pointer_cursor(corner, false), IDC_SIZENWSE);
            state.prepare_render_cache()?;
            assert_ne!(
                state.render_cache.as_ref().unwrap().image.pixels,
                state.full_resolution_image()?.pixels
            );
        }
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn dirty_events_merge_with_image_changes_taking_precedence() {
        let (pending, dirty) = coalesce_dirty(vec![
            Pending::DirtyMeta("a".into()),
            Pending::Dirty("a".into()),
            Pending::DirtyMeta("a".into()),
            Pending::DirtyMeta("b".into()),
            Pending::DirtyMeta("b".into()),
            Pending::Action("a".into(), "copy".into()),
        ]);
        assert_eq!(dirty.len(), 2);
        assert!(dirty["a"]);
        assert!(!dirty["b"]);
        assert!(
            matches!(pending.as_slice(), [Pending::Action(id, action)] if id == "a" && action == "copy")
        );
    }

    #[test]
    fn text_cards_measure_wrapped_lines_and_scale_swatch_and_padding() -> Result<()> {
        let text = "自动换行长文本".repeat(180);
        let image = text_card(&text, None, 1.)?;
        assert_eq!(image.width, 480);
        assert!(image.height > 900);
        let ink_rows = image
            .pixels
            .chunks_exact(image.width as usize * 4)
            .enumerate()
            .filter_map(|(y, row)| {
                row.as_chunks::<4>()
                    .0
                    .iter()
                    .any(|p| *p != [245, 248, 255, 255])
                    .then_some(y)
            })
            .collect::<Vec<_>>();
        assert!(ink_rows.last().copied().unwrap() > 900);
        assert!(ink_rows.last().copied().unwrap() < image.height as usize - 16);
        for scale in [1., 2., 4.] {
            let card = text_card("#123456\nRGB(18, 52, 86)", Some([18, 52, 86]), scale)?;
            assert_eq!(card.width, (480. * scale) as u32);
            let swatch = (120. * scale) as usize;
            let stride = card.width as usize * 4;
            assert_eq!(
                &card.pixels[(swatch - 1) * stride..][..4],
                &[86, 52, 18, 255]
            );
            assert_eq!(&card.pixels[swatch * stride..][..4], &[245, 248, 255, 255]);
        }
        assert!(text_card(&"line\n".repeat(2000), None, 1.).is_err());
        Ok(())
    }

    #[test]
    fn metadata_changes_preserve_png_and_cached_render_while_content_invalidates_it() -> Result<()>
    {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let directory = std::env::temp_dir().join(format!("orbment-pin-cache-{:?}", unsafe {
            CoCreateGuid()?
        }));
        let mut manager = PinManager::new(&directory, Rc::new(D2dContext::new()?))?;
        let id = manager.image_pin(
            CapturedImage {
                width: 64,
                height: 64,
                pixels: vec![255; 64 * 64 * 4],
            },
            Point { x: 40., y: 40. },
        )?;
        let path = directory.join(format!("media/pins/{id}.png"));
        let mut original = std::fs::read(&path)?;
        original.extend_from_slice(b"metadata-must-preserve-original");
        std::fs::write(&path, &original)?;
        {
            let pin = &manager.pins[&id];
            let mut state = pin.state.borrow_mut();
            state.info.scale_pct = 50.;
            state.prepare_render_cache()?;
            let cache = state.render_cache.as_ref().unwrap();
            let pixels = cache.image.pixels.as_ptr();
            let bitmap = cache.dib.as_ref().unwrap().bits;
            state.info.bounds.x += 20.;
            state.info.opacity_pct = 65.;
            state.info.group = "moved".into();
            state.draw(pin.hwnd)?;
            assert_eq!(
                state.render_cache.as_ref().unwrap().image.pixels.as_ptr(),
                pixels
            );
            assert_eq!(
                state
                    .render_cache
                    .as_ref()
                    .unwrap()
                    .dib
                    .as_ref()
                    .unwrap()
                    .bits,
                bitmap
            );
            state.queue_dirty_meta();
            state.queue_dirty_meta();
        }
        assert!(
            !manager
                .poll()
                .iter()
                .any(|event| matches!(event, PinEvent::Error(_)))
        );
        assert_eq!(std::fs::read(&path)?, original);
        let stored: (String, f64, f64) = manager.db.query_row(
            "SELECT group_name,scale_pct,opacity_pct FROM pin_records WHERE id=?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(stored, ("moved".into(), 50., 65.));
        {
            let pin = &manager.pins[&id];
            let mut state = pin.state.borrow_mut();
            let before = state.render_cache.as_ref().unwrap().image.pixels.clone();
            state.scene.add(
                MarkKind::Rectangle {
                    bounds: ui_rect(8., 8., 40., 40.),
                },
                MarkStyle {
                    argb: 0xff0000ff,
                    fill: true,
                    ..Default::default()
                },
            );
            state.prepare_render_cache()?;
            assert_ne!(state.render_cache.as_ref().unwrap().image.pixels, before);
            let bounds = OcrBoundingBox {
                x: 4.,
                y: 4.,
                width: 48.,
                height: 12.,
            };
            state.ocr = Some(PinTextSelection::new(OcrResult {
                text: "text".into(),
                lines: vec![OcrLine {
                    text: "text".into(),
                    bounding_box: bounds,
                    words: vec![OcrWord {
                        text: "text".into(),
                        bounding_box: bounds,
                    }],
                }],
            }));
            state.prepare_render_cache()?;
            let unselected = state.render_cache.as_ref().unwrap().image.pixels.clone();
            let pixels = state.render_cache.as_ref().unwrap().image.pixels.as_ptr();
            state
                .ocr
                .as_mut()
                .unwrap()
                .pointer_move(Point { x: 10., y: 8. }, false);
            state.prepare_render_cache()?;
            assert_eq!(
                state.render_cache.as_ref().unwrap().image.pixels.as_ptr(),
                pixels
            );
            let ocr = state.ocr.as_mut().unwrap();
            ocr.pointer_down(Point { x: 6., y: 8. }, false);
            ocr.pointer_move(Point { x: 45., y: 8. }, false);
            state.prepare_render_cache()?;
            assert_ne!(
                state.render_cache.as_ref().unwrap().image.pixels,
                unselected
            );
            state.info.scale_pct = 100.;
            state.prepare_render_cache()?;
            assert_eq!(state.render_cache.as_ref().unwrap().image.width, 64);
            state.info.penetrate = true;
            state.prepare_render_cache()?;
            assert_eq!(
                &state.render_cache.as_ref().unwrap().image.pixels[..4],
                &[0, 130, 255, 255]
            );
            state.queue_dirty();
            state.queue_dirty_meta();
        }
        assert!(
            !manager
                .poll()
                .iter()
                .any(|event| matches!(event, PinEvent::Error(_)))
        );
        assert_ne!(std::fs::read(&path)?, original);
        let hwnd = manager.pins[&id].hwnd;
        for penetrate in [false, true] {
            manager.pins[&id].state.borrow_mut().info.penetrate = penetrate;
            manager.set_capture_excluded(true)?;
            let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
            assert_eq!(
                ex & (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0),
                WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0
            );
            manager.set_capture_excluded(false)?;
            assert_eq!(
                unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TRANSPARENT.0 != 0,
                penetrate
            );
        }
        std::fs::remove_file(&path)?;
        manager.persist_meta(&id)?;
        assert!(CapturedImage::load(&path).is_ok());
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}

#[cfg(test)]
mod transform_tests {
    use super::*;

    fn patterned_image(width: u32, height: u32) -> CapturedImage {
        CapturedImage {
            width,
            height,
            pixels: (0..width * height)
                .flat_map(|index| {
                    let value = (index % 251) as u8;
                    [value, value.wrapping_add(17), value.wrapping_add(43), 255]
                })
                .collect(),
        }
    }

    #[test]
    fn transforms_map_pixels_and_annotation_points_in_display_space() -> Result<()> {
        let original = patterned_image(3, 2);
        for (action, expected, dimensions) in [
            (TransformAction::RotateRight, vec![3, 0, 4, 1, 5, 2], (2, 3)),
            (TransformAction::RotateLeft, vec![2, 5, 1, 4, 0, 3], (2, 3)),
            (
                TransformAction::FlipHorizontal,
                vec![2, 1, 0, 5, 4, 3],
                (3, 2),
            ),
            (
                TransformAction::FlipVertical,
                vec![3, 4, 5, 0, 1, 2],
                (3, 2),
            ),
        ] {
            let mut transform = PinTransform::default();
            transform.change(action);
            let output = transform.apply(original.clone())?;
            assert_eq!((output.width, output.height), dimensions);
            assert_eq!(
                output
                    .pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|p| p[0])
                    .collect::<Vec<_>>(),
                expected
            );
        }
        for quarter_turns in 0..4 {
            for flip_horizontal in [false, true] {
                for flip_vertical in [false, true] {
                    let transform = PinTransform {
                        quarter_turns,
                        flip_horizontal,
                        flip_vertical,
                        ..Default::default()
                    };
                    let output = transform.apply(original.clone())?;
                    for y in 0..original.height {
                        for x in 0..original.width {
                            let point = Point {
                                x: f64::from(x) + 0.5,
                                y: f64::from(y) + 0.5,
                            };
                            let mapped = transform.map_point(point, 3., 2.);
                            assert_eq!(transform.unmap_point(mapped, 3., 2.), point);
                            let offset =
                                (mapped.y as usize * output.width as usize + mapped.x as usize) * 4;
                            let source = (y as usize * original.width as usize + x as usize) * 4;
                            assert_eq!(
                                &output.pixels[offset..offset + 4],
                                &original.pixels[source..source + 4]
                            );
                        }
                    }
                    for action in [
                        TransformAction::FlipHorizontal,
                        TransformAction::FlipVertical,
                    ] {
                        let mut flipped = transform;
                        flipped.change(action);
                        let expected = PinTransform {
                            flip_horizontal: action == TransformAction::FlipHorizontal,
                            flip_vertical: action == TransformAction::FlipVertical,
                            ..Default::default()
                        }
                        .apply(output.clone())?;
                        assert_eq!(flipped.apply(original.clone())?.pixels, expected.pixels);
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn color_modes_preserve_alpha_and_reset_restores_the_source() -> Result<()> {
        let image = CapturedImage {
            width: 2,
            height: 1,
            pixels: vec![20, 70, 200, 64, 5, 15, 25, 0],
        };
        let mut transform = PinTransform::default();
        transform.change(TransformAction::Grayscale);
        let gray = transform.apply(image.clone())?;
        assert_eq!(&gray.pixels[..4], &[103, 103, 103, 64]);
        assert_eq!(gray.pixels[7], 0);
        transform.change(TransformAction::Invert);
        let inverted = transform.apply(image.clone())?;
        assert_eq!(&inverted.pixels[..4], &[152, 152, 152, 64]);
        assert_eq!(inverted.pixels[7], 0);
        transform.change(TransformAction::Grayscale);
        assert_eq!(
            &transform.apply(image.clone())?.pixels[..4],
            &[235, 185, 55, 64]
        );
        transform.change(TransformAction::RotateRight);
        transform.change(TransformAction::FlipHorizontal);
        transform.change(TransformAction::Reset);
        let reset = transform.apply(image.clone())?;
        assert_eq!((reset.width, reset.height), (2, 1));
        assert_eq!(reset.pixels, image.pixels);
        Ok(())
    }

    #[test]
    fn legacy_annotation_records_default_to_an_untransformed_view() -> Result<()> {
        let mut scene = Scene::default();
        scene.add(
            MarkKind::Rectangle {
                bounds: ui_rect(1., 2., 3., 4.),
            },
            MarkStyle::default(),
        );
        let document: PinDocument = serde_json::from_str(&serde_json::to_string(&scene)?)?;
        assert_eq!(document.transform, PinTransform::default());
        assert_eq!(document.scene.marks(), scene.marks());
        Ok(())
    }

    #[test]
    fn scaled_thumbnail_export_history_and_reset_share_full_resolution_transforms() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let directory = std::env::temp_dir().join(format!("orbment-pin-transform-{:?}", unsafe {
            CoCreateGuid()?
        }));
        let mut manager = PinManager::new(&directory, Rc::new(D2dContext::new()?))?;
        let id = manager.image_pin(patterned_image(320, 180), Point { x: 60., y: 60. })?;
        let revision = manager.image_revision(&id)?;
        let mut scene = Scene::default();
        scene.add(
            MarkKind::Rectangle {
                bounds: ui_rect(12., 24., 60., 40.),
            },
            MarkStyle {
                argb: 0xffe03050,
                fill: true,
                ..Default::default()
            },
        );
        manager.set_scene(&id, scene)?;
        let baseline = manager.image(&id)?;
        let expected_transform = PinTransform {
            quarter_turns: 1,
            flip_vertical: true,
            grayscale: true,
            invert: true,
            ..Default::default()
        };
        let expected = expected_transform.apply(baseline.clone())?;
        {
            let pin = &manager.pins[&id];
            let mut state = pin.state.borrow_mut();
            state.info.scale_pct = 25.;
            for action in [
                TransformAction::RotateRight,
                TransformAction::FlipHorizontal,
                TransformAction::Grayscale,
                TransformAction::Invert,
            ] {
                state.change_transform(action)?;
            }
            assert_eq!(state.transform, expected_transform);
            assert_eq!(
                (state.info.bounds.width, state.info.bounds.height),
                (45., 80.)
            );
            assert_eq!(state.full_resolution_image()?.pixels, expected.pixels);
            state.info.thumbnail = true;
            state.draw(pin.hwnd)?;
            assert_eq!(
                (state.info.bounds.width, state.info.bounds.height),
                (90., 160.)
            );
            assert_eq!(state.full_resolution_image()?.pixels, expected.pixels);
        }
        assert_eq!(manager.image_revision(&id)?, revision);
        assert_eq!(manager.ocr_image(&id)?.pixels, baseline.pixels);
        let full = manager.image(&id)?;
        assert_eq!((full.width, full.height), (180, 320));
        assert_eq!(full.pixels, expected.pixels);
        let saved = manager.save(&id, &directory.join("exports"))?;
        let saved = CapturedImage::load(&saved)?;
        assert_eq!((saved.width, saved.height), (180, 320));
        assert_eq!(saved.pixels, expected.pixels);
        manager.close(&id)?;
        assert_eq!(manager.image(&id)?.pixels, expected.pixels);
        manager.restore_by_id(&id)?;
        assert_ne!(manager.image_revision(&id)?, revision);
        assert_eq!(manager.image(&id)?.pixels, expected.pixels);
        {
            let mut state = manager.pins[&id].state.borrow_mut();
            state.change_transform(TransformAction::Reset)?;
            assert_eq!(state.scene.marks().len(), 1);
        }
        let reset = manager.image(&id)?;
        assert_eq!((reset.width, reset.height), (320, 180));
        assert_eq!(reset.pixels, baseline.pixels);
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn identical_live_frames_preserve_revision_ocr_and_cache_until_content_changes() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let directory = std::env::temp_dir().join(format!("orbment-pin-frame-{:?}", unsafe {
            CoCreateGuid()?
        }));
        let mut manager = PinManager::new(&directory, Rc::new(D2dContext::new()?))?;
        let original = patterned_image(80, 60);
        let id = manager.image_pin(original.clone(), Point { x: 60., y: 60. })?;
        {
            let pin = &manager.pins[&id];
            let mut state = pin.state.borrow_mut();
            state.scene.add(
                MarkKind::Rectangle {
                    bounds: ui_rect(2., 2., 30., 20.),
                },
                MarkStyle {
                    fill: true,
                    ..Default::default()
                },
            );
            state.change_transform(TransformAction::RotateRight)?;
            state.ocr = Some(PinTextSelection::new(OcrResult::default()));
            state.prepare_render_cache()?;
            let revision = state.image_revision;
            let source_pixels = state.image.pixels.as_ptr();
            let rendered_pixels = state.render_cache.as_ref().unwrap().image.pixels.as_ptr();
            assert!(!state.update_frame(original.clone()));
            assert_eq!(state.image_revision, revision);
            assert_eq!(state.image.pixels.as_ptr(), source_pixels);
            assert_eq!(
                state.render_cache.as_ref().unwrap().image.pixels.as_ptr(),
                rendered_pixels,
            );
            assert!(state.ocr.is_some());
            assert_eq!(state.scene.marks().len(), 1);

            let mut changed = original.clone();
            changed.pixels[0] ^= 0xff;
            assert!(state.update_frame(changed.clone()));
            assert_ne!(state.image_revision, revision);
            assert!(state.ocr.is_none() && state.render_cache.is_none());
            assert_eq!(state.scene.marks().len(), 1);
            assert_eq!(state.transform.quarter_turns, 1);
            let revision = state.image_revision;
            assert!(!state.update_frame(changed.clone()));
            assert_eq!(state.image_revision, revision);

            changed.width = 60;
            changed.height = 80;
            assert!(state.update_frame(changed));
            assert_ne!(state.image_revision, revision);
            assert!(state.scene.marks().is_empty());
            assert_eq!(state.image_dimensions(), (80, 60));
        }
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn replacement_resets_image_geometry_and_advances_the_source_revision() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let directory = std::env::temp_dir().join(format!("orbment-pin-replace-{:?}", unsafe {
            CoCreateGuid()?
        }));
        let mut manager = PinManager::new(&directory, Rc::new(D2dContext::new()?))?;
        let id = manager.image_pin(patterned_image(80, 60), Point { x: 60., y: 60. })?;
        let revision = manager.image_revision(&id)?;
        {
            let pin = &manager.pins[&id];
            let mut state = pin.state.borrow_mut();
            state.scene.add(
                MarkKind::Rectangle {
                    bounds: ui_rect(2., 2., 30., 20.),
                },
                MarkStyle {
                    fill: true,
                    ..Default::default()
                },
            );
            state.change_transform(TransformAction::RotateRight)?;
            state.info.thumbnail = true;
            state.info.scale_pct = 50.;
            state.ocr = Some(PinTextSelection::new(OcrResult::default()));
            state.editor = Some(ArrowCommentEditor::new(1, ArrowComment::default()));
            state.pending_auto_mosaics.push(ui_rect(0., 0., 80., 60.));
            state.mark_points.push(Point { x: 12., y: 18. });
            state.info.kind = "text".into();
            state.text = Some("previous text".into());
        }
        let replacement = patterned_image(24, 40);
        manager.replace_image(&id, replacement.clone())?;
        assert_ne!(manager.image_revision(&id)?, revision);
        let output = manager.image(&id)?;
        assert_eq!((output.width, output.height), (24, 40));
        assert_eq!(output.pixels, replacement.pixels);
        {
            let state = manager.pins[&id].state.borrow();
            assert!(state.scene.marks().is_empty());
            assert!(state.ocr.is_none() && state.editor.is_none());
            assert!(state.mark_points.is_empty() && state.pending_auto_mosaics.is_empty());
            assert!(state.text.is_none() && state.stream.is_none());
            assert_eq!(state.info.kind, "image");
            assert_eq!(state.transform, PinTransform::default());
            let cache = state.render_cache.as_ref().unwrap();
            assert_eq!((cache.image.width, cache.image.height), (24, 40));
        }
        assert_eq!(
            manager.db.query_row(
                "SELECT pin_kind FROM pin_records WHERE id=?1",
                [&id],
                |row| row.get::<_, String>(0),
            )?,
            "image"
        );
        let revision = manager.image_revision(&id)?;
        assert!(
            manager
                .replace_image(
                    &id,
                    CapturedImage {
                        width: 1,
                        height: 1,
                        pixels: vec![]
                    }
                )
                .is_err()
        );
        assert_eq!(manager.image_revision(&id)?, revision);
        assert_eq!(manager.image(&id)?.pixels, replacement.pixels);
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn keyboard_actions_respect_text_input_lock_modifiers_and_movement_steps() {
        for key in PIN_KEYS {
            for control in [false, true] {
                assert_eq!(
                    pin_key_action(key, control, false, false, false, true),
                    None
                );
                assert_eq!(
                    pin_key_action(key, control, false, true, false, false),
                    None
                );
            }
        }
        for (key, delta) in [
            (VK_LEFT.0, Point { x: -1., y: 0. }),
            (VK_RIGHT.0, Point { x: 1., y: 0. }),
            (VK_UP.0, Point { x: 0., y: -1. }),
            (VK_DOWN.0, Point { x: 0., y: 1. }),
        ] {
            assert_eq!(
                pin_key_action(key, false, false, false, false, false),
                Some(PinKeyAction::Move(delta))
            );
            assert_eq!(
                pin_key_action(key, false, true, false, false, false),
                Some(PinKeyAction::Move(Point {
                    x: delta.x * 10.,
                    y: delta.y * 10.
                },))
            );
            assert_eq!(pin_key_action(key, false, false, false, true, false), None);
            assert_eq!(pin_key_action(key, true, false, false, false, false), None);
        }
        assert_eq!(
            pin_key_action(u16::from(b'R'), false, false, false, true, false),
            Some(PinKeyAction::Transform(TransformAction::RotateRight))
        );
        assert_eq!(
            pin_key_action(u16::from(b'V'), true, false, false, false, false),
            Some(PinKeyAction::Event("vision.replace_pin"))
        );
        assert_eq!(
            pin_key_action(u16::from(b'C'), true, false, false, false, false),
            Some(PinKeyAction::Event("vision.copy_image"))
        );
    }

    #[test]
    fn keyboard_movement_queues_metadata_and_inline_text_does_not_transform() -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let directory = std::env::temp_dir().join(format!("orbment-pin-keyboard-{:?}", unsafe {
            CoCreateGuid()?
        }));
        let mut manager = PinManager::new(&directory, Rc::new(D2dContext::new()?))?;
        let id = manager.image_pin(patterned_image(80, 60), Point { x: 100., y: 100. })?;
        manager.events.borrow_mut().clear();
        let hwnd = manager.pins[&id].hwnd;
        let original = manager.bounds(&id)?;
        {
            let mut state = manager.pins[&id].state.borrow_mut();
            state.handle_key(
                pin_key_action(VK_RIGHT.0, false, false, false, false, false).unwrap(),
            )?;
            state.handle_key(pin_key_action(VK_UP.0, false, true, false, false, false).unwrap())?;
            assert_eq!(
                (state.info.bounds.x, state.info.bounds.y),
                (original.x + 1., original.y - 10.)
            );
            assert!(
                state
                    .events
                    .borrow()
                    .iter()
                    .all(|event| matches!(event, Pending::DirtyMeta(pin) if pin == &id))
            );
            state.editor = Some(ArrowCommentEditor::new(0, ArrowComment::default()));
        }
        unsafe {
            for key in *b"RLHVGI0" {
                SendMessageW(
                    hwnd,
                    WM_KEYDOWN,
                    Some(WPARAM(usize::from(key))),
                    Some(LPARAM(0)),
                );
            }
            SendMessageW(
                hwnd,
                WM_KEYDOWN,
                Some(WPARAM(usize::from(VK_UP.0))),
                Some(LPARAM(0)),
            );
        }
        {
            let mut state = manager.pins[&id].state.borrow_mut();
            assert_eq!(state.transform, PinTransform::default());
            assert_eq!(
                (state.info.bounds.x, state.info.bounds.y),
                (original.x + 1., original.y - 10.)
            );
            state.editor = None;
        }
        assert!(
            !manager
                .poll()
                .iter()
                .any(|event| matches!(event, PinEvent::Error(_)))
        );
        let stored = manager.db.query_row(
            "SELECT pos_x,pos_y FROM pin_records WHERE id=?1",
            [&id],
            |row| Ok((row.get::<_, i32>(0)?, row.get::<_, i32>(1)?)),
        )?;
        assert_eq!(
            stored,
            ((original.x + 1.) as i32, (original.y - 10.) as i32)
        );
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn save_visibility_restores_only_prior_windows_after_nested_cancel_error_and_close()
    -> Result<()> {
        let _desktop = NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let directory = std::env::temp_dir().join(format!("orbment-pin-visibility-{:?}", unsafe {
            CoCreateGuid()?
        }));
        let mut manager = PinManager::new(&directory, Rc::new(D2dContext::new()?))?;
        let id = manager.image_pin(patterned_image(80, 60), Point { x: 60., y: 60. })?;
        let (hwnd, toolbar) = {
            let pin = &manager.pins[&id];
            (pin.hwnd, pin.toolbar)
        };
        let visible = |hwnd| unsafe { IsWindowVisible(hwnd).as_bool() };
        assert!(visible(hwnd) && !visible(toolbar));
        {
            let _guard = manager.hide_for_save(&id)?;
            assert!(!visible(hwnd) && !visible(toolbar));
            assert!(manager.pins[&id].state.borrow().info.visible);
            manager.pins[&id].state.borrow_mut().track_toolbar(hwnd)?;
            unsafe {
                SendMessageW(hwnd, WM_TIMER, Some(WPARAM(2)), Some(LPARAM(0)));
                SendMessageW(toolbar, WM_MOUSEMOVE, Some(WPARAM(0)), Some(LPARAM(0)));
            }
            assert!(!visible(hwnd) && !visible(toolbar));
        }
        assert!(visible(hwnd) && !visible(toolbar));
        unsafe {
            let _ = ShowWindow(toolbar, SW_SHOWNOACTIVATE);
        }
        assert!(visible(toolbar));
        let outer = manager.hide_for_save(&id)?;
        let inner = manager.hide_for_save(&id)?;
        drop(outer);
        assert!(!visible(hwnd) && !visible(toolbar));
        drop(inner);
        assert!(visible(hwnd) && visible(toolbar));
        let failure: Result<()> = (|| {
            let _guard = manager.hide_for_save(&id)?;
            ensure!(visible(hwnd), "simulated save failure");
            Ok(())
        })();
        assert!(failure.is_err());
        assert!(visible(hwnd) && visible(toolbar));
        manager.show_group(None, false)?;
        {
            let _guard = manager.hide_for_save(&id)?;
        }
        assert!(!visible(hwnd) && !visible(toolbar));
        manager.show_group(None, true)?;
        let guard = manager.hide_for_save(&id)?;
        manager.close(&id)?;
        drop(guard);
        assert!(!unsafe { IsWindow(Some(hwnd)).as_bool() });
        assert!(!unsafe { IsWindow(Some(toolbar)).as_bool() });
        drop(manager);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
