use crate::config::{ColorFormat, KeepAwakeDuration, KeepAwakeMode, TopmostTarget};
use crate::i18n::tr;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Devices::Display::*,
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        System::{
            Com::*, DataExchange::*, Memory::*, Power::*, Threading::*, Variant::VARIANT, Wmi::*,
        },
        UI::WindowsAndMessaging::*,
    },
    core::{BSTR, PCWSTR, PWSTR, w},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}
impl Rgb {
    pub fn hsl(self) -> [f64; 3] {
        let [r, g, b] = [self.r, self.g, self.b].map(|v| f64::from(v) / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        let l = (max + min) / 2.0;
        if d == 0.0 {
            return [0.0, 0.0, l];
        }
        let h = if max == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        [h * 60.0, d / (1.0 - (2.0 * l - 1.0).abs()), l]
    }
    fn linear_rgb(self) -> [f64; 3] {
        [self.r, self.g, self.b].map(|value| {
            let component = f64::from(value) / 255.0;
            if component <= 0.04045 {
                component / 12.92
            } else {
                ((component + 0.055) / 1.055).powf(2.4)
            }
        })
    }

    pub fn hsb(self) -> [f64; 3] {
        let [r, g, b] = [self.r, self.g, self.b].map(|value| f64::from(value) / 255.0);
        let maximum = r.max(g).max(b);
        let minimum = r.min(g).min(b);
        [
            self.hsl()[0],
            if maximum == 0.0 {
                0.0
            } else {
                (maximum - minimum) / maximum
            },
            maximum,
        ]
    }

    pub fn cmyk(self) -> [f64; 4] {
        let [r, g, b] = [self.r, self.g, self.b].map(|value| f64::from(value) / 255.0);
        let maximum = r.max(g).max(b);
        if maximum == 0.0 {
            return [0.0, 0.0, 0.0, 1.0];
        }
        [
            (maximum - r) / maximum,
            (maximum - g) / maximum,
            (maximum - b) / maximum,
            1.0 - maximum,
        ]
    }

    pub fn lab(self) -> [f64; 3] {
        let [r, g, b] = self.linear_rgb();
        let [x, y, z] = [
            (0.4124564 * r + 0.3575761 * g + 0.1804375 * b) / 0.95047,
            0.2126729 * r + 0.7151522 * g + 0.0721750 * b,
            (0.0193339 * r + 0.1191920 * g + 0.9503041 * b) / 1.08883,
        ]
        .map(|value| {
            if value > 216.0 / 24389.0 {
                value.cbrt()
            } else {
                value * (24389.0 / 27.0) / 116.0 + 16.0 / 116.0
            }
        });
        [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
    }

    pub fn oklab(self) -> [f64; 3] {
        let [r, g, b] = self.linear_rgb();
        let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
        let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
        let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
        let light = 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s;
        let a = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s;
        let b = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s;
        [light, a, b]
    }

    pub fn oklch(self) -> [f64; 3] {
        let [light, a, b] = self.oklab();
        let chroma = a.hypot(b);
        [
            light,
            chroma,
            if chroma < 1e-7 {
                0.0
            } else {
                b.atan2(a).to_degrees().rem_euclid(360.0)
            },
        ]
    }

    /// RGB channel template tokens have no closing percent: `%ReX%GrX%BlX`.
    /// X/x selects hexadecimal case, D selects decimal and F a normalized channel.
    pub fn format_template(self, mut template: &str) -> String {
        let mut result = String::new();
        while let Some(index) = template.find('%') {
            result.push_str(&template[..index]);
            template = &template[index..];
            if let Some(rest) = template.strip_prefix("%%") {
                result.push('%');
                template = rest;
                continue;
            }
            if let Some((token, format)) = [
                ("%HEX", ColorFormat::Hex),
                ("%RGB", ColorFormat::Rgb),
                ("%BGR", ColorFormat::Bgr),
                ("%HSL", ColorFormat::Hsl),
                ("%HSB", ColorFormat::Hsb),
                ("%CMYK", ColorFormat::Cmyk),
                ("%Oklab", ColorFormat::Oklab),
                ("%Oklch", ColorFormat::Oklch),
                ("%Lab", ColorFormat::Lab),
            ]
            .into_iter()
            .find(|(token, _)| template.starts_with(token))
            {
                result.push_str(&self.format(format));
                template = &template[token.len()..];
                continue;
            }
            let channel = [
                ("%Re", self.r),
                ("%Gr", self.g),
                ("%Bl", self.b),
                ("%Al", 255),
            ]
            .into_iter()
            .find(|(prefix, _)| template.starts_with(prefix));
            if let Some((_, channel)) = channel {
                let value = match template.as_bytes().get(3) {
                    Some(b'X') => Some(format!("{channel:02X}")),
                    Some(b'x') => Some(format!("{channel:02x}")),
                    Some(b'D' | b'd') => Some(channel.to_string()),
                    Some(b'F' | b'f') => Some(format!("{:.3}", f64::from(channel) / 255.0)),
                    _ => None,
                };
                if let Some(value) = value {
                    result.push_str(&value);
                    template = &template[4..];
                    continue;
                }
            }
            result.push('%');
            template = &template[1..];
        }
        result.push_str(template);
        result
    }
    pub fn format(self, format: ColorFormat) -> String {
        match format {
            ColorFormat::Hex => format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b),
            ColorFormat::Rgb => format!("rgb({}, {}, {})", self.r, self.g, self.b),
            ColorFormat::Hsl => {
                let [h, s, l] = self.hsl();
                format!("hsl({h:.1}, {:.1}%, {:.1}%)", s * 100.0, l * 100.0)
            }
            ColorFormat::Oklch => {
                let [l, c, h] = self.oklch();
                format!("oklch({:.2}% {c:.4} {h:.1})", l * 100.0)
            }
            ColorFormat::Hsb => {
                let [h, s, b] = self.hsb();
                format!("hsb({h:.1}, {:.1}%, {:.1}%)", s * 100.0, b * 100.0)
            }
            ColorFormat::Cmyk => {
                let [c, m, y, k] = self.cmyk().map(|value| value * 100.0);
                format!("cmyk({c:.1}%, {m:.1}%, {y:.1}%, {k:.1}%)")
            }
            ColorFormat::Oklab => {
                let [l, a, b] = self.oklab();
                format!("oklab({:.2}% {a:.4} {b:.4})", l * 100.0)
            }
            ColorFormat::Lab => {
                let [l, a, b] = self.lab();
                format!("lab({l:.2} {a:.2} {b:.2})")
            }
            ColorFormat::Bgr => {
                (u32::from(self.r) | (u32::from(self.g) << 8) | (u32::from(self.b) << 16))
                    .to_string()
            }
            ColorFormat::Custom => self.format_template("%ReX%GrX%BlX"),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TopmostChange {
    pub hwnd: isize,
    pub enabled: bool,
    pub native_border: bool,
}
pub fn toggle_topmost(target: TopmostTarget, color: u32) -> Result<TopmostChange> {
    set_topmost(None, None, target, None, color)
}
pub fn set_topmost(
    hwnd: Option<isize>,
    title: Option<&str>,
    target: TopmostTarget,
    enabled: Option<bool>,
    color: u32,
) -> Result<TopmostChange> {
    ensure!(
        hwnd.is_none() || title.is_none(),
        "{}",
        tr("config.invalid_value")
    );
    let implicit_target = hwnd.is_none() && title.is_none();
    unsafe {
        let hwnd = if let Some(hwnd) = hwnd {
            HWND(hwnd as _)
        } else if let Some(title) = title {
            let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
            FindWindowW(None, PCWSTR(title.as_ptr()))?
        } else {
            match target {
                TopmostTarget::Foreground => GetForegroundWindow(),
                TopmostTarget::UnderCursor => {
                    let mut point = POINT::default();
                    GetCursorPos(&mut point)?;
                    WindowFromPoint(point)
                }
            }
        };
        let hwnd = GetAncestor(hwnd, GA_ROOT);
        ensure!(!hwnd.is_invalid(), "{}", tr("utilities.no_window"));
        if implicit_target {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            ensure!(
                pid != 0 && pid != std::process::id(),
                "{}",
                tr("utilities.no_window")
            );
        }
        let enabled = enabled
            .unwrap_or_else(|| GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 == 0);
        SetWindowPos(
            hwnd,
            Some(if enabled {
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
        let color = if enabled { color } else { 0xffffffff };
        let native_border =
            DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, (&color as *const u32).cast(), 4)
                .is_ok();
        Ok(TopmostChange {
            hwnd: hwnd.0 as isize,
            enabled,
            native_border,
        })
    }
}
pub fn screen_pixel(x: i32, y: i32) -> Result<Rgb> {
    unsafe {
        let dc = GetDC(None);
        ensure!(!dc.is_invalid(), "{}", tr("utilities.screen_unavailable"));
        let pixel = GetPixel(dc, x, y);
        ReleaseDC(None, dc);
        ensure!(
            pixel.0 != 0xffffffff,
            "{}",
            tr("utilities.screen_unavailable")
        );
        Ok(Rgb {
            r: pixel.0 as u8,
            g: (pixel.0 >> 8) as u8,
            b: (pixel.0 >> 16) as u8,
        })
    }
}
pub fn copy_text(owner: isize, text: &str) -> Result<()> {
    struct Clipboard;
    impl Drop for Clipboard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }
    unsafe {
        OpenClipboard(Some(HWND(owner as _)))?;
        let _clipboard = Clipboard;
        let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let memory = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2)?;
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(windows::core::Error::from_win32().into());
        }
        std::ptr::copy_nonoverlapping(text.as_ptr(), pointer.cast(), text.len());
        let _ = GlobalUnlock(memory);
        if let Err(error) =
            EmptyClipboard().and_then(|()| SetClipboardData(13, Some(HANDLE(memory.0))).map(|_| ()))
        {
            let _ = GlobalFree(Some(memory));
            return Err(error.into());
        }
        Ok(())
    }
}

struct OwnedHandle(HANDLE);
unsafe impl Send for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct PowerRequest {
    handle: OwnedHandle,
    display: bool,
}
impl Drop for PowerRequest {
    fn drop(&mut self) {
        unsafe {
            if self.display {
                let _ = PowerClearRequest(self.handle.0, PowerRequestDisplayRequired);
            }
            let _ = PowerClearRequest(self.handle.0, PowerRequestSystemRequired);
        }
    }
}
pub struct KeepAwakeService {
    cancel: Option<OwnedHandle>,
    worker: Option<JoinHandle<()>>,
    active: Arc<AtomicBool>,
    notify: Arc<dyn Fn() + Send + Sync>,
}
impl KeepAwakeService {
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            cancel: None,
            worker: None,
            active: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(notify),
        }
    }
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub fn set(
        &mut self,
        mode: KeepAwakeMode,
        duration: KeepAwakeDuration,
        pid: Option<u32>,
    ) -> Result<()> {
        let process = if duration == KeepAwakeDuration::Process {
            let pid = pid
                .filter(|pid| *pid > 0)
                .ok_or_else(|| anyhow::anyhow!(tr("config.invalid_value")))?;
            Some(OwnedHandle(unsafe {
                OpenProcess(PROCESS_SYNCHRONIZE, false, pid)?
            }))
        } else {
            None
        };
        let cancel = OwnedHandle(unsafe { CreateEventW(None, true, false, None)? });
        let mut reason: Vec<u16> = tr("utilities.power_reason")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let context = REASON_CONTEXT {
            Version: 0,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(reason.as_mut_ptr()),
            },
        };
        let request = PowerRequest {
            handle: OwnedHandle(unsafe { PowerCreateRequest(&context)? }),
            display: mode == KeepAwakeMode::Display,
        };
        unsafe {
            PowerSetRequest(request.handle.0, PowerRequestSystemRequired)?;
            if request.display {
                PowerSetRequest(request.handle.0, PowerRequestDisplayRequired)?;
            }
        }
        self.stop();
        let cancel_value = cancel.0.0 as usize;
        let active = self.active.clone();
        let notify = self.notify.clone();
        active.store(true, Ordering::Release);
        let worker = thread::Builder::new()
            .name("orbment-power".into())
            .spawn(move || {
                let _request = request;
                let mut handles = vec![HANDLE(cancel_value as _)];
                if let Some(process) = &process {
                    handles.push(process.0);
                }
                let timeout = match duration {
                    KeepAwakeDuration::Minutes30 => 1_800_000,
                    KeepAwakeDuration::Hour1 => 3_600_000,
                    KeepAwakeDuration::Hours2 => 7_200_000,
                    _ => INFINITE,
                };
                unsafe {
                    WaitForMultipleObjects(&handles, false, timeout);
                }
                drop(_request);
                active.store(false, Ordering::Release);
                notify();
            });
        match worker {
            Ok(worker) => {
                self.cancel = Some(cancel);
                self.worker = Some(worker);
                Ok(())
            }
            Err(error) => {
                self.active.store(false, Ordering::Release);
                Err(error.into())
            }
        }
    }
    pub fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            unsafe {
                let _ = SetEvent(cancel.0);
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
        self.active.store(false, Ordering::Release);
    }
}
impl Drop for KeepAwakeService {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MonitorBrightness {
    pub id: u64,
    pub name: String,
    pub percent: Option<u8>,
    pub error: Option<String>,
}
enum BrightnessCommand {
    Refresh,
    Set(u64, u8),
    Adjust(i16),
    SetDebounce(Duration),
    Stop,
}
pub struct BrightnessService {
    sender: Sender<BrightnessCommand>,
    state: Arc<Mutex<Vec<MonitorBrightness>>>,
    worker: Option<JoinHandle<()>>,
}
impl BrightnessService {
    pub fn new(debounce: Duration, notify: impl Fn() + Send + 'static) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(Vec::new()));
        let shared = state.clone();
        let worker = thread::Builder::new()
            .name("orbment-brightness".into())
            .spawn(move || brightness_worker(receiver, shared, debounce, notify))?;
        Ok(Self {
            sender,
            state,
            worker: Some(worker),
        })
    }
    pub fn snapshot(&self) -> Vec<MonitorBrightness> {
        self.state.lock().unwrap().clone()
    }
    pub fn refresh(&self) -> Result<()> {
        self.sender.send(BrightnessCommand::Refresh)?;
        Ok(())
    }
    pub fn set_debounce(&self, debounce: Duration) -> Result<()> {
        self.sender.send(BrightnessCommand::SetDebounce(debounce))?;
        Ok(())
    }
    pub fn set(&self, id: u64, percent: u8) -> Result<()> {
        ensure!(percent <= 100, "{}", tr("config.invalid_value"));
        ensure!(
            self.state
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.id == id && m.percent.is_some()),
            "{}",
            tr("utilities.monitor_missing")
        );
        self.sender.send(BrightnessCommand::Set(id, percent))?;
        Ok(())
    }
    pub fn adjust(&self, delta: i16) -> Result<()> {
        self.sender.send(BrightnessCommand::Adjust(delta))?;
        Ok(())
    }
}
impl Drop for BrightnessService {
    fn drop(&mut self) {
        let _ = self.sender.send(BrightnessCommand::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct PhysicalMonitor {
    identity: String,
    device_id: String,
    monitor: Option<PHYSICAL_MONITOR>,
    wmi_path: Option<BSTR>,
    min: u32,
    max: u32,
    snapshot: MonitorBrightness,
}
impl Drop for PhysicalMonitor {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor {
            unsafe {
                let _ = DestroyPhysicalMonitors(&[monitor]);
            }
        }
    }
}
unsafe extern "system" fn enumerate_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    unsafe {
        (&mut *(data.0 as *mut Vec<HMONITOR>)).push(monitor);
    }
    true.into()
}
fn physical_monitors() -> Vec<PhysicalMonitor> {
    let mut logical = Vec::<HMONITOR>::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enumerate_monitor),
            LPARAM((&mut logical as *mut Vec<HMONITOR>) as isize),
        );
    }
    let mut result = Vec::new();
    for logical in logical {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !unsafe { GetMonitorInfoW(logical, &mut info.monitorInfo) }.as_bool() {
            continue;
        }
        let display_name = wide_text(&info.szDevice);
        let mut count = 0;
        if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(logical, &mut count) }.is_err() {
            continue;
        }
        let mut physical = vec![PHYSICAL_MONITOR::default(); count as usize];
        if unsafe { GetPhysicalMonitorsFromHMONITOR(logical, &mut physical) }.is_err() {
            continue;
        }
        for (index, monitor) in physical.into_iter().enumerate() {
            let mut device = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            let device_id = if unsafe {
                EnumDisplayDevicesW(
                    PCWSTR(info.szDevice.as_ptr()),
                    index as u32,
                    &mut device,
                    EDD_GET_DEVICE_INTERFACE_NAME,
                )
            }
            .as_bool()
            {
                display_device_id(&wide_text(&device.DeviceID))
            } else {
                String::new()
            };
            let (mut min, mut current, mut max) = (0, 0, 0);
            let ok = unsafe {
                GetMonitorBrightness(monitor.hPhysicalMonitor, &mut min, &mut current, &mut max)
            } != 0
                && max > min;
            let description = monitor.szPhysicalMonitorDescription;
            let name = wide_text(&description);
            result.push(PhysicalMonitor {
                identity: format!("ddc:{display_name}:{index}"),
                device_id,
                monitor: Some(monitor),
                wmi_path: None,
                min,
                max,
                snapshot: MonitorBrightness {
                    id: 0,
                    name,
                    percent: ok.then(|| {
                        ((u64::from(current.saturating_sub(min)) * 100 / u64::from(max - min))
                            .min(100)) as u8
                    }),
                    error: (!ok).then(|| tr("utilities.brightness_unsupported")),
                },
            });
        }
    }
    result
}

fn wmi_services() -> Result<IWbemServices> {
    unsafe {
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
        let empty = BSTR::new();
        let services = locator.ConnectServer(
            &BSTR::from(r"ROOT\WMI"),
            &empty,
            &empty,
            &empty,
            0,
            &empty,
            None,
        )?;
        CoSetProxyBlanket(
            &services,
            10,
            0,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )?;
        Ok(services)
    }
}
fn wmi_monitors(services: &IWbemServices) -> Result<Vec<PhysicalMonitor>> {
    unsafe {
        let rows = services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from("SELECT * FROM WmiMonitorBrightness WHERE Active=TRUE"),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )?;
        let mut result = Vec::new();
        loop {
            let mut objects = [None];
            let mut count = 0;
            rows.Next(5000, &mut objects, &mut count).ok()?;
            if count == 0 {
                break;
            }
            let object = objects[0]
                .take()
                .ok_or_else(|| anyhow::anyhow!(tr("utilities.brightness_failed")))?;
            let mut name = VARIANT::default();
            let mut value = VARIANT::default();
            object.Get(w!("InstanceName"), 0, &mut name, None, None)?;
            object.Get(w!("CurrentBrightness"), 0, &mut value, None, None)?;
            let name = BSTR::try_from(&name)?.to_string();
            let percent = u8::try_from(u32::try_from(&value)?)?;
            ensure!(percent <= 100, "{}", tr("utilities.brightness_failed"));
            let escaped = name.replace('\\', r"\\").replace('"', r#"\""#);
            let path = format!("WmiMonitorBrightnessMethods.InstanceName=\"{escaped}\"");
            result.push(PhysicalMonitor {
                identity: format!("wmi:{path}"),
                device_id: wmi_device_id(&name),
                monitor: None,
                wmi_path: Some(BSTR::from(path)),
                min: 0,
                max: 100,
                snapshot: MonitorBrightness {
                    id: 0,
                    name,
                    percent: Some(percent),
                    error: None,
                },
            });
        }
        Ok(result)
    }
}
fn wmi_set(services: &IWbemServices, path: &BSTR, value: u8) -> Result<()> {
    unsafe {
        let mut class = None;
        services.GetObject(
            &BSTR::from("WmiMonitorBrightnessMethods"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            Some(&mut class),
            None,
        )?;
        let class = class.ok_or_else(|| anyhow::anyhow!(tr("utilities.brightness_failed")))?;
        let mut signature = None;
        class.GetMethod(
            w!("WmiSetBrightness"),
            0,
            &mut signature,
            std::ptr::null_mut(),
        )?;
        let input = signature
            .ok_or_else(|| anyhow::anyhow!(tr("utilities.brightness_failed")))?
            .SpawnInstance(0)?;
        input.Put(w!("Timeout"), 0, &VARIANT::from("0"), 0)?;
        input.Put(w!("Brightness"), 0, &VARIANT::from(value), 0)?;
        services.ExecMethod(
            path,
            &BSTR::from("WmiSetBrightness"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            &input,
            None,
            None,
        )?;
        Ok(())
    }
}
fn wide_text(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|v| *v == 0).unwrap_or(value.len())])
}
fn display_device_id(interface: &str) -> String {
    interface
        .trim_start_matches(r"\\?\")
        .split('#')
        .take(3)
        .collect::<Vec<_>>()
        .join("\\")
        .to_ascii_lowercase()
}
fn wmi_device_id(instance: &str) -> String {
    let instance = instance.to_ascii_lowercase();
    match instance.rsplit_once('_') {
        Some((device, suffix))
            if !suffix.is_empty() && suffix.bytes().all(|v| v.is_ascii_digit()) =>
        {
            device.to_owned()
        }
        _ => instance,
    }
}
fn inventory(ids: &mut BTreeMap<String, u64>, wmi: Option<&IWbemServices>) -> Vec<PhysicalMonitor> {
    let mut monitors = physical_monitors();
    if let Some(wmi) = wmi {
        match wmi_monitors(wmi) {
            Ok(internal) => {
                for panel in internal {
                    if let Some(index) = monitors
                        .iter()
                        .position(|m| !m.device_id.is_empty() && m.device_id == panel.device_id)
                    {
                        if monitors[index].snapshot.percent.is_none() {
                            monitors[index] = panel;
                        }
                    } else {
                        monitors.push(panel);
                    }
                }
            }
            Err(error) => tracing::warn!(%error,"WMI brightness inventory failed"),
        }
    }
    for monitor in &mut monitors {
        let next = ids.len() as u64 + 1;
        monitor.snapshot.id = *ids.entry(monitor.identity.clone()).or_insert(next);
    }
    monitors
}
fn brightness_worker(
    receiver: mpsc::Receiver<BrightnessCommand>,
    state: Arc<Mutex<Vec<MonitorBrightness>>>,
    mut debounce: Duration,
    notify: impl Fn(),
) {
    struct ComApartment(bool);
    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe {
                    CoUninitialize();
                }
            }
        }
    }
    let apartment = ComApartment(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok());
    let wmi = if apartment.0 {
        match wmi_services() {
            Ok(wmi) => Some(wmi),
            Err(error) => {
                tracing::warn!(%error,"WMI brightness unavailable");
                None
            }
        }
    } else {
        None
    };
    let mut ids = BTreeMap::new();
    let mut monitors = inventory(&mut ids, wmi.as_ref());
    let mut known: BTreeMap<u64, MonitorBrightness> = monitors
        .iter()
        .map(|m| (m.snapshot.id, m.snapshot.clone()))
        .collect();
    let mut disappeared = BTreeMap::<u64, MonitorBrightness>::new();
    let mut pending = BTreeMap::<u64, u8>::new();
    let mut deadline = None::<Instant>;
    *state.lock().unwrap() = monitors.iter().map(|m| m.snapshot.clone()).collect();
    notify();
    loop {
        let command = match deadline {
            Some(end) => receiver.recv_timeout(end.saturating_duration_since(Instant::now())),
            None => receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match command {
            Ok(BrightnessCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(BrightnessCommand::SetDebounce(value)) => {
                debounce = value;
                if deadline.is_some() {
                    deadline = Some(Instant::now() + debounce);
                }
            }
            Ok(BrightnessCommand::Refresh) => {
                monitors.clear();
                monitors = inventory(&mut ids, wmi.as_ref());
                pending.retain(|id, _| {
                    let present = monitors.iter().any(|m| m.snapshot.id == *id);
                    if !present {
                        record_disappeared(*id, &known, &mut disappeared);
                    }
                    present
                });
                if pending.is_empty() {
                    deadline = None;
                }
                disappeared.retain(|id, _| !monitors.iter().any(|m| m.snapshot.id == *id));
            }
            Ok(BrightnessCommand::Set(id, value)) => {
                if monitors.iter().any(|m| m.snapshot.id == id) {
                    pending.insert(id, value);
                    deadline = Some(Instant::now() + debounce);
                } else {
                    record_disappeared(id, &known, &mut disappeared);
                }
            }
            Ok(BrightnessCommand::Adjust(delta)) => {
                for m in &monitors {
                    if let Some(value) = pending.get(&m.snapshot.id).copied().or(m.snapshot.percent)
                    {
                        pending.insert(
                            m.snapshot.id,
                            (i32::from(value) + i32::from(delta)).clamp(0, 100) as u8,
                        );
                    }
                }
                deadline = Some(Instant::now() + debounce);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                for m in &mut monitors {
                    if let Some(value) = pending.remove(&m.snapshot.id) {
                        let success = if let (Some(path), Some(wmi)) = (&m.wmi_path, &wmi) {
                            wmi_set(wmi, path, value).is_ok()
                        } else if let Some(monitor) = m.monitor {
                            m.max > m.min
                                && unsafe {
                                    SetMonitorBrightness(
                                        monitor.hPhysicalMonitor,
                                        m.min
                                            + ((u64::from(m.max - m.min) * u64::from(value)) / 100)
                                                as u32,
                                    )
                                } != 0
                        } else {
                            false
                        };
                        if success {
                            m.snapshot.percent = Some(value);
                            m.snapshot.error = None;
                        } else {
                            m.snapshot.error = Some(tr("utilities.brightness_failed"));
                        }
                    }
                }
                pending.clear();
                deadline = None;
            }
        }
        for monitor in &monitors {
            known.insert(monitor.snapshot.id, monitor.snapshot.clone());
        }
        *state.lock().unwrap() = monitors
            .iter()
            .map(|m| m.snapshot.clone())
            .chain(disappeared.values().cloned())
            .collect();
        notify();
    }
}
fn record_disappeared(
    id: u64,
    known: &BTreeMap<u64, MonitorBrightness>,
    disappeared: &mut BTreeMap<u64, MonitorBrightness>,
) {
    if let Some(previous) = known.get(&id) {
        let mut snapshot = previous.clone();
        snapshot.percent = None;
        snapshot.error = Some(tr("utilities.monitor_missing"));
        disappeared.insert(id, snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primary_color_spaces() {
        let red = Rgb { r: 255, g: 0, b: 0 };
        assert_eq!(red.format(ColorFormat::Hex), "#FF0000");
        assert_eq!(red.hsl(), [0.0, 1.0, 0.5]);
        let [l, c, h] = red.oklch();
        assert!(
            (l - 0.627955).abs() < 0.00001
                && (c - 0.257683).abs() < 0.00001
                && (h - 29.2339).abs() < 0.001
        );
    }
    #[test]
    fn achromatic_colors_have_no_hue() {
        for value in [0, 128, 255] {
            let color = Rgb {
                r: value,
                g: value,
                b: value,
            };
            assert_eq!(color.hsl()[0], 0.0);
            assert_eq!(color.hsl()[1], 0.0);
            assert!(color.oklch()[1] < 1e-7);
            assert_eq!(color.oklch()[2], 0.0);
        }
    }

    #[test]
    fn color_template_preserves_unknown_tokens_and_formats_channels() {
        let color = Rgb {
            r: 15,
            g: 128,
            b: 255,
        };
        assert_eq!(color.format_template("#%ReX%GrX%BlX"), "#0F80FF");
        assert_eq!(
            color.format_template("%Rex %GrD %BlF %% %unknown"),
            "0f 128 1.000 % %unknown"
        );
        assert_eq!(color.format_template("%Re"), "%Re");
    }
}
