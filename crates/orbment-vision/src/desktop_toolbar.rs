use crate::capture_overlay::{virtual_desktop, wide};
use anyhow::{Result, ensure};
use std::cell::RefCell;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

#[derive(Clone, Copy, Debug)]
pub enum ToolbarAction {
    Capture,
    Restore,
    Record,
    TogglePins,
}
struct State {
    events: Vec<ToolbarAction>,
    mode: u8,
    expanded: bool,
    wake: Option<isize>,
    scale: f64,
}
impl State {
    fn resize(&self, hwnd: HWND, suggested: Option<RECT>) -> Result<()> {
        let (width, height) = toolbar_size(self.scale, self.expanded);
        unsafe {
            let mut rect = RECT::default();
            if let Some(suggested) = suggested {
                rect = suggested;
            } else {
                GetWindowRect(hwnd, &mut rect)?;
            }
            let mut monitor = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            ensure!(GetMonitorInfoW(MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST), &mut monitor).as_bool(),
                "Desktop toolbar monitor unavailable");
            let work = monitor.rcWork;
            SetWindowPos(
                hwnd, None,
                (rect.right - width).clamp(work.left, (work.right - width).max(work.left)),
                rect.top.clamp(work.top, (work.bottom - height).max(work.top)),
                width, height, SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        Ok(())
    }
}

fn toolbar_size(scale: f64, expanded: bool) -> (i32, i32) {
    (((if expanded { 168. } else { 12. }) * scale).round() as i32,
        (44. * scale).round() as i32)
}

fn toolbar_hit(scale: f64, x: i32, y: i32) -> Option<usize> {
    let (width, height) = toolbar_size(scale, true);
    (x >= 0 && x < width && y >= 0 && y < height)
        .then(|| (f64::from(x) / (56. * scale)) as usize)
}
pub struct DesktopToolbar {
    hwnd: HWND,
    state: Box<RefCell<State>>,
}
impl DesktopToolbar {
    pub fn new(mode: u8) -> Result<Self> {
        ensure!(mode <= 2, "Invalid desktop toolbar mode");
        unsafe {
            let instance = GetModuleHandleW(None)?;
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(toolbar_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_HAND)?,
                lpszClassName: w!("OrbmentDesktopToolbar"),
                ..Default::default()
            });
            let desktop = virtual_desktop();
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("OrbmentDesktopToolbar"),
                w!("Orbment"),
                WS_POPUP,
                (desktop.x + desktop.width - 168.0) as i32,
                (desktop.y + desktop.height / 2.0) as i32,
                168,
                44,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            let state = Box::new(RefCell::new(State {
                events: Vec::new(),
                mode,
                expanded: mode != 1,
                wake: None,
                scale: (f64::from(GetDpiForWindow(hwnd)) / 96.).max(1.),
            }));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*state as *const _ as isize);
            let mut result = Self { hwnd, state };
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;
            result.configure(mode)?;
            Ok(result)
        }
    }
    pub fn configure(&mut self, mode: u8) -> Result<()> {
        ensure!(mode <= 2, "Invalid desktop toolbar mode");
        let mut state = self.state.borrow_mut();
        state.mode = mode;
        state.expanded = mode != 1;
        state.scale = (f64::from(unsafe { GetDpiForWindow(self.hwnd) }) / 96.).max(1.);
        unsafe {
            let _ = ShowWindow(
                self.hwnd,
                if mode == 2 {
                    SW_HIDE
                } else {
                    SW_SHOWNOACTIVATE
                },
            );
            state.resize(self.hwnd, None)?;
        }
        Ok(())
    }
    pub fn poll(&mut self) -> Vec<ToolbarAction> {
        std::mem::take(&mut self.state.borrow_mut().events)
    }
    pub fn set_wake_hwnd(&mut self, hwnd: isize) {
        self.state.borrow_mut().wake = Some(hwnd);
    }
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
    pub fn visible(&self) -> bool {
        self.state.borrow().mode != 2
    }
}
impl Drop for DesktopToolbar {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
unsafe extern "system" fn toolbar_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let Ok(mut state) = (*ptr).try_borrow_mut() else {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        };
        match message {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let brush = CreateSolidBrush(COLORREF(0x392c23));
                FillRect(dc, &rect, brush);
                let _ = DeleteObject(HGDIOBJ(brush.0));
                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, COLORREF(0xffffff));
                let font = CreateFontW(
                    -(14. * state.scale).round() as i32, 0, 0, 0, 400, 0, 0, 0,
                    DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY, DEFAULT_PITCH.0 as u32, w!("Microsoft YaHei UI"),
                );
                let old = SelectObject(dc, HGDIOBJ(font.0));
                if state.expanded {
                    for (index, key) in [
                        "vision.toolbar_capture",
                        "vision.toolbar_restore",
                        "vision.toolbar_record",
                    ]
                    .iter()
                    .enumerate()
                    {
                        let mut text = wide(&orbment_core::i18n::tr(key));
                        text.pop();
                        let mut cell = RECT {
                            left: (index as f64 * 56. * state.scale).round() as i32,
                            top: 0,
                            right: ((index + 1) as f64 * 56. * state.scale).round() as i32,
                            bottom: (44. * state.scale).round() as i32,
                        };
                        DrawTextW(
                            dc,
                            &mut text,
                            &mut cell,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                        );
                    }
                }
                SelectObject(dc, old);
                let _ = DeleteObject(HGDIOBJ(font.0));
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }
            WM_MOUSEMOVE if state.mode == 1 && !state.expanded => {
                state.expanded = true;
                let _ = state.resize(hwnd, None);
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut track);
                return LRESULT(0);
            }
            WM_MOUSELEAVE if state.mode == 1 => {
                state.expanded = false;
                let _ = state.resize(hwnd, None);
                return LRESULT(0);
            }
            WM_LBUTTONUP if state.expanded => {
                let x = lparam.0 as u16 as i16 as i32;
                let y = (lparam.0 >> 16) as u16 as i16 as i32;
                let Some(index) = toolbar_hit(state.scale, x, y) else {
                    return LRESULT(0);
                };
                state.events.push(match index {
                    0 => ToolbarAction::Capture,
                    1 => ToolbarAction::Restore,
                    _ => ToolbarAction::Record,
                });
                if let Some(wake) = state.wake {
                    orbment_ui::wake(HWND(wake as *mut _));
                }
                return LRESULT(0);
            }
            WM_RBUTTONUP => {
                state.events.push(ToolbarAction::TogglePins);
                if let Some(wake) = state.wake {
                    orbment_ui::wake(HWND(wake as *mut _));
                }
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                state.scale = (f64::from(wparam.0 as u16) / 96.).max(1.);
                let _ = state.resize(hwnd, Some(*(lparam.0 as *const RECT)));
                return LRESULT(0);
            }
            WM_CLOSE => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                return LRESULT(0);
            }
            _ => {}
        }
        drop(state);
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_scaled_button_bounds_match_hit_testing() {
        for scale in [1., 1.25, 2., 4.] {
            let (width, height) = toolbar_size(scale, true);
            assert_eq!(width, (168. * scale) as i32);
            assert_eq!(height, (44. * scale) as i32);
            assert_eq!(toolbar_size(scale, false).0, (12. * scale) as i32);
            for index in 0..3 {
                let center = ((index as f64 * 56. + 28.) * scale) as i32;
                assert_eq!(toolbar_hit(scale, center, height / 2), Some(index));
            }
            assert_eq!(toolbar_hit(scale, -1, 0), None);
            assert_eq!(toolbar_hit(scale, width, height / 2), None);
            assert_eq!(toolbar_hit(scale, 0, height), None);
        }
    }
}
