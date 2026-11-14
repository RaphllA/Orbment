use crate::{
    D2dContext,
    d2d_context::{TextRun, TextStyle, draw},
};
use anyhow::{Context, Result, ensure};
use orbment_core::config::TaskbarConfig;
use std::{
    cell::{Cell, RefCell},
    mem::size_of,
    rc::Rc,
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{
                Common::{D2D1_COLOR_F, D2D_RECT_F},
                D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
                ID2D1DCRenderTarget,
            },
            DirectWrite::{
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE,
                IDWriteTextFormat,
            },
            Gdi::*,
        },
        System::{
            LibraryLoader::GetModuleHandleW,
            Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        },
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    core::{BOOL, w},
};

const OSD_TIMER: usize = 2;
const LAYOUT_MESSAGE: u32 = WM_APP + 10;

#[derive(Clone)]
pub(crate) struct OsdPlacement {
    pub position: String,
    pub custom: [i32; 2],
    pub primary: bool,
    pub stack: usize,
    pub opacity: u8,
}

impl Default for OsdPlacement {
    fn default() -> Self {
        Self {
            position: "TopCenter".into(),
            custom: [0, 0],
            primary: false,
            stack: 0,
            opacity: 100,
        }
    }
}

struct Appearance {
    text: String,
    font: String,
    size: f32,
    foreground: u32,
    background: u32,
    style: u32,
    transparent: Option<u32>,
    on_left: bool,
    snap: bool,
}

struct Surface {
    hwnd: Cell<HWND>,
    context: Rc<D2dContext>,
    appearance: RefCell<Appearance>,
    renderer: RefCell<Option<Renderer>>,
    taskbar: bool,
    osd: RefCell<OsdPlacement>,
    runs: RefCell<Vec<TextRun>>,
    grid: RefCell<Option<TaskbarGrid>>,
}

struct Renderer {
    target: ID2D1DCRenderTarget,
    format: IDWriteTextFormat,
    bitmap: Option<Bitmap>,
}

struct TaskbarGrid {
    config: TaskbarConfig,
    items: Vec<[TextRun; 2]>,
    light_theme: bool,
}

struct GridMetrics {
    runs: Vec<[(f32, f32); 2]>,
    columns: Vec<f32>,
    row_height: f32,
    row_step: f32,
    rows: usize,
}

impl GridMetrics {
    fn dimensions(&self, config: &TaskbarConfig) -> (f32, f32) {
        (
            self.columns.iter().sum::<f32>()
                + self.columns.len().saturating_sub(1) as f32 * (config.item_space as f32 + 6.0),
            self.row_height + self.rows.saturating_sub(1) as f32 * self.row_step,
        )
    }
}

impl TaskbarGrid {
    fn transparent(&self) -> bool {
        !self.config.auto_set_background_color
            && (self.config.transparent_background
                || self.config.back_color == self.config.transparent_color)
    }

    fn measure(
        &self,
        context: &D2dContext,
        format: &IDWriteTextFormat,
    ) -> windows::core::Result<GridMetrics> {
        let rows = if !self.config.horizontal_arrange && self.items.len() > 1 {
            2
        } else {
            1
        };
        let mut runs = Vec::with_capacity(self.items.len());
        let mut columns = vec![0.0_f32; self.items.len().div_ceil(rows)];
        let mut row_height = 0.0_f32;
        for (index, item) in self.items.iter().enumerate() {
            let mut sizes = [(0.0, 0.0); 2];
            for (run, size) in item.iter().zip(&mut sizes) {
                let text: Vec<u16> = run.text.encode_utf16().collect();
                unsafe {
                    let layout =
                        context.write_factory().CreateTextLayout(&text, format, 4096.0, 1024.0)?;
                    let mut metrics = DWRITE_TEXT_METRICS::default();
                    layout.GetMetrics(&mut metrics)?;
                    *size = (metrics.widthIncludingTrailingWhitespace, metrics.height);
                    row_height = row_height.max(metrics.height);
                }
            }
            columns[index / rows] =
                columns[index / rows].max(sizes[0].0 + sizes[1].0);
            runs.push(sizes);
        }
        Ok(GridMetrics {
            runs,
            columns,
            row_height,
            row_step: (row_height + self.config.vertical_margin as f32).max(row_height / 2.0),
            rows,
        })
    }
}

fn system_uses_light_theme() -> bool {
    let mut value = 0u32;
    let mut bytes = size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut bytes),
        ) == ERROR_SUCCESS
            && value != 0
    }
}

fn taskbar_color(value: u32, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: (value & 255) as f32 / 255.0,
        g: ((value >> 8) & 255) as f32 / 255.0,
        b: ((value >> 16) & 255) as f32 / 255.0,
        a: alpha,
    }
}

fn draw_taskbar(
    context: &D2dContext,
    target: &ID2D1DCRenderTarget,
    bitmap: &Bitmap,
    dpi: u32,
    style: &TextStyle<'_>,
    grid: &TaskbarGrid,
    metrics: &GridMetrics,
) -> windows::core::Result<()> {
    unsafe {
        let bounds = RECT {
            left: 0,
            top: 0,
            right: bitmap.width,
            bottom: bitmap.height,
        };
        target.BindDC(bitmap.dc, &bounds)?;
        target.SetDpi(dpi as f32, dpi as f32);
        target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let scale = 96.0 / dpi as f32;
        let transparent = grid.transparent();
        let background = if transparent {
            taskbar_color(0, 0.0)
        } else if grid.config.auto_set_background_color {
            taskbar_color(if grid.light_theme { 0x00f3_f3f3 } else { 0x0020_2020 }, 1.0)
        } else {
            taskbar_color(grid.config.back_color, 1.0)
        };
        let (width, height) = metrics.dimensions(&grid.config);
        let mut column_x = ((bitmap.width as f32 * scale - width) / 2.0).max(0.0);
        let top = ((bitmap.height as f32 * scale - height) / 2.0).max(0.0);
        let mut text_draws = Vec::new();
        let mut fills = Vec::new();
        for (index, item) in grid.items.iter().enumerate() {
            let column = index / metrics.rows;
            let row = index % metrics.rows;
            if index != 0 && row == 0 {
                column_x += metrics.columns[column - 1] + grid.config.item_space as f32 + 6.0;
            }
            let y = top + row as f32 * metrics.row_step;
            for (part, run) in item.iter().enumerate() {
                if run.text.is_empty() {
                    continue;
                }
                let run_width = metrics.runs[index][part].0;
                let x = column_x
                    + if part == 0 {
                        0.0
                    } else if grid.config.value_right_align {
                        metrics.columns[column] - run_width
                    } else {
                        metrics.runs[index][0].0
                    };
                let text: Vec<u16> = run.text.encode_utf16().collect();
                let layout = context.write_factory().CreateTextLayout(
                    &text,
                    style.format,
                    run_width + 2.0 * scale,
                    metrics.row_height,
                )?;
                layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
                let range = DWRITE_TEXT_RANGE {
                    startPosition: 0,
                    length: text.len() as u32,
                };
                layout.SetUnderline(style.decorations & 4 != 0, range)?;
                layout.SetStrikethrough(style.decorations & 8 != 0, range)?;
                let color = if grid.config.auto_adapt_light_theme
                    && !grid.config.specify_each_item_color
                {
                    if grid.light_theme { 0x0020_2020 } else { 0x00f5_f5f5 }
                } else {
                    run.color
                };
                let foreground = taskbar_color(color, 1.0);
                let brush = target.CreateSolidColorBrush(&foreground, None)?;
                let shadow = if transparent {
                    let luminance =
                        foreground.r * 0.2126 + foreground.g * 0.7152 + foreground.b * 0.0722;
                    Some(target.CreateSolidColorBrush(
                        &taskbar_color(if luminance < 0.5 { 0x00ff_ffff } else { 0 }, 0.65),
                        None,
                    )?)
                } else {
                    None
                };
                if let Some(progress) = run.progress.filter(|value| *value > 0.0) {
                    fills.push((
                        D2D_RECT_F {
                            left: x,
                            top: y,
                            right: x + run_width * progress.min(1.0),
                            bottom: y + metrics.row_height,
                        },
                        target.CreateSolidColorBrush(&taskbar_color(run.progress_color, 0.5), None)?,
                    ));
                }
                text_draws.push((layout, brush, shadow, windows_numerics::Vector2 { X: x, Y: y }));
            }
        }
        // Build every fallible layout/brush before BeginDraw so failures never
        // leave the DC render target inside an unfinished drawing session.
        target.BeginDraw();
        target.Clear(Some(&background));
        for (rectangle, brush) in fills {
            target.FillRectangle(&rectangle, &brush);
        }
        for (layout, brush, shadow, position) in text_draws {
            if let Some(shadow) = shadow {
                target.DrawTextLayout(
                    windows_numerics::Vector2 {
                        X: position.X + scale,
                        Y: position.Y + scale,
                    },
                    &layout,
                    &shadow,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                );
            }
            target.DrawTextLayout(position, &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
        }
        target.EndDraw(None, None)
    }
}

pub(crate) struct Bitmap {
    pub(crate) dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u32,
    width: i32,
    height: i32,
}

impl Bitmap {
    pub(crate) fn new(width: i32, height: i32) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            ensure!(!dc.is_invalid(), "CreateCompatibleDC failed");
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = std::ptr::null_mut();
            let bitmap =
                match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0) {
                    Ok(bitmap) => bitmap,
                    Err(error) => {
                        let _ = DeleteDC(dc);
                        return Err(error.into());
                    }
                };
            let previous = SelectObject(dc, HGDIOBJ(bitmap.0));
            if previous.is_invalid() || previous.0 as isize == -1 {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(dc);
                anyhow::bail!("SelectObject failed");
            }
            Ok(Self {
                dc,
                bitmap,
                previous,
                pixels: pixels.cast(),
                width,
                height,
            })
        }
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

struct NativeWindow(Rc<Surface>);
impl NativeWindow {
    fn new(context: Rc<D2dContext>, appearance: Appearance, taskbar: bool) -> Result<Self> {
        let result = Self(Rc::new(Surface {
            hwnd: Cell::new(HWND::default()),
            context,
            appearance: RefCell::new(appearance),
            renderer: RefCell::new(None),
            taskbar,
            osd: RefCell::new(OsdPlacement::default()),
            runs: RefCell::new(Vec::new()),
            grid: RefCell::new(None),
        }));
        result.create()?;
        Ok(result)
    }

    fn create(&self) -> Result<()> {
        crate::register_class(w!("Orbment.MicroWindow"), Some(surface_proc))?;
        unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                w!("Orbment.MicroWindow"),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(GetModuleHandleW(None)?.into()),
                Some(Rc::as_ptr(&self.0).cast()),
            )?;
        }
        Ok(())
    }
}
impl Drop for NativeWindow {
    fn drop(&mut self) {
        let hwnd = self.0.hwnd.get();
        if !hwnd.is_invalid() {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

pub struct TaskbarChildWindow(NativeWindow);
impl TaskbarChildWindow {
    pub fn new(
        context: Rc<D2dContext>,
        text: &str,
        font: &str,
        size: f32,
        color: u32,
        background: u32,
    ) -> Result<Self> {
        let mut result = Self(NativeWindow::new(
            context,
            Appearance {
                text: text.into(),
                font: font.into(),
                size,
                foreground: color,
                background,
                style: 0,
                transparent: None,
                on_left: false,
                snap: false,
            },
            true,
        )?);
        result.reattach()?;
        Ok(result)
    }

    pub fn set_text(&mut self, text: &str) -> Result<()> {
        {
            let mut appearance = self.0.0.appearance.borrow_mut();
            if appearance.text == text
                && self.0.0.runs.borrow().is_empty()
                && self.0.0.grid.borrow().is_none()
            {
                return Ok(());
            }
            appearance.text = text.into();
            self.0.0.runs.borrow_mut().clear();
            self.0.0.grid.borrow_mut().take();
        }
        self.refresh()
    }

    pub fn set_styled_text(&mut self, runs: Vec<TextRun>) -> Result<()> {
        let text: String = runs.iter().map(|run| run.text.as_str()).collect();
        {
            let mut current = self.0.0.runs.borrow_mut();
            if *current == runs
                && self.0.0.appearance.borrow().text == text
                && self.0.0.grid.borrow().is_none()
            {
                return Ok(());
            }
            self.0.0.appearance.borrow_mut().text = text;
            *current = runs;
            self.0.0.grid.borrow_mut().take();
        }
        self.refresh()
    }

    pub fn set_grid_options(&mut self, config: &TaskbarConfig) -> Result<()> {
        *self.0.0.grid.borrow_mut() = Some(TaskbarGrid {
            config: config.clone(),
            items: Vec::new(),
            light_theme: system_uses_light_theme(),
        });
        self.reattach()
    }

    pub fn set_grid_items(&mut self, items: Vec<[TextRun; 2]>) -> Result<()> {
        {
            let mut grid = self.0.0.grid.borrow_mut();
            let grid = grid.as_mut().context("Taskbar grid options are not configured")?;
            let light_theme = system_uses_light_theme();
            if grid.items == items && grid.light_theme == light_theme {
                return Ok(());
            }
            grid.items = items;
            grid.light_theme = light_theme;
        }
        self.refresh()
    }

    fn refresh(&mut self) -> Result<()> {
        let hwnd = self.0.0.hwnd.get();
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool()
                || (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_CHILD.0 != 0
                    && !GetParent(hwnd).is_ok_and(|parent| IsWindow(Some(parent)).as_bool()))
            {
                return self.reattach();
            }
            let mut bounds = RECT::default();
            GetClientRect(hwnd, &mut bounds)?;
            let size = self.0.0.dimensions(GetDpiForWindow(hwnd).max(96))?;
            if size != (bounds.right - bounds.left, bounds.bottom - bounds.top) {
                return self.reattach();
            }
        }
        self.0.0.paint()
    }

    pub fn set_options(
        &mut self,
        style: u32,
        transparent_color: u32,
        on_left: bool,
        snap: bool,
    ) -> Result<()> {
        {
            let mut appearance = self.0.0.appearance.borrow_mut();
            appearance.style = style;
            appearance.transparent = Some(transparent_color);
            appearance.on_left = on_left;
            appearance.snap = snap;
        }
        self.0.0.renderer.borrow_mut().take();
        self.reattach()
    }

    pub fn reattach(&mut self) -> Result<()> {
        if !unsafe { IsWindow(Some(self.0.0.hwnd.get())) }.as_bool() {
            self.0.0.renderer.borrow_mut().take();
            self.0.create()?;
        }
        self.0.0.layout_taskbar()
    }
}

pub struct OverlayFloatingWindow(NativeWindow);
impl OverlayFloatingWindow {
    pub fn new(context: Rc<D2dContext>, text: &str, duration_ms: u32) -> Result<Self> {
        let result = Self(NativeWindow::new(
            context,
            Appearance {
                text: text.into(),
                font: "Segoe UI".into(),
                size: 15.0,
                foreground: 0x00ff_ffff,
                background: 0x002b_2b2b,
                style: 0,
                transparent: None,
                on_left: false,
                snap: false,
            },
            false,
        )?);
        result.configure(OsdPlacement::default())?;
        result.set_duration(Some(duration_ms))?;
        Ok(result)
    }

    pub fn is_alive(&self) -> bool {
        !self.0.0.hwnd.get().is_invalid()
    }

    pub(crate) fn configure(&self, placement: OsdPlacement) -> Result<()> {
        *self.0.0.osd.borrow_mut() = placement;
        self.0.0.layout_osd()
    }

    pub(crate) fn set_duration(&self, duration: Option<u32>) -> Result<()> {
        unsafe {
            let hwnd = self.0.0.hwnd.get();
            let _ = KillTimer(Some(hwnd), OSD_TIMER);
            if let Some(duration) = duration {
                ensure!(
                    SetTimer(Some(hwnd), OSD_TIMER, duration.max(1), None) != 0,
                    "Unable to start OSD expiry timer"
                );
            }
        }
        Ok(())
    }
}

impl Surface {
    fn dimensions(&self, dpi: u32) -> Result<(i32, i32)> {
        let appearance = self.appearance.borrow();
        let format =
            self.context
                .text_format(&appearance.font, appearance.size, appearance.style)?;
        let grid = self.grid.borrow();
        let (width, height) = if let Some(grid) = grid.as_ref() {
            grid.measure(&self.context, &format)?.dimensions(&grid.config)
        } else {
            self.context.measure(&appearance.text, &format)?
        };
        let scale = dpi as f32 / 96.0;
        Ok((
            ((width + 16.0).clamp(80.0, 900.0) * scale).ceil() as i32,
            ((height + if self.taskbar { 4.0 } else { 24.0 }).clamp(24.0, 200.0) * scale).ceil()
                as i32,
        ))
    }

    fn paint(&self) -> Result<()> {
        let hwnd = self.hwnd.get();
        if hwnd.is_invalid() {
            return Ok(());
        }
        unsafe {
            let mut bounds = RECT::default();
            GetClientRect(hwnd, &mut bounds)?;
            if bounds.right <= 0 || bounds.bottom <= 0 {
                return Ok(());
            }
            let dpi = GetDpiForWindow(hwnd).max(96);
            let appearance = self.appearance.borrow();
            let mut renderer = self.renderer.borrow_mut();
            if renderer.is_none() {
                *renderer = Some(Renderer {
                    target: self.context.target()?,
                    format: self.context.text_format(
                        &appearance.font,
                        appearance.size,
                        appearance.style,
                    )?,
                    bitmap: None,
                });
            }
            let renderer = renderer.as_mut().context("Window renderer was destroyed")?;
            if renderer
                .bitmap
                .as_ref()
                .is_none_or(|bitmap| bitmap.width != bounds.right || bitmap.height != bounds.bottom)
            {
                renderer.bitmap = Some(Bitmap::new(bounds.right, bounds.bottom)?);
            }
            let bitmap = renderer
                .bitmap
                .as_ref()
                .context("Window bitmap was destroyed")?;
            let runs = self.runs.borrow();
            let style = TextStyle {
                format: &renderer.format,
                decorations: appearance.style,
                foreground: appearance.foreground,
                background: appearance.background,
                runs: &runs,
            };
            let grid = self.grid.borrow();
            let grid_metrics = grid
                .as_ref()
                .map(|grid| grid.measure(&self.context, &renderer.format))
                .transpose()?;
            let paint = |target: &ID2D1DCRenderTarget| {
                if let (Some(grid), Some(metrics)) = (grid.as_ref(), grid_metrics.as_ref()) {
                    return draw_taskbar(
                        &self.context,
                        target,
                        bitmap,
                        dpi,
                        &style,
                        grid,
                        metrics,
                    );
                }
                draw(
                    &self.context,
                    target,
                    bitmap.dc,
                    bounds,
                    dpi,
                    &appearance.text,
                    &style,
                )
            };
            if let Err(error) = paint(&renderer.target) {
                if error.code() != D2DERR_RECREATE_TARGET {
                    return Err(error.into());
                }
                let target = self.context.target()?;
                paint(&target)?;
                renderer.target = target;
            }
            // Grid drawing uses true premultiplied alpha; color-key removal would
            // also erase matching glyph pixels and their antialiased edges.
            if grid.is_none()
                && let Some(color) = appearance.transparent
            {
                let rgb = ((color & 0xff) << 16) | (color & 0xff00) | ((color >> 16) & 0xff);
                for pixel in std::slice::from_raw_parts_mut(
                    bitmap.pixels,
                    (bitmap.width * bitmap.height) as usize,
                ) {
                    if *pixel & 0x00ff_ffff == rgb {
                        *pixel = 0;
                    }
                }
            }
            let size = SIZE {
                cx: bitmap.width,
                cy: bitmap.height,
            };
            let source = POINT::default();
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: (self.osd.borrow().opacity.clamp(10, 100) as u32 * 255 / 100)
                    as u8,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            UpdateLayeredWindow(
                hwnd,
                None,
                None,
                Some(&size),
                Some(bitmap.dc),
                Some(&source),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?;
        }
        Ok(())
    }

    fn layout_osd(&self) -> Result<()> {
        unsafe {
            let mut cursor = POINT::default();
            GetCursorPos(&mut cursor)?;
            let placement = self.osd.borrow();
            let monitor = monitor_info(MonitorFromPoint(
                if placement.primary {
                    POINT::default()
                } else {
                    cursor
                },
                if placement.primary {
                    MONITOR_DEFAULTTOPRIMARY
                } else {
                    MONITOR_DEFAULTTONEAREST
                },
            ))?;
            let dpi = GetDpiForWindow(self.hwnd.get()).max(96);
            let (width, height) = self.dimensions(dpi)?;
            let work = monitor.rcWork;
            let width = width.min(work.right - work.left);
            let height = height.min(work.bottom - work.top);
            let margin = (12 * dpi / 96) as i32;
            let step = (height + margin) * placement.stack as i32;
            let position = placement.position.as_str();
            let mut x = if position.ends_with("Left") {
                work.left + margin
            } else if position.ends_with("Right") {
                work.right - width - margin
            } else {
                work.left + (work.right - work.left - width) / 2
            };
            let mut y = if position.starts_with("Bottom") {
                work.bottom - height - margin - step
            } else if position.starts_with("Middle") || position.starts_with("Center") {
                work.top + (work.bottom - work.top - height) / 2 + step
            } else {
                work.top + margin + step
            };
            if position == "Custom" {
                x = placement.custom[0];
                y = placement.custom[1] + step;
            }
            drop(placement);
            self.position(None, x, y, width, height)?;
            self.paint()
        }
    }

    fn layout_taskbar(&self) -> Result<()> {
        unsafe {
            let taskbar = FindWindowW(w!("Shell_TrayWnd"), None).ok();
            let hwnd = self.hwnd.get();
            let monitor = monitor_info(MonitorFromWindow(
                taskbar.unwrap_or(hwnd),
                MONITOR_DEFAULTTOPRIMARY,
            ))?;
            let dpi = GetDpiForWindow(taskbar.unwrap_or(hwnd)).max(96);
            let (width, height) = self.dimensions(dpi)?;
            let mut bar = RECT::default();
            if let Some(taskbar) = taskbar {
                GetWindowRect(taskbar, &mut bar)?;
                let options = self.appearance.borrow();
                let padding = if options.snap {
                    0
                } else {
                    (4 * dpi / 96) as i32
                };
                let on_left = options.on_left;
                drop(options);
                let horizontal = bar.right - bar.left >= bar.bottom - bar.top;
                if horizontal {
                    let defaults = TaskbarConfig::default();
                    let reserves = self.grid.borrow().as_ref().map_or(
                        (
                            defaults.taskbar_left_space_win11,
                            defaults.taskbar_right_space_win11,
                        ),
                        |grid| {
                            (
                                grid.config.taskbar_left_space_win11,
                                grid.config.taskbar_right_space_win11,
                            )
                        },
                    );
                    let tray = FindWindowExW(Some(taskbar), None, w!("TrayNotifyWnd"), None)
                        .ok()
                        .and_then(|tray| {
                            let mut rect = RECT::default();
                            GetWindowRect(tray, &mut rect).ok().map(|()| rect)
                        });
                    // Win11's full-width XAML input island is not an occupied icon area.
                    let (x, y) = horizontal_taskbar_position(
                        bar,
                        (width, height),
                        reserves,
                        dpi,
                        on_left,
                        padding,
                        tray,
                    );
                    if self.position(Some(taskbar), x, y, width, height).is_err() {
                        self.position(None, x, y, width, height)?;
                    }
                    return self.paint();
                }
                let mut occupied = Occupied {
                    own: hwnd,
                    rectangles: Vec::new(),
                };
                let _ = EnumChildWindows(
                    Some(taskbar),
                    Some(collect_occupied),
                    LPARAM((&mut occupied as *mut Occupied) as isize),
                );
                if let Some((x, y)) = free_position(
                    bar,
                    &occupied.rectangles,
                    width,
                    height,
                    horizontal,
                    on_left,
                    padding,
                ) && self.position(Some(taskbar), x, y, width, height).is_ok()
                {
                    return self.paint();
                }
            }
            // Only missing Explorer or an unusable vertical bar needs a desktop fallback.
            let work = monitor.rcWork;
            let width = width.min(work.right - work.left);
            let height = height.min(work.bottom - work.top);
            let on_left = self.appearance.borrow().on_left;
            let x = if on_left {
                work.left + 4
            } else {
                work.right - width - 4
            }
            .max(work.left);
            let y = if bar.bottom > bar.top && bar.bottom <= work.top {
                work.top + 4
            } else {
                work.bottom - height - 4
            }
            .max(work.top);
            self.position(None, x, y, width, height)?;
            self.paint()
        }
    }

    fn position(
        &self,
        parent: Option<HWND>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<()> {
        unsafe {
            let hwnd = self.hwnd.get();
            let previous_parent = GetParent(hwnd).ok();
            let style = if parent.is_some() { WS_CHILD } else { WS_POPUP };
            let mut changed = previous_parent != parent
                || GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & (WS_CHILD.0 | WS_POPUP.0) != style.0;
            if changed {
                SetWindowLongPtrW(hwnd, GWL_STYLE, style.0 as isize);
                // SetParent returns NULL on a successful transition from a top-level window.
                let _ = SetParent(hwnd, parent);
                ensure!(
                    GetParent(hwnd).ok() == parent,
                    "Taskbar parent change failed"
                );
            }
            let mut old = RECT::default();
            GetWindowRect(hwnd, &mut old)?;
            changed |= old.left != x
                || old.top != y
                || old.right - old.left != width
                || old.bottom - old.top != height;
            let mut point = POINT { x, y };
            if let Some(parent) = parent {
                ScreenToClient(parent, &mut point).ok()?;
            }
            if changed {
                SetWindowPos(
                    hwnd,
                    Some(if parent.is_some() {
                        HWND_TOP
                    } else {
                        HWND_TOPMOST
                    }),
                    point.x,
                    point.y,
                    width,
                    height,
                    SWP_NOACTIVATE | SWP_FRAMECHANGED,
                )?;
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(())
    }
}

fn horizontal_taskbar_position(
    bar: RECT,
    size: (i32, i32),
    reserves: (u32, u32),
    dpi: u32,
    on_left: bool,
    padding: i32,
    tray: Option<RECT>,
) -> (i32, i32) {
    let (width, height) = size;
    let scale = dpi as f32 / 96.0;
    // Imported reserves are unrestricted u32 values; clamp in a wider coordinate type.
    let left_reserve = (reserves.0 as f32 * scale).round() as i64;
    let right_reserve = (reserves.1 as f32 * scale).round() as i64;
    let left = i64::from(bar.left);
    let right = i64::from(bar.right);
    let mut right_edge = right - right_reserve;
    if reserves.1 == TaskbarConfig::default().taskbar_right_space_win11
        && let Some(tray) = tray.filter(|tray| {
            tray.right > tray.left
                && tray.bottom > tray.top
                && i64::from(tray.left) >= left + (right - left) / 2
                && tray.left < bar.right
                && tray.top < bar.bottom
                && tray.bottom > bar.top
        })
    {
        // Defaults avoid the notification area; custom reserves provide exact manual placement.
        right_edge = right_edge.min(i64::from(tray.left));
    }
    let x = if on_left {
        left + left_reserve + i64::from(padding)
    } else {
        right_edge - i64::from(width) - i64::from(padding)
    }
    .clamp(left, (right - i64::from(width)).max(left)) as i32;
    (x, bar.top + (bar.bottom - bar.top - height) / 2)
}

struct Occupied {
    own: HWND,
    rectangles: Vec<RECT>,
}
unsafe extern "system" fn collect_occupied(hwnd: HWND, parameter: LPARAM) -> BOOL {
    unsafe {
        let occupied = &mut *(parameter.0 as *mut Occupied);
        if hwnd != occupied.own
            && IsWindowVisible(hwnd).as_bool()
            && GetWindow(hwnd, GW_CHILD).is_err()
        {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok()
                && rect.right > rect.left
                && rect.bottom > rect.top
            {
                occupied.rectangles.push(rect);
            }
        }
        BOOL(1)
    }
}

fn free_position(
    bar: RECT,
    occupied: &[RECT],
    width: i32,
    height: i32,
    horizontal: bool,
    on_left: bool,
    padding: i32,
) -> Option<(i32, i32)> {
    if width + 2 * padding > bar.right - bar.left || height + 2 * padding > bar.bottom - bar.top {
        return None;
    }
    let (start, end, length) = if horizontal {
        (bar.left + padding, bar.right - padding, width)
    } else {
        (bar.top + padding, bar.bottom - padding, height)
    };
    let mut blocked: Vec<(i32, i32)> = occupied
        .iter()
        .filter(|rect| {
            rect.left < bar.right
                && rect.right > bar.left
                && rect.top < bar.bottom
                && rect.bottom > bar.top
        })
        .map(|rect| {
            if horizontal {
                (rect.left - padding, rect.right + padding)
            } else {
                (rect.top - padding, rect.bottom + padding)
            }
        })
        .map(|(left, right)| (left.max(start), right.min(end)))
        .filter(|(left, right)| left < right)
        .collect();
    blocked.sort_unstable();
    let mut cursor = start;
    let mut gaps = Vec::new();
    for (left, right) in blocked {
        if left - cursor >= length {
            gaps.push((cursor, left.min(end)));
        }
        cursor = cursor.max(right);
    }
    if end - cursor >= length {
        gaps.push((cursor, end));
    }
    let gap = if on_left { gaps.first() } else { gaps.last() }?;
    let coordinate = if on_left { gap.0 } else { gap.1 - length };
    Some(if horizontal {
        (coordinate, bar.top + (bar.bottom - bar.top - height) / 2)
    } else {
        (bar.left + (bar.right - bar.left - width) / 2, coordinate)
    })
}

fn monitor_info(monitor: HMONITOR) -> Result<MONITORINFO> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(monitor, &mut info).ok()?;
    }
    Ok(info)
}

unsafe extern "system" fn surface_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            let pointer = create.lpCreateParams as *const Surface;
            Rc::increment_strong_count(pointer);
            (*pointer).hwnd.set(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Surface;
        if !pointer.is_null() {
            Rc::increment_strong_count(pointer);
            let state = Rc::from_raw(pointer);
            match message {
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    BeginPaint(hwnd, &mut paint);
                    if let Err(error) = state.paint() {
                        tracing::warn!(%error, "Window repaint failed");
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                WM_ERASEBKGND => return LRESULT(1),
                WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
                WM_TIMER if wparam.0 == OSD_TIMER => {
                    let _ = KillTimer(Some(hwnd), OSD_TIMER);
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                    let _ = PostMessageW(Some(hwnd), LAYOUT_MESSAGE, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
                LAYOUT_MESSAGE => {
                    let result = if state.taskbar {
                        state.layout_taskbar()
                    } else {
                        state.layout_osd()
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error, "Window layout failed");
                    }
                    return LRESULT(0);
                }
                WM_NCDESTROY => {
                    state.hwnd.set(HWND::default());
                    state.renderer.borrow_mut().take();
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Rc::from_raw(pointer));
                    if !state.taskbar
                        && let Ok(host) = FindWindowW(w!("Orbment.Host"), None)
                    {
                        crate::wake(host);
                    }
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: RECT = RECT {
        left: 0,
        top: 1040,
        right: 1920,
        bottom: 1080,
    };

    #[test]
    fn horizontal_reserves_move_each_side_by_exact_dpi_scaled_pixels() {
        for (dpi, step) in [(96, 20), (144, 30), (192, 40)] {
            let base_left =
                horizontal_taskbar_position(BAR, (200, 24), (160, 280), dpi, true, 4, None);
            let moved_left =
                horizontal_taskbar_position(BAR, (200, 24), (180, 280), dpi, true, 4, None);
            let base_right =
                horizontal_taskbar_position(BAR, (200, 24), (160, 280), dpi, false, 4, None);
            let moved_right =
                horizontal_taskbar_position(BAR, (200, 24), (160, 300), dpi, false, 4, None);
            assert_eq!(moved_left.0 - base_left.0, step);
            assert_eq!(base_right.0 - moved_right.0, step);
            assert_eq!(base_left.1, 1048);
            assert_eq!(base_right.1, 1048);
        }
        assert_eq!(
            horizontal_taskbar_position(BAR, (200, 24), (160, 280), 96, true, 4, None),
            (164, 1048)
        );
        assert_eq!(
            horizontal_taskbar_position(BAR, (200, 24), (160, 280), 96, false, 4, None),
            (1436, 1048)
        );
    }

    #[test]
    fn default_right_reserve_avoids_tray_but_custom_reserves_remain_exact() {
        let tray = RECT {
            left: 1500,
            top: BAR.top,
            right: BAR.right,
            bottom: BAR.bottom,
        };
        assert_eq!(
            horizontal_taskbar_position(
                BAR,
                (200, 24),
                (160, 280),
                96,
                false,
                4,
                Some(tray)
            ),
            (1296, 1048)
        );
        assert_eq!(
            horizontal_taskbar_position(
                BAR,
                (200, 24),
                (160, 300),
                96,
                false,
                4,
                Some(tray)
            ),
            (1416, 1048)
        );
        // A full-width XAML island cannot masquerade as the right-hand tray.
        assert_eq!(
            horizontal_taskbar_position(BAR, (200, 24), (160, 280), 96, false, 4, Some(BAR)),
            (1436, 1048)
        );
    }

    #[test]
    fn horizontal_placement_clamps_large_reserves_and_handles_negative_origins() {
        let bar = RECT {
            left: -1920,
            right: 0,
            ..BAR
        };
        assert_eq!(
            horizontal_taskbar_position(bar, (200, 24), (160, 280), 96, true, 0, None),
            (-1760, 1048)
        );
        assert_eq!(
            horizontal_taskbar_position(
                bar,
                (200, 24),
                (u32::MAX, u32::MAX),
                192,
                true,
                4,
                None
            ),
            (-200, 1048)
        );
        assert_eq!(
            horizontal_taskbar_position(
                bar,
                (200, 24),
                (u32::MAX, u32::MAX),
                192,
                false,
                4,
                None
            ),
            (-1920, 1048)
        );
        assert_eq!(
            horizontal_taskbar_position(bar, (2200, 24), (160, 280), 96, false, 4, None),
            (-1920, 1048)
        );
    }
}
