use crate::{d2d_context::D2dContext, windows::Bitmap};
use anyhow::{Result, ensure};
use orbment_core::{
    config::ColorFormat,
    i18n::tr,
    utilities::{Rgb, copy_text, screen_pixel},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{Common::*, *},
            Gdi::*,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI}, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::w,
};

pub(crate) type ColorPinMailbox = Rc<Cell<Option<([u8; 3], i32, i32)>>>;

/// Independent layered OverlayFloatingWindow; no swap chain or capture-module dependency.
pub(crate) struct UtilityOverlay(Rc<State>);
enum Kind {
    Picker {
        format: Cell<ColorFormat>,
        position: Cell<POINT>,
        rgb: Cell<Option<Rgb>>,
        previous: HWND,
        template: String,
        pin_enabled: Rc<Cell<bool>>,
        pending_pin: ColorPinMailbox,
    },
    Border {
        target: HWND,
        color: u32,
        bounds: Cell<RECT>,
    },
}
struct State {
    hwnd: Cell<HWND>,
    graphics: RefCell<Rc<D2dContext>>,
    kind: Kind,
    wake: HWND,
    scale: Cell<f32>,
    dirty: Cell<bool>,
}
impl UtilityOverlay {
    pub fn picker(
        graphics: Rc<D2dContext>,
        wake: HWND,
        format: ColorFormat,
        template: String,
        pin_enabled: Rc<Cell<bool>>,
        pending_pin: ColorPinMailbox,
    ) -> Result<Self> {
        Self::new(
            graphics,
            wake,
            Kind::Picker {
                format: Cell::new(format),
                position: Cell::new(POINT {
                    x: i32::MIN,
                    y: i32::MIN,
                }),
                rgb: Cell::new(None),
                previous: unsafe { GetForegroundWindow() },
                template,
                pin_enabled,
                pending_pin,
            },
        )
    }
    pub fn border(graphics: Rc<D2dContext>, wake: HWND, target: HWND, color: u32) -> Result<Self> {
        Self::new(
            graphics,
            wake,
            Kind::Border {
                target,
                color,
                bounds: Cell::new(RECT::default()),
            },
        )
    }
    fn new(graphics: Rc<D2dContext>, wake: HWND, kind: Kind) -> Result<Self> {
        crate::register_class(w!("Orbment.UtilityOverlay"), Some(window_proc))?;
        let overlay = Self(Rc::new(State {
            hwnd: Cell::new(HWND::default()),
            graphics: RefCell::new(graphics),
            kind,
            wake,
            scale: Cell::new(1.0),
            dirty: Cell::new(true),
        }));
        unsafe {
            let passive = if matches!(overlay.0.kind, Kind::Border { .. }) {
                WS_EX_TRANSPARENT | WS_EX_NOACTIVATE
            } else {
                WINDOW_EX_STYLE::default()
            };
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | passive,
                w!("Orbment.UtilityOverlay"),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(GetModuleHandleW(None)?.into()),
                Some(Rc::as_ptr(&overlay.0).cast()),
            )?;
            let hwnd = overlay.0.hwnd.get();
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            ensure!(
                SetTimer(
                    Some(hwnd),
                    1,
                    if matches!(overlay.0.kind, Kind::Picker { .. }) {
                        16
                    } else {
                        100
                    },
                    None
                ) != 0,
                "Overlay timer failed"
            );
        }
        overlay.0.tick()?;
        if matches!(overlay.0.kind, Kind::Picker { .. }) {
            unsafe {
                let _ = SetForegroundWindow(overlay.0.hwnd.get());
                SetCapture(overlay.0.hwnd.get());
            }
        }
        Ok(overlay)
    }
    pub fn is_alive(&self) -> bool {
        !self.0.hwnd.get().is_invalid()
    }
    pub fn rebuild_graphics(&self, graphics: Rc<D2dContext>) {
        *self.0.graphics.borrow_mut() = graphics;
        self.0.dirty.set(true);
    }
}
impl Drop for UtilityOverlay {
    fn drop(&mut self) {
        if self.is_alive() {
            self.0.close();
        }
    }
}
impl State {
    fn close(&self) {
        unsafe {
            let hwnd = self.hwnd.replace(HWND::default());
            if hwnd.is_invalid() {
                return;
            }
            let restore = GetForegroundWindow() == hwnd;
            let _ = DestroyWindow(hwnd);
            if restore && let Kind::Picker { previous, .. } = &self.kind {
                let _ = SetForegroundWindow(*previous);
            }
        }
        crate::wake(self.wake);
    }
    fn tick(&self) -> Result<()> {
        match &self.kind {
            Kind::Picker {
                format,
                position,
                rgb,
                template,
                pin_enabled,
                ..
            } => unsafe {
                if GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0 {
                    self.close();
                    return Ok(());
                }
                let mut cursor = POINT::default();
                GetCursorPos(&mut cursor)?;
                let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
                let (mut dpi_x, mut dpi_y) = (96, 96);
                GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y)?;
                let scale = dpi_scale(dpi_x);
                let dpi_changed = self.scale.replace(scale) != scale;
                let color = screen_pixel(cursor.x, cursor.y)?;
                let moved = cursor.x != position.get().x || cursor.y != position.get().y;
                let changed = rgb
                    .get()
                    .is_none_or(|old| old.r != color.r || old.g != color.g || old.b != color.b);
                if !moved && !changed && !dpi_changed && !self.dirty.replace(false) {
                    return Ok(());
                }
                position.set(cursor);
                rgb.set(Some(color));
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                ensure!(
                    GetMonitorInfoW(monitor, &mut info).as_bool(),
                    "Monitor bounds unavailable"
                );
                let bounds = picker_bounds(cursor, info.rcMonitor, scale);
                self.paint(
                    bounds,
                    |target| {
                        let background =
                            target.CreateSolidColorBrush(&rgba(0.10, 0.11, 0.13, 1.0), None)?;
                        target.FillRectangle(
                            &D2D_RECT_F {
                                left: 0.0,
                                top: 0.0,
                                right: 264.0,
                                bottom: 224.0,
                            },
                            &background,
                        );
                        for row in 0..11 {
                            for col in 0..11 {
                                let sample = screen_pixel(
                                    (cursor.x + col - 5)
                                        .clamp(info.rcMonitor.left, info.rcMonitor.right - 1),
                                    (cursor.y + row - 5)
                                        .clamp(info.rcMonitor.top, info.rcMonitor.bottom - 1),
                                )?;
                                let brush = target.CreateSolidColorBrush(
                                    &rgba(
                                        sample.r as f32 / 255.0,
                                        sample.g as f32 / 255.0,
                                        sample.b as f32 / 255.0,
                                        1.0,
                                    ),
                                    None,
                                )?;
                                let rect = D2D_RECT_F {
                                    left: 44.0 + col as f32 * 16.0,
                                    top: 4.0 + row as f32 * 16.0,
                                    right: 59.0 + col as f32 * 16.0,
                                    bottom: 19.0 + row as f32 * 16.0,
                                };
                                target.FillRectangle(&rect, &brush);
                            }
                        }
                        let white =
                            target.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 1.0), None)?;
                        target.DrawRectangle(
                            &D2D_RECT_F {
                                left: 123.0,
                                top: 83.0,
                                right: 140.0,
                                bottom: 100.0,
                            },
                            &white,
                            2.0,
                            None,
                        );
                        let text = format!(
                            "{}\n{}",
                            if format.get() == ColorFormat::Custom {
                                color.format_template(template)
                            } else {
                                color.format(format.get())
                            },
                            tr(if pin_enabled.get() {
                                "utilities.picker_pin_help"
                            } else {
                                "utilities.picker_help"
                            })
                        );
                        let text: Vec<u16> = text.encode_utf16().collect();
                        let font = self.graphics.borrow().text_format("Segoe UI", 12.0, 0)?;
                        target.DrawText(
                            &text,
                            &font,
                            &D2D_RECT_F {
                                left: 6.0,
                                top: 182.0,
                                right: 260.0,
                                bottom: 224.0,
                            },
                            &white,
                            D2D1_DRAW_TEXT_OPTIONS_CLIP,
                            windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL,
                        );
                        Ok(())
                    },
                )?;
            },
            Kind::Border {
                target,
                color,
                bounds,
            } => unsafe {
                if !IsWindow(Some(*target)).as_bool()
                    || GetWindowLongPtrW(*target, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 == 0
                {
                    self.close();
                    return Ok(());
                }
                if IsIconic(*target).as_bool() || !IsWindowVisible(*target).as_bool() {
                    let _ = ShowWindow(self.hwnd.get(), SW_HIDE);
                    bounds.set(RECT::default());
                    return Ok(());
                }
                let mut rect = RECT::default();
                GetWindowRect(*target, &mut rect)?;
                let scale = dpi_scale(GetDpiForWindow(*target));
                let dpi_changed = self.scale.replace(scale) != scale;
                if rect == bounds.get() && !dpi_changed && !self.dirty.replace(false) {
                    return Ok(());
                }
                bounds.set(rect);
                self.paint(rect, |target| {
                    let brush = target.CreateSolidColorBrush(
                        &rgba(
                            (color & 255) as f32 / 255.0,
                            ((color >> 8) & 255) as f32 / 255.0,
                            ((color >> 16) & 255) as f32 / 255.0,
                            1.0,
                        ),
                        None,
                    )?;
                    target.DrawRectangle(
                        &D2D_RECT_F {
                            left: 1.5,
                            top: 1.5,
                            right: (rect.right - rect.left) as f32 / scale - 1.5,
                            bottom: (rect.bottom - rect.top) as f32 / scale - 1.5,
                        },
                        &brush,
                        3.0,
                        None,
                    );
                    Ok(())
                })?;
            },
        }
        Ok(())
    }
    fn paint(
        &self,
        bounds: RECT,
        draw: impl FnOnce(&ID2D1DCRenderTarget) -> Result<()>,
    ) -> Result<()> {
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        if width <= 0 || height <= 0 {
            return Ok(());
        }
        let bitmap = Bitmap::new(width, height)?;
        let target = self.graphics.borrow().target()?;
        unsafe {
            target.BindDC(
                bitmap.dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                },
            )?;
            let dpi = 96.0 * self.scale.get();
            target.SetDpi(dpi, dpi);
            target.BeginDraw();
            target.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
            let rendered = draw(&target);
            let ended = target.EndDraw(None, None);
            rendered?;
            ended?;
            UpdateLayeredWindow(
                self.hwnd.get(),
                None,
                Some(&POINT {
                    x: bounds.left,
                    y: bounds.top,
                }),
                Some(&SIZE {
                    cx: width,
                    cy: height,
                }),
                Some(bitmap.dc),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                }),
                ULW_ALPHA,
            )?;
            let _ = ShowWindow(self.hwnd.get(), SW_SHOWNOACTIVATE);
        }
        Ok(())
    }
}
fn dpi_scale(dpi: u32) -> f32 {
    (dpi as f32 / 96.0).max(1.0)
}

fn picker_bounds(cursor: POINT, monitor: RECT, scale: f32) -> RECT {
    let width = (264.0 * scale).round() as i32;
    let height = (224.0 * scale).round() as i32;
    let offset = (24.0 * scale).round() as i32;
    let x = if cursor.x + width + offset <= monitor.right {
        cursor.x + offset
    } else {
        cursor.x - width - offset
    }.clamp(monitor.left, (monitor.right - width).max(monitor.left));
    let y = (cursor.y + offset)
        .clamp(monitor.top, (monitor.bottom - height).max(monitor.top));
    RECT { left: x, top: y, right: x + width, bottom: y + height }
}

fn rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
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
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            let state = &*(create.lpCreateParams as *const State);
            state.hwnd.set(hwnd);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
        if !ptr.is_null() {
            let state = &*ptr;
            if message == WM_DPICHANGED {
                // The cursor/target monitor owns geometry; recompute outside this synchronous callback.
                state.dirty.set(true);
                let _ = PostMessageW(Some(hwnd), WM_TIMER, WPARAM(1), LPARAM(0));
                return LRESULT(0);
            }
            if message == WM_TIMER {
                if let Err(error) = state.tick() {
                    tracing::warn!(%error,"Utility overlay failed");
                    state.close();
                }
                return LRESULT(0);
            }
            if let Kind::Picker {
                format,
                rgb,
                template,
                pin_enabled,
                pending_pin,
                ..
            } = &state.kind
            {
                if message == WM_KEYDOWN && wparam.0 == VK_ESCAPE.0 as usize {
                    state.close();
                    return LRESULT(0);
                }
                if message == WM_KEYDOWN && wparam.0 == b'P' as usize && pin_enabled.get() {
                    let mut point = POINT::default();
                    match GetCursorPos(&mut point)
                        .map_err(anyhow::Error::from)
                        .and_then(|_| screen_pixel(point.x, point.y))
                    {
                        Ok(color) => {
                            pending_pin.set(Some(([color.r, color.g, color.b], point.x, point.y)))
                        }
                        Err(error) => tracing::warn!(%error, "Reading color card pixel"),
                    }
                    state.close();
                    return LRESULT(0);
                }
                if message == WM_LBUTTONUP {
                    let mut point = POINT::default();
                    let copied = GetCursorPos(&mut point)
                        .map_err(anyhow::Error::from)
                        .and_then(|_| screen_pixel(point.x, point.y))
                        .and_then(|color| {
                            copy_text(
                                hwnd.0 as isize,
                                &if format.get() == ColorFormat::Custom {
                                    color.format_template(template)
                                } else {
                                    color.format(format.get())
                                },
                            )
                        });
                    if let Err(error) = copied {
                        tracing::warn!(%error,"Color copy failed");
                    }
                    state.close();
                    return LRESULT(0);
                }
                if message == WM_RBUTTONUP {
                    format.set(match format.get() {
                        ColorFormat::Hex => ColorFormat::Rgb,
                        ColorFormat::Rgb => ColorFormat::Hsl,
                        ColorFormat::Hsl => ColorFormat::Hsb,
                        ColorFormat::Hsb => ColorFormat::Cmyk,
                        ColorFormat::Cmyk => ColorFormat::Oklab,
                        ColorFormat::Oklab => ColorFormat::Oklch,
                        ColorFormat::Oklch => ColorFormat::Lab,
                        ColorFormat::Lab => ColorFormat::Bgr,
                        ColorFormat::Bgr => ColorFormat::Custom,
                        ColorFormat::Custom => ColorFormat::Hex,
                    });
                    rgb.set(None);
                    if let Err(error) = state.tick() {
                        tracing::warn!(%error,"Picker redraw failed");
                        state.close();
                    }
                    return LRESULT(0);
                }
                if message == WM_KILLFOCUS || message == WM_CAPTURECHANGED {
                    state.close();
                    return LRESULT(0);
                }
            }
            if message == WM_NCHITTEST && matches!(state.kind, Kind::Border { .. }) {
                return LRESULT(HTTRANSPARENT as isize);
            }
            if message == WM_NCDESTROY {
                let _ = KillTimer(Some(hwnd), 1);
                state.hwnd.set(HWND::default());
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_scales_and_stays_on_negative_coordinate_monitor() {
        let monitor = RECT { left: -1920, top: 0, right: 0, bottom: 1080 };
        for dpi in [96, 144, 192] {
            let scale = dpi_scale(dpi);
            let rect = picker_bounds(POINT { x: -100, y: 1040 }, monitor, scale);
            assert_eq!(rect.right - rect.left, (264.0 * scale).round() as i32);
            assert_eq!(rect.bottom - rect.top, (224.0 * scale).round() as i32);
            assert!(rect.left >= monitor.left && rect.right <= monitor.right);
            assert!(rect.top >= monitor.top && rect.bottom <= monitor.bottom);
        }
        assert_eq!(dpi_scale(0), 1.0);
    }
}
