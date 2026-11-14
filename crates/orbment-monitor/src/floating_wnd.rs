use crate::{
    service::label,
    skin::{METRICS, Skin},
};
use anyhow::{Result, ensure};
use orbment_core::{config::MonitorConfig, i18n::tr};
use orbment_ui::D2dContext;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    mem::size_of,
    path::Path,
    rc::Rc,
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{Common::*, *},
            DirectWrite::*,
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Gdi::*,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::GetDpiForWindow, Shell::ShellExecuteW, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};

const VISIBILITY_TIMER: usize = 41;
const RELAYOUT: u32 = WM_APP + 71;

type PositionCallback = Rc<dyn Fn(i32, i32)>;
type ActionCallback = Rc<dyn Fn(u8)>;

pub struct FloatingWindow(Rc<Surface>);
struct Surface {
    hwnd: Cell<HWND>,
    context: Rc<D2dContext>,
    config: RefCell<MonitorConfig>,
    skin: RefCell<Skin>,
    values: RefCell<BTreeMap<String, String>>,
    renderer: RefCell<Option<Renderer>>,
    position_callback: RefCell<Option<PositionCallback>>,
    action_callback: RefCell<Option<ActionCallback>>,
    folded: Cell<Option<Folded>>,
    dragging: Cell<bool>,
    visibility_timer: Cell<bool>,
}
#[derive(Clone, Copy)]
struct Folded {
    original: RECT,
    visible: RECT,
}
struct Renderer {
    target: ID2D1DCRenderTarget,
    format: IDWriteTextFormat,
    bitmap: Bitmap,
    background: Option<ID2D1Bitmap>,
    dpi: u32,
}
struct Bitmap {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    width: i32,
    height: i32,
}
impl Bitmap {
    fn new(width: i32, height: i32) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            ensure!(!dc.is_invalid(), "{}", tr("monitor-window-bitmap"));
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
                anyhow::bail!("{}", tr("monitor-window-bitmap"));
            }
            Ok(Self {
                dc,
                bitmap,
                previous,
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

impl FloatingWindow {
    /// Create and drop on the shared D2D/UI thread. `skin_root` is data_dir/skins.
    pub fn new(context: Rc<D2dContext>, config: MonitorConfig, skin_root: &Path) -> Result<Self> {
        let skin = load_skin(&config, skin_root)?;
        let state = Rc::new(Surface {
            hwnd: Cell::new(HWND::default()),
            context,
            config: RefCell::new(config),
            skin: RefCell::new(skin),
            values: RefCell::new(BTreeMap::new()),
            renderer: RefCell::new(None),
            position_callback: RefCell::new(None),
            action_callback: RefCell::new(None),
            folded: Cell::new(None),
            dragging: Cell::new(false),
            visibility_timer: Cell::new(false),
        });
        let result = Self(state);
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: w!("Orbment.Monitor.Floating"),
                style: CS_DBLCLKS,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                ensure!(
                    GetLastError() == ERROR_CLASS_ALREADY_EXISTS,
                    "{}",
                    tr("monitor-window-class")
                );
            }
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.lpszClassName,
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                Some(Rc::as_ptr(&result.0).cast()),
            )?;
            result.0.hwnd.set(hwnd);
            result.0.configure()?;
        }
        Ok(result)
    }
    pub fn hwnd(&self) -> HWND {
        self.0.hwnd.get()
    }
    pub fn set_values(&self, values: BTreeMap<String, String>) {
        let changed = {
            let previous = self.0.values.borrow();
            let config = self.0.config.borrow();
            let skin = self.0.skin.borrow();
            skin.layout(config.floating.show_cpu_memory)
                .items
                .iter()
                .filter(|item| item.show)
                .any(|item| {
                    let key = if config.floating.swap_up_down {
                        match item.metric.as_str() {
                            "up" => "down",
                            "down" => "up",
                            key => key,
                        }
                    } else {
                        &item.metric
                    };
                    previous.get(key) != values.get(key)
                })
        };
        *self.0.values.borrow_mut() = values;
        if changed {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd()), None, false);
            }
        }
    }
    pub fn apply_config(&self, config: MonitorConfig, skin_root: &Path) -> Result<()> {
        let skin = load_skin(&config, skin_root)?;
        *self.0.config.borrow_mut() = config;
        *self.0.skin.borrow_mut() = skin;
        self.0.configure()
    }
    pub fn position(&self) -> (i32, i32) {
        let mut rect = self
            .0
            .folded
            .get()
            .map(|folded| folded.original)
            .unwrap_or_default();
        if self.0.folded.get().is_none() {
            unsafe {
                let _ = GetWindowRect(self.hwnd(), &mut rect);
            }
        }
        (rect.left, rect.top)
    }
    pub fn set_position_callback(&self, callback: impl Fn(i32, i32) + 'static) {
        *self.0.position_callback.borrow_mut() = Some(Rc::new(callback));
    }
    /// Monitor skin action IDs 1(history), 3(settings), 6(skin chooser) are host UI actions.
    pub fn set_action_callback(&self, callback: impl Fn(u8) + 'static) {
        *self.0.action_callback.borrow_mut() = Some(Rc::new(callback));
    }
}
impl Drop for FloatingWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd());
        }
    }
}

fn load_skin(config: &MonitorConfig, root: &Path) -> Result<Skin> {
    if config.floating.skin_selected.is_empty() {
        Ok(Skin::default())
    } else {
        Skin::load(&root.join(&config.floating.skin_selected))
    }
}

impl Surface {
    fn configure(&self) -> Result<()> {
        let hwnd = self.hwnd.get();
        let config = self.config.try_borrow()?;
        let floating = config.floating.clone();
        drop(config);
        let (mut x, mut y, width, height, topmost, clickthrough) = {
            let skin = self.skin.try_borrow()?;
            let layout = skin.layout(floating.show_cpu_memory);
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
            (
                floating.pos_x,
                floating.pos_y,
                layout.width * dpi as i32 / 96,
                layout.height * dpi as i32 / 96,
                floating.always_on_top,
                floating.mouse_penetrate,
            )
        };
        unsafe {
            self.unfold();
            let style = WS_EX_LAYERED
                | WS_EX_TOOLWINDOW
                | WS_EX_NOACTIVATE
                | if clickthrough {
                    WS_EX_TRANSPARENT
                } else {
                    WINDOW_EX_STYLE(0)
                };
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style.0 as isize);
            let mut rect = RECT {
                left: x,
                top: y,
                right: x + width,
                bottom: y + height,
            };
            if x == -1 && y == -1 {
                let work = monitor_work(rect);
                rect.left = work.right - width - 12;
                rect.top = work.bottom - height - 12;
                rect.right = rect.left + width;
                rect.bottom = rect.top + height;
            }
            let work = monitor_work(rect);
            constrain(&mut rect, work, false, floating.allow_out_of_border != 0);
            x = rect.left;
            y = rect.top;
            SetWindowPos(
                hwnd,
                Some(if topmost {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                }),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE,
            )?;
        }
        self.renderer.try_borrow_mut()?.take();
        let needs_timer =
            floating.enabled && (floating.hide_on_fullscreen || floating.allow_out_of_border == 2);
        if needs_timer != self.visibility_timer.get() {
            unsafe {
                if needs_timer {
                    ensure!(
                        SetTimer(Some(hwnd), VISIBILITY_TIMER, 500, None) != 0,
                        "{}",
                        tr("monitor-window-timer")
                    );
                } else {
                    KillTimer(Some(hwnd), VISIBILITY_TIMER)?;
                }
            }
            self.visibility_timer.set(needs_timer);
        }
        self.visibility();
        self.paint()
    }

    fn create_renderer(&self) -> Result<Renderer> {
        let config = self.config.try_borrow()?;
        let floating = &config.floating;
        let skin = self.skin.try_borrow()?;
        let layout = skin.layout(floating.show_cpu_memory);
        let dpi = unsafe { GetDpiForWindow(self.hwnd.get()) }.max(96);
        let target = self.context.target()?;
        let font = skin
            .font
            .as_ref()
            .filter(|_| floating.allow_skin_cover_font);
        let format = self.context.text_format(
            font.map_or(&floating.font_name, |font| &font.name),
            font.map_or(floating.size, |font| font.size),
            font.map_or(floating.style, |font| font.style),
        )?;
        let bitmap = Bitmap::new(
            layout.width * dpi as i32 / 96,
            layout.height * dpi as i32 / 96,
        )?;
        let background_path = if floating.show_cpu_memory {
            &skin.background_large
        } else {
            &skin.background_small
        };
        let background = if let Some(path) = background_path {
            let image = image::ImageReader::open(path)?
                .with_guessed_format()?
                .decode()?
                .to_rgba8();
            let (width, height) = image.dimensions();
            let mut pixels = image.into_raw();
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
                for i in 0..3 {
                    pixel[i] = (u16::from(pixel[i]) * u16::from(pixel[3]) / 255) as u8;
                }
            }
            let properties = D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
            };
            Some(unsafe {
                target.CreateBitmap(
                    D2D_SIZE_U { width, height },
                    Some(pixels.as_ptr().cast()),
                    width * 4,
                    &properties,
                )
            }?)
        } else {
            None
        };
        Ok(Renderer {
            target,
            format,
            bitmap,
            background,
            dpi,
        })
    }

    fn paint(&self) -> Result<()> {
        if self.renderer.try_borrow()?.is_none() {
            let renderer = self.create_renderer()?;
            *self.renderer.try_borrow_mut()? = Some(renderer);
        }
        let result = self.paint_frame();
        if result.is_err() {
            self.renderer.try_borrow_mut()?.take();
        }
        result
    }

    fn paint_frame(&self) -> Result<()> {
        let renderer = self.renderer.try_borrow()?;
        let renderer = renderer.as_ref().expect("renderer initialized");
        let config = self.config.try_borrow()?;
        let floating = &config.floating;
        let skin = self.skin.try_borrow()?;
        let layout = skin.layout(floating.show_cpu_memory);
        let values = self.values.try_borrow()?;
        let bounds = RECT {
            left: 0,
            top: 0,
            right: renderer.bitmap.width,
            bottom: renderer.bitmap.height,
        };
        unsafe {
            let target = &renderer.target;
            target.BindDC(renderer.bitmap.dc, &bounds)?;
            target.SetDpi(renderer.dpi as f32, renderer.dpi as f32);
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            target.BeginDraw();
            target.Clear(Some(&D2D1_COLOR_F {
                r: 0.06,
                g: 0.06,
                b: 0.06,
                a: if renderer.background.is_some() {
                    0.0
                } else {
                    1.0
                },
            }));
            if let Some(background) = &renderer.background {
                let destination = D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: layout.width as f32,
                    bottom: layout.height as f32,
                };
                target.DrawBitmap(
                    background,
                    Some(&destination),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    None,
                );
            }
            let draw_result = (|| -> Result<()> {
                for item in layout
                    .items
                    .iter()
                    .filter(|item| item.show && item.width > 0)
                {
                    let key = if floating.swap_up_down {
                        match item.metric.as_str() {
                            "up" => "down",
                            "down" => "up",
                            key => key,
                        }
                    } else {
                        &item.metric
                    };
                    let prefix = if skin.no_label {
                        ""
                    } else if floating.allow_skin_cover_text {
                        skin.labels
                            .get(key)
                            .map(String::as_str)
                            .unwrap_or_else(|| label(&config.labels, key))
                    } else {
                        label(&config.labels, key)
                    };
                    let value = values.get(key).map(String::as_str).unwrap_or_default();
                    let index = METRICS
                        .iter()
                        .find(|(name, _)| *name == key)
                        .map_or(0, |(_, bit)| bit.trailing_zeros() as usize);
                    let colors = if skin.text_colors.is_empty() {
                        &floating.text_colors
                    } else {
                        &skin.text_colors
                    };
                    let per_item = if skin.text_colors.is_empty() {
                        floating.specify_each_item_color
                    } else {
                        skin.specify_each_item_color
                    };
                    let color = colors
                        .get(if per_item { index } else { 0 })
                        .or_else(|| colors.first())
                        .copied()
                        .unwrap_or(0xffffff);
                    let brush = target.CreateSolidColorBrush(&colorref(color), None)?;
                    let font_style = skin
                        .font
                        .as_ref()
                        .filter(|_| floating.allow_skin_cover_font)
                        .map_or(floating.style, |font| font.style);
                    let draw_text = |text: &str, alignment: DWRITE_TEXT_ALIGNMENT| -> Result<()> {
                        let text = text.encode_utf16().collect::<Vec<_>>();
                        let text_layout = self.context.write_factory().CreateTextLayout(
                            &text,
                            &renderer.format,
                            item.width as f32,
                            skin.text_height as f32,
                        )?;
                        text_layout.SetTextAlignment(alignment)?;
                        let range = DWRITE_TEXT_RANGE {
                            startPosition: 0,
                            length: text.len() as u32,
                        };
                        text_layout.SetUnderline(font_style & 4 != 0, range)?;
                        text_layout.SetStrikethrough(font_style & 8 != 0, range)?;
                        target.DrawTextLayout(
                            windows_numerics::Vector2 {
                                X: item.x as f32,
                                Y: item.y as f32,
                            },
                            &text_layout,
                            &brush,
                            D2D1_DRAW_TEXT_OPTIONS_CLIP,
                        );
                        Ok(())
                    };
                    if item.align == 3
                        && self
                            .context
                            .measure(&format!("{prefix}{value}"), &renderer.format)?
                            .0
                            < item.width as f32
                    {
                        draw_text(prefix, DWRITE_TEXT_ALIGNMENT_LEADING)?;
                        draw_text(value, DWRITE_TEXT_ALIGNMENT_TRAILING)?;
                    } else {
                        draw_text(
                            &format!("{prefix}{value}"),
                            match item.align {
                                1 => DWRITE_TEXT_ALIGNMENT_TRAILING,
                                2 => DWRITE_TEXT_ALIGNMENT_CENTER,
                                _ => DWRITE_TEXT_ALIGNMENT_LEADING,
                            },
                        )?;
                    }
                }
                Ok(())
            })();
            let end = target.EndDraw(None, None);
            draw_result?;
            end?;
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: (u16::from(floating.opacity_pct) * 255 / 100) as u8,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            drop(values);
            drop(skin);
            drop(config);
            UpdateLayeredWindow(
                self.hwnd.get(),
                None,
                None,
                Some(&SIZE {
                    cx: renderer.bitmap.width,
                    cy: renderer.bitmap.height,
                }),
                Some(renderer.bitmap.dc),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?;
        }
        Ok(())
    }

    fn visibility(&self) {
        let Ok(config) = self.config.try_borrow() else {
            return;
        };
        let floating = config.floating.clone();
        drop(config);
        let hide =
            !floating.enabled || (floating.hide_on_fullscreen && fullscreen(self.hwnd.get()));
        unsafe {
            if IsWindowVisible(self.hwnd.get()).as_bool() == hide {
                let _ = ShowWindow(
                    self.hwnd.get(),
                    if hide { SW_HIDE } else { SW_SHOWNOACTIVATE },
                );
            }
        }
        if hide || floating.allow_out_of_border != 2 || self.dragging.get() {
            return;
        }
        unsafe {
            let hwnd = self.hwnd.get();
            let mut cursor = POINT::default();
            let mut rect = RECT::default();
            if GetCursorPos(&mut cursor).is_err() || GetWindowRect(hwnd, &mut rect).is_err() {
                return;
            }
            let visible = self.folded.get().map_or(rect, |folded| folded.visible);
            let hovered = cursor.x >= visible.left
                && cursor.x < visible.right
                && cursor.y >= visible.top
                && cursor.y < visible.bottom;
            if self.folded.get().is_some() {
                if hovered {
                    self.unfold();
                }
            } else if !hovered {
                let work = monitor_work(rect);
                let thickness = (3 * GetDpiForWindow(hwnd).max(96) / 96) as i32;
                let Some(visible) = edge_strip(rect, work, thickness) else {
                    return;
                };
                // Clip in place rather than moving most of the HWND onto an adjacent monitor.
                let region = CreateRectRgn(
                    visible.left - rect.left,
                    visible.top - rect.top,
                    visible.right - rect.left,
                    visible.bottom - rect.top,
                );
                if !region.is_invalid() {
                    self.folded.set(Some(Folded {
                        original: rect,
                        visible,
                    }));
                    if SetWindowRgn(hwnd, Some(region), true) == 0 {
                        self.folded.set(None);
                        let _ = DeleteObject(HGDIOBJ(region.0));
                    }
                }
            }
        }
    }

    fn unfold(&self) {
        if self.folded.take().is_some() {
            unsafe {
                let _ = SetWindowRgn(self.hwnd.get(), None, true);
            }
        }
    }

    fn double_click(&self) {
        let (action, executable) = {
            let Ok(config) = self.config.try_borrow() else {
                return;
            };
            (config.double_click_action, config.double_click_exe.clone())
        };
        let target = match action {
            0 => Some("ncpa.cpl".to_owned()),
            4 => Some("taskmgr.exe".to_owned()),
            5 => Some(executable),
            _ => None,
        };
        if let Some(target) = target {
            let target = target.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            unsafe {
                let result = ShellExecuteW(
                    Some(self.hwnd.get()),
                    w!("open"),
                    PCWSTR(target.as_ptr()),
                    None,
                    None,
                    SW_SHOWNORMAL,
                );
                if result.0 as isize <= 32 {
                    tracing::warn!("monitor-double-click-failed");
                }
            }
        } else if action == 2 {
            {
                let Ok(mut config) = self.config.try_borrow_mut() else {
                    return;
                };
                config.floating.show_cpu_memory = !config.floating.show_cpu_memory;
            }
            if let Err(error) = self.configure() {
                tracing::warn!(%error, "Monitor layout switch");
            }
            let callback = self
                .action_callback
                .try_borrow()
                .ok()
                .and_then(|value| value.clone());
            if let Some(callback) = callback {
                callback(action);
            }
        } else if action != 7
            && let Some(callback) = self
                .action_callback
                .try_borrow()
                .ok()
                .and_then(|value| value.clone())
        {
            callback(action);
        }
    }
}

fn colorref(value: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: (value & 255) as f32 / 255.0,
        g: ((value >> 8) & 255) as f32 / 255.0,
        b: ((value >> 16) & 255) as f32 / 255.0,
        a: 1.0,
    }
}
fn monitor_work(rect: RECT) -> RECT {
    unsafe {
        let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            info.rcWork
        } else {
            rect
        }
    }
}
fn fullscreen(own: HWND) -> bool {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_invalid()
            || foreground == own
            || foreground == GetDesktopWindow()
            || foreground == GetShellWindow()
        {
            return false;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(foreground, Some(&mut pid));
        if pid == std::process::id() {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(foreground, &mut rect).is_err() {
            return false;
        }
        let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info).as_bool()
            && rect.left <= info.rcMonitor.left
            && rect.top <= info.rcMonitor.top
            && rect.right >= info.rcMonitor.right
            && rect.bottom >= info.rcMonitor.bottom
    }
}
fn edge_strip(rect: RECT, work: RECT, thickness: i32) -> Option<RECT> {
    let mut visible = RECT {
        left: rect.left.max(work.left),
        top: rect.top.max(work.top),
        right: rect.right.min(work.right),
        bottom: rect.bottom.min(work.bottom),
    };
    if rect.left <= work.left {
        visible.right = visible.right.min(visible.left + thickness);
    } else if rect.right >= work.right {
        visible.left = visible.left.max(visible.right - thickness);
    } else if rect.top <= work.top {
        visible.bottom = visible.bottom.min(visible.top + thickness);
    } else if rect.bottom >= work.bottom {
        visible.top = visible.top.max(visible.bottom - thickness);
    } else {
        return None;
    }
    (visible.left < visible.right && visible.top < visible.bottom).then_some(visible)
}

fn constrain(rect: &mut RECT, work: RECT, snap: bool, allow_outside: bool) {
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if snap {
        if (rect.left - work.left).abs() <= 12 {
            rect.left = work.left;
        } else if (rect.right - work.right).abs() <= 12 {
            rect.left = work.right - width;
        }
        if (rect.top - work.top).abs() <= 12 {
            rect.top = work.top;
        } else if (rect.bottom - work.bottom).abs() <= 12 {
            rect.top = work.bottom - height;
        }
    }
    if !allow_outside {
        rect.left = rect
            .left
            .clamp(work.left, (work.right - width).max(work.left));
        rect.top = rect
            .top
            .clamp(work.top, (work.bottom - height).max(work.top));
    }
    rect.right = rect.left + width;
    rect.bottom = rect.top + height;
}

unsafe extern "system" fn window_proc(
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
                    let _ = BeginPaint(hwnd, &mut paint);
                    if let Err(error) = state.paint() {
                        tracing::warn!(%error, "Monitor floating paint");
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                WM_ERASEBKGND => return LRESULT(1),
                WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
                WM_NCHITTEST => {
                    let Ok(config) = state.config.try_borrow() else {
                        return LRESULT(HTCLIENT as isize);
                    };
                    return LRESULT(if config.floating.mouse_penetrate {
                        HTTRANSPARENT as isize
                    } else if config.floating.lock_position {
                        HTCLIENT as isize
                    } else {
                        HTCAPTION as isize
                    });
                }
                WM_NCLBUTTONDBLCLK | WM_LBUTTONDBLCLK => {
                    state.double_click();
                    return LRESULT(0);
                }
                WM_ENTERSIZEMOVE => {
                    state.dragging.set(true);
                    state.unfold();
                }
                WM_MOVING => {
                    let rect = &mut *(lparam.0 as *mut RECT);
                    let Ok(config) = state.config.try_borrow() else {
                        return LRESULT(0);
                    };
                    constrain(
                        rect,
                        monitor_work(*rect),
                        config.floating.snap,
                        config.floating.allow_out_of_border != 0,
                    );
                    return LRESULT(1);
                }
                WM_EXITSIZEMOVE => {
                    state.dragging.set(false);
                    let mut rect = RECT::default();
                    if GetWindowRect(hwnd, &mut rect).is_ok() {
                        if let Ok(mut config) = state.config.try_borrow_mut() {
                            config.floating.pos_x = rect.left;
                            config.floating.pos_y = rect.top;
                        }
                        let callback = state
                            .position_callback
                            .try_borrow()
                            .ok()
                            .and_then(|value| value.clone());
                        if let Some(callback) = callback {
                            callback(rect.left, rect.top);
                        }
                    }
                }
                WM_TIMER if wparam.0 == VISIBILITY_TIMER => {
                    state.visibility();
                    return LRESULT(0);
                }
                WM_MOVE => {
                    if state.dragging.get() && state.folded.get().is_none() {
                        let mut rect = RECT::default();
                        if GetWindowRect(hwnd, &mut rect).is_ok()
                            && let Ok(mut config) = state.config.try_borrow_mut()
                        {
                            config.floating.pos_x = rect.left;
                            config.floating.pos_y = rect.top;
                        }
                    }
                }
                WM_DPICHANGED => {
                    let rect = &*(lparam.0 as *const RECT);
                    if let Ok(mut config) = state.config.try_borrow_mut() {
                        config.floating.pos_x = rect.left;
                        config.floating.pos_y = rect.top;
                    }
                    let _ = PostMessageW(Some(hwnd), RELAYOUT, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
                WM_DISPLAYCHANGE => {
                    let _ = PostMessageW(Some(hwnd), RELAYOUT, WPARAM(0), LPARAM(0));
                }
                RELAYOUT => {
                    if let Err(error) = state.configure() {
                        tracing::warn!(%error, "Monitor floating relayout");
                    }
                    return LRESULT(0);
                }
                WM_NCDESTROY => {
                    state.hwnd.set(HWND::default());
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Rc::from_raw(pointer));
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
    #[test]
    fn hidden_strip_stays_on_its_monitor_at_shared_edges() {
        let work = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        for rect in [
            RECT {
                left: -200,
                top: 100,
                right: 0,
                bottom: 140,
            },
            RECT {
                left: -1920,
                top: 100,
                right: -1720,
                bottom: 140,
            },
            RECT {
                left: -200,
                top: 0,
                right: -50,
                bottom: 40,
            },
            RECT {
                left: -200,
                top: 1040,
                right: -50,
                bottom: 1080,
            },
        ] {
            let strip = edge_strip(rect, work, 6).unwrap();
            assert!(strip.left >= work.left && strip.right <= work.right);
            assert!(strip.top >= work.top && strip.bottom <= work.bottom);
            assert!((strip.right - strip.left).min(strip.bottom - strip.top) == 6);
        }
    }

    #[test]
    fn snaps_and_clamps_on_negative_coordinate_monitor() {
        let work = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1040,
        };
        let mut rect = RECT {
            left: -1915,
            top: 10,
            right: -1715,
            bottom: 50,
        };
        constrain(&mut rect, work, true, false);
        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (-1920, 0, -1720, 40)
        );
        let mut rect = RECT {
            left: -100,
            top: 1100,
            right: 100,
            bottom: 1140,
        };
        constrain(&mut rect, work, false, false);
        assert_eq!((rect.left, rect.top), (-200, 1000));
    }
}
