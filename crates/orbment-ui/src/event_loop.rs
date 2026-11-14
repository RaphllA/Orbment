use anyhow::{Result, bail, ensure};
use std::{
    cell::RefCell,
    collections::VecDeque,
    marker::PhantomData,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    rc::Rc,
};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, DataExchange::COPYDATASTRUCT, LibraryLoader::GetModuleHandleW},
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

pub(crate) const TRAY_MESSAGE: u32 = WM_APP + 1;
pub(crate) const WAKE_MESSAGE: u32 = WM_APP + 2;
pub(crate) const MUTE_MESSAGE: u32 = WM_APP + 3;
pub(crate) const CLIPBOARD_MESSAGE: u32 = WM_APP + 4;
pub(crate) const SEARCH_MESSAGE: u32 = WM_APP + 5;
const IDLE_TIMER: usize = 1;
pub const PIN_IMAGE_COPYDATA_TAG: usize = 0x4F425049;
const MAX_PIN_PATH_UNITS: usize = 32768;

#[derive(Debug, Clone)]
pub enum UiEvent {
    Action(String),
    VolumeMute,
    Hotkey(i32),
    TaskbarCreated,
    Activate,
    Tray(u32),
    AudioTray(u32),
    MemoryTray(u32),
    DisplayChanged,
    Wake,
    IdleTimer,
    PinImage(PathBuf),
}

struct HostState {
    events: RefCell<VecDeque<UiEvent>>,
    taskbar_created: u32,
    activate: u32,
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

pub struct EventLoop {
    hwnd: HWND,
    state: Box<HostState>,
    _apartment: Apartment,
    _main_thread: PhantomData<Rc<()>>,
}

impl EventLoop {
    pub fn new() -> Result<Self> {
        let apartment = Apartment::new()?;
        crate::register_class(w!("Orbment.Host"), Some(host_proc))?;
        unsafe {
            let state = Box::new(HostState {
                events: RefCell::new(VecDeque::new()),
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                activate: RegisterWindowMessageW(w!("Orbment.Activate")),
            });
            if state.taskbar_created == 0 || state.activate == 0 {
                return Err(windows::core::Error::from_win32().into());
            }
            // A hidden top-level window receives Explorer's registered broadcasts.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("Orbment.Host"),
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(GetModuleHandleW(None)?.into()),
                Some((&*state as *const HostState).cast()),
            )?;
            let result = Self {
                hwnd,
                state,
                _apartment: apartment,
                _main_thread: PhantomData,
            };
            ChangeWindowMessageFilterEx(hwnd, result.state.taskbar_created, MSGFLT_ALLOW, None)?;
            ChangeWindowMessageFilterEx(hwnd, result.state.activate, MSGFLT_ALLOW, None)?;
            ChangeWindowMessageFilterEx(hwnd, TRAY_MESSAGE, MSGFLT_ALLOW, None)?;
            for message in [WM_DROPFILES, WM_COPYDATA, 0x0049] {
                ChangeWindowMessageFilterEx(hwnd, message, MSGFLT_ALLOW, None)?;
            }
            Ok(result)
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn run(&self, mut callback: impl FnMut(UiEvent)) -> Result<()> {
        unsafe {
            loop {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    if message.message == WM_QUIT {
                        return Ok(());
                    }
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                    loop {
                        let event = self.state.events.borrow_mut().pop_front();
                        match event {
                            Some(event) => callback(event),
                            None => break,
                        }
                    }
                }
                if MsgWaitForMultipleObjectsEx(None, u32::MAX, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
                    == WAIT_FAILED
                {
                    bail!(windows::core::Error::from_win32());
                }
            }
        }
    }

    pub fn schedule_idle(&self, deadline: Option<u64>, now_ms: u64) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), IDLE_TIMER);
            if let Some(deadline) = deadline {
                let delay = deadline.saturating_sub(now_ms).clamp(1, 0x7fff_ffff) as u32;
                if SetTimer(Some(self.hwnd), IDLE_TIMER, delay, None) == 0 {
                    tracing::error!("Failed to schedule UI idle timer");
                }
            }
        }
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

pub fn wake(hwnd: HWND) {
    unsafe {
        if let Err(error) = PostMessageW(Some(hwnd), WAKE_MESSAGE, WPARAM(0), LPARAM(0)) {
            tracing::warn!(%error, "UI wake failed");
        }
    }
}

/// Forward an image-open request to the existing UI host without depending on MCP.
pub fn send_pin_image(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "Pin image path must be absolute");
    let mut text: Vec<u16> = path.as_os_str().encode_wide().collect();
    ensure!(
        !text.contains(&0) && text.len() < MAX_PIN_PATH_UNITS,
        "Invalid pin image path"
    );
    text.push(0);
    unsafe {
        let hwnd = FindWindowW(w!("Orbment.Host"), None)?;
        let data = COPYDATASTRUCT {
            dwData: PIN_IMAGE_COPYDATA_TAG,
            cbData: (text.len() * 2) as u32,
            lpData: text.as_ptr().cast_mut().cast(),
        };
        let mut accepted = 0;
        ensure!(
            SendMessageTimeoutW(
                hwnd,
                WM_COPYDATA,
                WPARAM(0),
                LPARAM((&data as *const COPYDATASTRUCT) as isize),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                2000,
                Some(&mut accepted)
            )
            .0 != 0
                && accepted == 1,
            "Existing Orbment instance did not accept the image"
        );
    }
    Ok(())
}

unsafe extern "system" fn host_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const HostState;
        if !pointer.is_null() {
            let state = &*pointer;
            if message == WM_COPYDATA && lparam.0 != 0 {
                let data = &*(lparam.0 as *const COPYDATASTRUCT);
                if data.dwData != PIN_IMAGE_COPYDATA_TAG
                    || data.lpData.is_null()
                    || data.cbData < 4
                    || data.cbData as usize > MAX_PIN_PATH_UNITS * 2
                    || !data.cbData.is_multiple_of(2)
                {
                    return LRESULT(0);
                }
                let bytes =
                    std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize);
                let units: Vec<u16> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect();
                if units.last() != Some(&0) || units[..units.len() - 1].contains(&0) {
                    return LRESULT(0);
                }
                let path = PathBuf::from(std::ffi::OsString::from_wide(&units[..units.len() - 1]));
                if !path.is_absolute() {
                    return LRESULT(0);
                }
                state.events.borrow_mut().push_back(UiEvent::PinImage(path));
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                return LRESULT(1);
            }
            let event = match message {
                WM_HOTKEY => Some(UiEvent::Hotkey(crate::hotkeys::logical_id(wparam.0 as i32))),
                TRAY_MESSAGE if (lparam.0 as u32 >> 16) == 2 => {
                    Some(UiEvent::AudioTray((lparam.0 as u32) & 0xffff))
                }
                TRAY_MESSAGE if (lparam.0 as u32 >> 16) == 3 => {
                    Some(UiEvent::MemoryTray((lparam.0 as u32) & 0xffff))
                }
                WM_DISPLAYCHANGE => Some(UiEvent::DisplayChanged),
                TRAY_MESSAGE => Some(UiEvent::Tray((lparam.0 as u32) & 0xffff)),
                WAKE_MESSAGE => Some(UiEvent::Wake),
                MUTE_MESSAGE => Some(UiEvent::VolumeMute),
                CLIPBOARD_MESSAGE => Some(UiEvent::Action("clipboard.toggle".into())),
                SEARCH_MESSAGE => Some(UiEvent::Action("file_search.toggle".into())),
                WM_TIMER if wparam.0 == IDLE_TIMER => {
                    let _ = KillTimer(Some(hwnd), IDLE_TIMER);
                    Some(UiEvent::IdleTimer)
                }
                value if value == state.taskbar_created => Some(UiEvent::TaskbarCreated),
                value if value == state.activate => Some(UiEvent::Activate),
                _ => None,
            };
            if let Some(event) = event {
                state.events.borrow_mut().push_back(event);
                // Sent broadcasts can arrive inside PeekMessage without yielding a MSG.
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                return LRESULT(0);
            }
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        if message == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
