use crate::{CapturedImage, Point, Rect};
use crate::{
    arrow_comment::{ArrowComment, ArrowCommentEditor, EditResult},
    canvas_mark::{MarkKind, MarkStyle, MosaicMode, Scene, render_ui_text},
};
use anyhow::{Context, Result, anyhow, ensure};
use orbment_ui::D2dContext;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    mem::size_of,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::*,
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Dwm::*,
            Dxgi::{Common::*, *},
            Gdi::*,
        },
        System::{
            Com::*,
            LibraryLoader::GetModuleHandleW,
            Registry::*,
            WinRT::{
                Direct3D11::*, Graphics::Capture::IGraphicsCaptureItemInterop,
                RO_INIT_MULTITHREADED, RoGetActivationFactory, RoInitialize, RoUninitialize,
            },
        },
        UI::{
            Accessibility::*, Controls::WM_MOUSELEAVE, HiDpi::*, Input::KeyboardAndMouse::*,
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, HSTRING, Interface, PCWSTR, PWSTR, RuntimeName, w},
};

pub(crate) const FRAME_MESSAGE: u32 = WM_APP + 161;
const PROBE_MESSAGE: u32 = WM_APP + 162;

pub(crate) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn rectangle(r: RECT) -> Rect {
    Rect {
        x: r.left as f64,
        y: r.top as f64,
        width: (r.right - r.left) as f64,
        height: (r.bottom - r.top) as f64,
    }
}

#[derive(Clone, Debug)]
pub struct Monitor {
    pub handle: isize,
    pub bounds: Rect,
    pub dpi: u32,
    pub name: String,
    pub hdr: bool,
}

pub fn monitors() -> Result<Vec<Monitor>> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        unsafe {
            let list = &mut *(data.0 as *mut Vec<Monitor>);
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
                let mut x = 96;
                let mut y = 96;
                let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y);
                let name = String::from_utf16_lossy(
                    &info.szDevice[..info
                        .szDevice
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(info.szDevice.len())],
                );
                list.push(Monitor {
                    handle: monitor.0 as isize,
                    bounds: rectangle(info.monitorInfo.rcMonitor),
                    dpi: x,
                    name,
                    hdr: false,
                });
            }
            TRUE
        }
    }
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut list as *mut _ as isize),
        )
        .ok()?;
    }
    ensure!(!list.is_empty(), "No desktop monitors");
    for monitor in &mut list {
        monitor.hdr = monitor_hdr(monitor);
    }
    Ok(list)
}

fn monitor_hdr(monitor: &Monitor) -> bool {
    unsafe {
        if let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() {
            let mut ai = 0;
            while let Ok(adapter) = factory.EnumAdapters1(ai) {
                let mut oi = 0;
                while let Ok(output) = adapter.EnumOutputs(oi) {
                    if let Ok(desc) = output.GetDesc()
                        && desc.Monitor.0 as isize == monitor.handle
                        && let Ok(output6) = output.cast::<IDXGIOutput6>()
                        && let Ok(desc) = output6.GetDesc1()
                    {
                        return desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                    }
                    oi += 1;
                }
                ai += 1;
            }
        }
        // On older output interfaces consult only the matching monitor's persisted HDR switch.
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let name = wide(&monitor.name);
        if !EnumDisplayDevicesW(
            PCWSTR(name.as_ptr()),
            0,
            &mut device,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
        .as_bool()
        {
            return false;
        }
        let id = String::from_utf16_lossy(&device.DeviceID).to_ascii_lowercase();
        let model = id.split('#').nth(1).unwrap_or("");
        if model.is_empty() {
            return false;
        }
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SYSTEM\\CurrentControlSet\\Control\\GraphicsDrivers\\MonitorDataStore"),
            Some(0),
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return false;
        }
        let mut result = false;
        for index in 0..4096 {
            let mut text = [0u16; 512];
            let mut len = text.len() as u32;
            let status = RegEnumKeyExW(
                key,
                index,
                Some(PWSTR(text.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            );
            if status != ERROR_SUCCESS {
                break;
            }
            let sub = String::from_utf16_lossy(&text[..len as usize]);
            if sub.to_ascii_lowercase().contains(model) {
                let mut value = 0u32;
                let mut bytes = 4;
                if RegGetValueW(
                    key,
                    PCWSTR(text.as_ptr()),
                    w!("HDREnabled"),
                    RRF_RT_REG_DWORD,
                    None,
                    Some(&mut value as *mut _ as *mut _),
                    Some(&mut bytes),
                ) == ERROR_SUCCESS
                    && value != 0
                {
                    result = true;
                    break;
                }
            }
        }
        let _ = RegCloseKey(key);
        result
    }
}

pub fn virtual_desktop() -> Rect {
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
            y: GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64,
        }
    }
}
pub fn menu_active() -> bool {
    unsafe {
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(0, &mut info).is_ok() && info.flags.0 & 0x1c != 0
    }
}

#[derive(Clone, Debug)]
pub enum CaptureSource {
    Desktop(Rect),
    Window { hwnd: isize, roi: Option<Rect> },
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

struct WgcCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    include_cursor: bool,
    started: bool,
    winrt: IDirect3DDevice,
    event: i64,
    arrived: mpsc::Receiver<()>,
    staging: [Option<ID3D11Texture2D>; 3],
    timestamps: [i64; 3],
    slot: usize,
    filled: usize,
    size: [u32; 2],
    hdr: bool,
    last_frame: Option<(i64, CapturedImage)>,
}
impl WgcCapture {
    fn new(hwnd: Option<isize>, monitor: &Monitor, include_cursor: bool) -> Result<Self> {
        unsafe {
            // Generated static methods cache factories beyond this worker's MTA lifetime.
            // Activate locally so restarting capture cannot reuse a torn-down factory.
            let support: IGraphicsCaptureSessionStatics =
                RoGetActivationFactory(&HSTRING::from(GraphicsCaptureSession::NAME))?;
            let mut supported = false;
            (support.vtable().IsSupported)(support.as_raw(), &mut supported).ok()?;
            ensure!(supported, "Windows Graphics Capture unavailable");
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("D3D11 capture device missing")?;
            let context = context.context("D3D11 immediate context missing")?;
            let dxgi: IDXGIDevice = device.cast()?;
            let winrt: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            let interop: IGraphicsCaptureItemInterop =
                RoGetActivationFactory(&HSTRING::from(GraphicsCaptureItem::NAME))?;
            let item: GraphicsCaptureItem = if let Some(hwnd) = hwnd {
                interop.CreateForWindow(HWND(hwnd as *mut _))?
            } else {
                interop.CreateForMonitor(HMONITOR(monitor.handle as *mut _))?
            };
            let size = item.Size()?;
            let format = if monitor.hdr {
                DirectXPixelFormat::R16G16B16A16Float
            } else {
                DirectXPixelFormat::B8G8R8A8UIntNormalized
            };
            let factory: IDirect3D11CaptureFramePoolStatics2 =
                RoGetActivationFactory(&HSTRING::from(Direct3D11CaptureFramePool::NAME))?;
            let mut raw_pool = std::ptr::null_mut();
            (factory.vtable().CreateFreeThreaded)(
                factory.as_raw(),
                winrt.as_raw(),
                format,
                3,
                size,
                &mut raw_pool,
            )
            .ok()?;
            let pool: Direct3D11CaptureFramePool = windows::core::Type::from_abi(raw_pool)?;
            let (tx, arrived) = mpsc::sync_channel(1);
            let event = pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
                let _ = tx.try_send(());
                Ok(())
            }))?;
            let session = create_capture_session(&pool, &item, include_cursor)?;
            Ok(Self {
                device,
                context,
                pool,
                session,
                item,
                include_cursor,
                started: false,
                winrt,
                event,
                arrived,
                staging: [None, None, None],
                timestamps: [0; 3],
                slot: 0,
                filled: 0,
                size: [size.Width as u32, size.Height as u32],
                hdr: monitor.hdr,
                last_frame: None,
            })
        }
    }
    fn try_frame(&self) -> Result<Option<Direct3D11CaptureFrame>> {
        unsafe {
            let mut raw = std::ptr::null_mut();
            (self.pool.vtable().TryGetNextFrame)(self.pool.as_raw(), &mut raw).ok()?;
            // An empty frame pool returns S_OK with a null interface.
            Ok((!raw.is_null()).then(|| Direct3D11CaptureFrame::from_raw(raw)))
        }
    }
    fn next_fresh(&mut self) -> Result<(i64, CapturedImage)> {
        let restart = self.started;
        if restart {
            self.session.Close()?;
            self.started = false;
        }
        while self.arrived.try_recv().is_ok() {}
        drain_then_capture(
            self,
            |capture| capture.try_frame(),
            |frame| frame.Close().map_err(Into::into),
            |capture| {
                if restart {
                    capture.session = create_capture_session(
                        &capture.pool,
                        &capture.item,
                        capture.include_cursor,
                    )?;
                }
                capture.last_frame = None;
                capture.staging = [None, None, None];
                capture.filled = 0;
                capture.start()
            },
            |capture| capture.next(Duration::from_secs(3)),
        )
    }
    fn start(&mut self) -> Result<()> {
        if !self.started {
            self.session.StartCapture()?;
            self.started = true;
        }
        Ok(())
    }
    fn next(&mut self, timeout: Duration) -> Result<(i64, CapturedImage)> {
        self.start()?;
        let deadline = Instant::now() + timeout;
        let frame = loop {
            if let Some(frame) = self.try_frame()? {
                break frame;
            }
            match self
                .arrived
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Timeout) if self.last_frame.is_some() => {
                    let latest = (self.slot + 2) % 3;
                    if self.staging[latest].is_some()
                        && self.timestamps[latest] > self.last_frame.as_ref().unwrap().0
                    {
                        self.last_frame = Some(self.read_staging(latest, self.size)?);
                    }
                    return Ok(self.last_frame.as_ref().unwrap().clone());
                }
                Err(error) => return Err(error).context("WGC frame deadline elapsed"),
            }
        };
        let size = frame.ContentSize()?;
        let timestamp = frame.SystemRelativeTime()?.Duration;
        let result = (|| unsafe {
            ensure!(
                size.Width > 0 && size.Height > 0,
                "Capture source minimized or closed"
            );
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let source: ID3D11Texture2D = access.GetInterface()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            source.GetDesc(&mut desc);
            if self.size != [size.Width as u32, size.Height as u32]
                || desc.Width != self.size[0]
                || desc.Height != self.size[1]
            {
                self.staging = [None, None, None];
                self.filled = 0;
            }
            let slot = self.slot;
            self.slot = (slot + 1) % 3;
            if self.staging[slot].is_none() {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut self.staging[slot]))?;
            }
            self.context
                .CopyResource(self.staging[slot].as_ref().unwrap(), &source);
            self.timestamps[slot] = timestamp;
            self.filled = (self.filled + 1).min(3);
            // After warm-up read the oldest completed slot while the GPU fills the current one.
            let read_slot = (slot + 4 - self.filled) % 3;
            if let Some(previous) = &self.last_frame
                && self.timestamps[read_slot] <= previous.0
            {
                return Ok(previous.clone());
            }
            self.read_staging(read_slot, [size.Width as u32, size.Height as u32])
        })();
        let _ = frame.Close();
        // Recreate only after the frame/surface references have been returned to the pool.
        if size.Width > 0 && size.Height > 0 && self.size != [size.Width as u32, size.Height as u32]
        {
            let format = if self.hdr {
                DirectXPixelFormat::R16G16B16A16Float
            } else {
                DirectXPixelFormat::B8G8R8A8UIntNormalized
            };
            self.pool.Recreate(&self.winrt, format, 3, size)?;
            self.size = [size.Width as u32, size.Height as u32];
            self.staging = [None, None, None];
            self.filled = 0;
        }
        if let Ok(image) = &result {
            self.last_frame = Some(image.clone());
        }
        result
    }
    fn read_staging(&self, slot: usize, content_size: [u32; 2]) -> Result<(i64, CapturedImage)> {
        unsafe {
            let target = self.staging[slot]
                .as_ref()
                .context("Capture staging slot is empty")?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            target.GetDesc(&mut desc);
            let width = desc.Width.min(content_size[0]);
            let height = desc.Height.min(content_size[1]);
            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            map_staging_read(|flags| {
                self.context
                    .Map(target, 0, D3D11_MAP_READ, flags, Some(&mut mapped))
            })?;
            let row_bytes = width as usize * if self.hdr { 8 } else { 4 };
            if mapped.pData.is_null() || (mapped.RowPitch as usize) < row_bytes {
                self.context.Unmap(target, 0);
                return Err(anyhow!("Invalid capture staging buffer or row pitch"));
            }
            for y in 0..height as usize {
                let row = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                let dst = &mut pixels[y * width as usize * 4..(y + 1) * width as usize * 4];
                if self.hdr {
                    for (x, pixel) in dst.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        let channels = std::slice::from_raw_parts(row.add(x * 8), 8);
                        let linear = |i: usize| {
                            half_to_float(u16::from_le_bytes([
                                channels[i * 2],
                                channels[i * 2 + 1],
                            ]))
                        };
                        let mapped = tone_map([linear(0), linear(1), linear(2)]);
                        pixel.copy_from_slice(&[mapped[2], mapped[1], mapped[0], 255]);
                    }
                } else {
                    dst.copy_from_slice(std::slice::from_raw_parts(row, dst.len()));
                    for p in dst.as_chunks_mut::<4>().0 {
                        p[3] = 255;
                    }
                }
            }
            self.context.Unmap(target, 0);
            Ok((
                self.timestamps[slot],
                CapturedImage {
                    width,
                    height,
                    pixels,
                },
            ))
        }
    }
}
impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.event);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn create_capture_session(
    pool: &Direct3D11CaptureFramePool,
    item: &GraphicsCaptureItem,
    include_cursor: bool,
) -> Result<GraphicsCaptureSession> {
    let session = pool.CreateCaptureSession(item)?;
    if session.cast::<IGraphicsCaptureSession3>().is_ok() {
        let _ = session.SetIsBorderRequired(false);
    }
    session.SetIsCursorCaptureEnabled(include_cursor)?;
    Ok(session)
}

fn drain_then_capture<State, Frame, Output>(
    state: &mut State,
    mut take: impl FnMut(&mut State) -> Result<Option<Frame>>,
    mut release: impl FnMut(Frame) -> Result<()>,
    start: impl FnOnce(&mut State) -> Result<()>,
    capture: impl FnOnce(&mut State) -> Result<Output>,
) -> Result<Output> {
    while let Some(frame) = take(state)? {
        release(frame)?;
    }
    start(state)?;
    capture(state)
}

fn map_staging_read(
    mut map: impl FnMut(u32) -> windows::core::Result<()>,
) -> windows::core::Result<()> {
    match map(D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32) {
        Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => map(0),
        result => result,
    }
}

fn half_to_float(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (bits >> 10) & 31;
    let fraction = (bits & 1023) as f32;
    match exp {
        0 => sign * fraction * 2.0f32.powi(-24),
        31 => {
            if fraction == 0.0 {
                sign * 65504.0
            } else {
                0.0
            }
        }
        _ => sign * (1.0 + fraction / 1024.0) * 2.0f32.powi(exp as i32 - 15),
    }
}
fn tone_map(rgb: [f32; 3]) -> [u8; 3] {
    let luminance = (rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722).max(0.0);
    let factor = 1.0 / (1.0 + luminance / 4.0);
    rgb.map(|v| {
        let v = (v.max(0.0) * factor).min(1.0);
        let s = if v <= 0.0031308 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0).round() as u8
    })
}

pub(crate) struct Dib {
    pub dc: HDC,
    pub bitmap: HBITMAP,
    old: HGDIOBJ,
    pub bits: *mut u8,
    pub width: u32,
    pub height: u32,
}
impl Dib {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            ensure!(!dc.is_invalid(), "CreateCompatibleDC failed");
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(v) => v,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e.into());
                }
            };
            let old = SelectObject(dc, HGDIOBJ(bitmap.0));
            Ok(Self {
                dc,
                bitmap,
                old,
                bits: bits.cast(),
                width,
                height,
            })
        }
    }
    pub fn image(&self) -> CapturedImage {
        CapturedImage {
            width: self.width,
            height: self.height,
            pixels: unsafe {
                std::slice::from_raw_parts(
                    self.bits,
                    self.width as usize * self.height as usize * 4,
                )
            }
            .to_vec(),
        }
    }
}
impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn qpc_100ns() -> Result<i64> {
    let mut ticks = 0;
    let mut frequency = 0;
    unsafe {
        windows::Win32::System::Performance::QueryPerformanceCounter(&mut ticks)?;
        windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency)?;
    }
    ensure!(frequency > 0, "Invalid QPC frequency");
    Ok((ticks as i128 * 10_000_000 / frequency as i128) as i64)
}
fn bitblt_timed(rect: Rect) -> Result<(i64, CapturedImage)> {
    rect.validate()?;
    let dib = Dib::new(rect.width as u32, rect.height as u32)?;
    let start = qpc_100ns()?;
    unsafe {
        let screen = GetDC(None);
        ensure!(!screen.is_invalid(), "Desktop DC unavailable");
        let result = BitBlt(
            dib.dc,
            0,
            0,
            dib.width as i32,
            dib.height as i32,
            Some(screen),
            rect.x as i32,
            rect.y as i32,
            SRCCOPY | CAPTUREBLT,
        );
        ReleaseDC(None, screen);
        result?;
    }
    let timestamp = start + (qpc_100ns()? - start) / 2;
    let mut image = dib.image();
    for p in image.pixels.as_chunks_mut::<4>().0 {
        p[3] = 255;
    }
    Ok((timestamp, image))
}

struct CapturePipeline {
    source: CaptureSource,
    entries: Vec<(Monitor, Option<WgcCapture>)>,
}
impl CapturePipeline {
    fn new(source: CaptureSource, include_cursor: bool) -> Result<Self> {
        let all = monitors()?;
        let selected = match &source {
            CaptureSource::Desktop(rect) => {
                rect.validate()?;
                all.into_iter()
                    .filter(|m| m.bounds.intersection(*rect).is_some())
                    .collect::<Vec<_>>()
            }
            CaptureSource::Window { hwnd, .. } => {
                let monitor =
                    unsafe { MonitorFromWindow(HWND(*hwnd as *mut _), MONITOR_DEFAULTTONEAREST) };
                all.into_iter()
                    .filter(|m| m.handle == monitor.0 as isize)
                    .collect()
            }
        };
        ensure!(
            !selected.is_empty(),
            "Capture region is outside the desktop"
        );
        let hwnd = match source {
            CaptureSource::Window { hwnd, .. } => Some(hwnd),
            _ => None,
        };
        let entries = selected
            .into_iter()
            .map(|m| {
                let capture = match WgcCapture::new(hwnd, &m, include_cursor) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::debug!(error=%e,"WGC unavailable; using desktop BitBlt");
                        None
                    }
                };
                (m, capture)
            })
            .collect();
        Ok(Self { source, entries })
    }
    fn next(&mut self) -> Result<CapturedImage> {
        self.next_frame(true).map(|(_, image)| image)
    }
    fn next_timed(&mut self) -> Result<(i64, CapturedImage)> {
        self.next_frame(false)
    }
    fn next_frame(&mut self, fresh: bool) -> Result<(i64, CapturedImage)> {
        let bounds = match self.source {
            CaptureSource::Desktop(rect) => rect,
            CaptureSource::Window { hwnd, .. } => {
                let mut r = RECT::default();
                unsafe {
                    GetWindowRect(HWND(hwnd as *mut _), &mut r)?;
                }
                rectangle(r)
            }
        };
        bounds.validate()?;
        let mut output = CapturedImage {
            width: bounds.width as u32,
            height: bounds.height as u32,
            pixels: vec![0; bounds.width as usize * bounds.height as usize * 4],
        };
        let mut timestamp = 0;
        for (monitor, capture) in &mut self.entries {
            let is_window = matches!(self.source, CaptureSource::Window { .. });
            let source_bounds = if is_window { bounds } else { monitor.bounds };
            let (captured_at, frame) = if let Some(wgc) = capture {
                let frame = if fresh {
                    wgc.next_fresh()
                } else {
                    let timeout = if wgc.last_frame.is_some() {
                        Duration::ZERO
                    } else {
                        Duration::from_millis(750)
                    };
                    wgc.next(timeout)
                };
                match frame {
                    Ok(frame) => frame,
                    Err(e) => {
                        tracing::debug!(error=%e,"WGC readback failed; using BitBlt");
                        *capture = None;
                        bitblt_timed(source_bounds)?
                    }
                }
            } else {
                bitblt_timed(source_bounds)?
            };
            // A composed desktop frame is dated by its newest contributing monitor.
            timestamp = timestamp.max(captured_at);
            if is_window {
                let image = match self.source {
                    CaptureSource::Window { roi: Some(roi), .. } => frame.crop(roi)?,
                    _ => frame,
                };
                return Ok((captured_at, image));
            }
            if let Some(region) = source_bounds.intersection(bounds) {
                let x = (region.x - source_bounds.x) as usize;
                let y = (region.y - source_bounds.y) as usize;
                for row in 0..region.height as usize {
                    let src = ((y + row) * frame.width as usize + x) * 4;
                    let dst = (((region.y - bounds.y) as usize + row) * output.width as usize
                        + (region.x - bounds.x) as usize)
                        * 4;
                    let count = region.width as usize * 4;
                    if src + count <= frame.pixels.len() {
                        output.pixels[dst..dst + count]
                            .copy_from_slice(&frame.pixels[src..src + count]);
                    }
                }
            }
        }
        Ok((timestamp, output))
    }
}

pub fn capture_rect(rect: Rect) -> Result<CapturedImage> {
    rect.validate()?;
    thread::Builder::new()
        .name("orbment-capture".into())
        .spawn(move || {
            let _apartment = Apartment::new()?;
            CapturePipeline::new(CaptureSource::Desktop(rect), false)?.next()
        })?
        .join()
        .map_err(|_| anyhow!("Capture worker panicked"))?
}

/// The worker owns its capture device. Only CPU bytes cross to the UI device.
pub struct CaptureStream {
    frame: Arc<Mutex<Option<(i64, CapturedImage)>>>,
    changed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
    wake: Arc<Mutex<Option<isize>>>,
}
impl CaptureStream {
    pub fn start(source: CaptureSource) -> Result<Self> {
        Self::start_with_fps(source, 30)
    }
    pub fn start_with_fps(source: CaptureSource, fps: u32) -> Result<Self> {
        Self::start_with_options(source, fps, false)
    }
    pub fn start_with_options(
        source: CaptureSource,
        fps: u32,
        include_cursor: bool,
    ) -> Result<Self> {
        ensure!(
            (1..=240).contains(&fps),
            "Capture frame rate must be 1–240 FPS"
        );
        let interval = Duration::from_secs_f64(1.0 / fps as f64);
        let frame = Arc::new(Mutex::new(None));
        let changed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let wake = Arc::new(Mutex::new(None));
        let (f, c, s, e, w) = (
            frame.clone(),
            changed.clone(),
            stop.clone(),
            error.clone(),
            wake.clone(),
        );
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("orbment-window-capture".into())
            .spawn(move || {
                let setup = (|| {
                    let apartment = Apartment::new()?;
                    let pipeline = CapturePipeline::new(source, include_cursor)?;
                    Ok::<_, anyhow::Error>((apartment, pipeline))
                })();
                let (_apartment, mut pipeline) = match setup {
                    Ok(v) => {
                        let _ = ready_tx.send(Ok(()));
                        v
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err.to_string()));
                        return;
                    }
                };
                while !s.load(Ordering::Acquire) {
                    let start = Instant::now();
                    match pipeline.next_timed() {
                        Ok(image) => {
                            *f.lock().unwrap() = Some(image);
                            c.store(true, Ordering::Release);
                        }
                        Err(err) => {
                            *e.lock().unwrap() = Some(err.to_string());
                            if let Some(hwnd) = *w.lock().unwrap() {
                                unsafe {
                                    let _ = PostMessageW(
                                        Some(HWND(hwnd as *mut _)),
                                        FRAME_MESSAGE,
                                        WPARAM(0),
                                        LPARAM(0),
                                    );
                                }
                            }
                            break;
                        }
                    }
                    if let Some(hwnd) = *w.lock().unwrap() {
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(hwnd as *mut _)),
                                FRAME_MESSAGE,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    }
                    if let Some(delay) = interval.checked_sub(start.elapsed()) {
                        thread::sleep(delay);
                    }
                }
            })?;
        ready_rx
            .recv()
            .context("Capture worker disconnected")?
            .map_err(|e| anyhow!(e))?;
        Ok(Self {
            frame,
            changed,
            stop,
            worker: Some(worker),
            error,
            wake,
        })
    }
    pub fn latest(&self) -> Option<CapturedImage> {
        self.latest_timed().map(|(_, image)| image)
    }
    /// Absolute source QPC time in 100ns units, retained through readback and worker delivery.
    pub fn latest_timed(&self) -> Option<(i64, CapturedImage)> {
        if self.changed.swap(false, Ordering::AcqRel) {
            self.frame.lock().unwrap().take()
        } else {
            None
        }
    }
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }
    pub fn set_wake_window(&self, hwnd: HWND) {
        *self.wake.lock().unwrap() = Some(hwnd.0 as isize);
        unsafe {
            let _ = PostMessageW(Some(hwnd), FRAME_MESSAGE, WPARAM(0), LPARAM(0));
        }
    }
}
impl Drop for CaptureStream {
    fn drop(&mut self) {
        *self.wake.lock().unwrap() = None;
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) struct PresentTarget {
    swap: IDXGISwapChain1,
    graphics: Rc<D2dContext>,
    width: u32,
    height: u32,
}
impl PresentTarget {
    pub fn new(graphics: Rc<D2dContext>, hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let dxgi: IDXGIDevice = graphics.d3d_device().cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let swap = factory.CreateSwapChainForHwnd(
                graphics.d3d_device(),
                hwnd,
                &DXGI_SWAP_CHAIN_DESC1 {
                    Width: width,
                    Height: height,
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
            Ok(Self {
                swap,
                graphics,
                width,
                height,
            })
        }
    }
    pub fn draw(&mut self, image: &CapturedImage) -> Result<()> {
        unsafe {
            if [self.width, self.height] != [image.width, image.height] {
                self.swap.ResizeBuffers(
                    0,
                    image.width,
                    image.height,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )?;
                self.width = image.width;
                self.height = image.height;
            }
            let texture: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            self.graphics.immediate_context().UpdateSubresource(
                &texture,
                0,
                None,
                image.pixels.as_ptr().cast(),
                image.width * 4,
                0,
            );
            self.swap.Present(0, DXGI_PRESENT(0)).ok()?;
            Ok(())
        }
    }
}

pub fn capture_source(source: CaptureSource) -> Result<CapturedImage> {
    thread::Builder::new()
        .name("orbment-capture".into())
        .spawn(move || {
            let _apartment = Apartment::new()?;
            CapturePipeline::new(source, false)?.next()
        })?
        .join()
        .map_err(|_| anyhow!("Capture worker panicked"))?
}

#[derive(Clone, Debug)]
pub struct WindowSnapshot {
    pub hwnd: isize,
    pub bounds: Rect,
}

pub fn window_snapshot(ignore: &[isize]) -> Result<Vec<WindowSnapshot>> {
    struct Context<'a> {
        ignore: &'a [isize],
        windows: Vec<WindowSnapshot>,
        desktop: Rect,
    }
    unsafe extern "system" fn collect(hwnd: HWND, param: LPARAM) -> BOOL {
        unsafe {
            let context = &mut *(param.0 as *mut Context<'_>);
            let mut class = [0u16; 128];
            let length = GetClassNameW(hwnd, &mut class).max(0) as usize;
            let class = String::from_utf16_lossy(&class[..length]);
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if context.ignore.contains(&(hwnd.0 as isize))
                || class.starts_with("OrbmentCapture")
                || class.starts_with("OrbmentPin")
                || !IsWindowVisible(hwnd).as_bool()
                || IsIconic(hwnd).as_bool()
                || style & (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0)
                    == WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0
            {
                return TRUE;
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4)
                .is_ok()
                && cloaked != 0
            {
                return TRUE;
            }
            let mut rect = RECT::default();
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rect as *mut _ as *mut _,
                size_of::<RECT>() as u32,
            )
            .is_err()
                && GetWindowRect(hwnd, &mut rect).is_err()
            {
                return TRUE;
            }
            if let Some(bounds) = rectangle(rect).intersection(context.desktop) {
                context.windows.push(WindowSnapshot {
                    hwnd: hwnd.0 as isize,
                    bounds,
                });
            }
            TRUE
        }
    }
    let mut context = Context {
        ignore,
        windows: Vec::new(),
        desktop: virtual_desktop(),
    };
    unsafe {
        EnumWindows(Some(collect), LPARAM(&mut context as *mut _ as isize))?;
    }
    Ok(context.windows)
}

fn contains_rect(outer: Rect, inner: Rect) -> bool {
    inner.width > 0.
        && inner.height > 0.
        && outer.x <= inner.x
        && outer.y <= inner.y
        && outer.x + outer.width >= inner.x + inner.width
        && outer.y + outer.height >= inner.y + inner.height
}

fn probe_window_alive(hwnd: HWND, saved: Rect) -> bool {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return false;
        }
        let mut current = RECT::default();
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut current as *mut _ as *mut _,
            size_of::<RECT>() as u32,
        )
        .is_ok()
            && contains_rect(rectangle(current), saved)
    }
}

fn hwnd_point_hierarchy(root: HWND, point: Point, clip: Rect) -> Vec<Rect> {
    unsafe {
        let mut current = root;
        let mut visited = HashSet::new();
        while visited.len() < 128 && visited.insert(current.0 as isize) {
            let mut client = POINT {
                x: point.x as i32,
                y: point.y as i32,
            };
            if !ScreenToClient(current, &mut client).as_bool() {
                break;
            }
            let child = ChildWindowFromPointEx(
                current,
                client,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            );
            if child.is_invalid() || child == current || visited.contains(&(child.0 as isize)) {
                break;
            }
            current = child;
        }
        let mut rectangles = Vec::new();
        visited.clear();
        while visited.len() < 128 && visited.insert(current.0 as isize) {
            let mut rect = RECT::default();
            if GetWindowRect(current, &mut rect).is_ok()
                && let Some(rect) = rectangle(rect).intersection(clip)
                && rect.contains(point)
            {
                rectangles.push(rect);
            }
            if current == root {
                break;
            }
            let Ok(parent) = GetParent(current) else {
                break;
            };
            current = parent;
        }
        rectangles.retain(|rect| *rect != clip);
        rectangles.push(clip);
        rectangles.dedup();
        rectangles
    }
}

#[derive(Default)]
struct ProbeHealth {
    degraded: HashSet<isize>,
}
impl ProbeHealth {
    fn available(&self, hwnd: isize) -> bool {
        !self.degraded.contains(&hwnd)
    }
    fn record(&mut self, hwnd: isize, elapsed: Duration, succeeded: bool) -> bool {
        if !succeeded || elapsed > Duration::from_millis(300) {
            self.degraded.insert(hwnd);
        }
        self.available(hwnd)
    }
}

struct CachedNode<Element = IUIAutomationElement> {
    element: Element,
    bounds: Rect,
    parent: Option<usize>,
    structural: bool,
    children: Option<Vec<usize>>,
}

struct WindowTree<Element = IUIAutomationElement> {
    nodes: Vec<CachedNode<Element>>,
}

impl<Element> WindowTree<Element> {
    fn query(
        &mut self,
        point: Point,
        mut load_children: impl FnMut(&mut Self, usize) -> bool,
    ) -> Vec<Rect> {
        const MAX_STEPS: usize = 80;
        let mut current = 0;
        for _ in 0..MAX_STEPS {
            if !load_children(self, current) {
                break;
            }
            if let Some(child) = self.hit_before(current, point, usize::MAX) {
                current = child;
            } else if let Some(alternative) = self.structural_alternative(current, point) {
                current = alternative;
            } else {
                break;
            }
        }
        self.path(current)
    }

    fn hit_before(&self, parent: usize, point: Point, before: usize) -> Option<usize> {
        self.nodes[parent]
            .children
            .as_ref()?
            .iter()
            .rev()
            .copied()
            .find(|&node| node < before && self.nodes[node].bounds.contains(point))
    }

    // Adapted from Snow Apps / snow-ui-selector (Copyright (C) 2026 mg-chao,
    // Apache-2.0, revision e7ba29839163). Modified for Orbment's bounds and child
    // lookup; see THIRD-PARTY-NOTICES.txt for the upstream file and license.
    fn structural_alternative(&self, mut current: usize, point: Point) -> Option<usize> {
        while let Some(parent) = self.nodes[current].parent {
            // Only a full-size layout layer may expose an earlier sibling branch.
            if !self.nodes[current].structural
                || self.nodes[current].bounds != self.nodes[parent].bounds
            {
                return None;
            }
            if let Some(sibling) = self.hit_before(parent, point, current) {
                return Some(sibling);
            }
            current = parent;
        }
        None
    }

    fn path(&self, mut current: usize) -> Vec<Rect> {
        let bounds = self.nodes[0].bounds;
        let mut path = Vec::new();
        loop {
            let node = &self.nodes[current];
            if node.bounds != bounds
                && node.bounds.width >= 16.
                && node.bounds.height >= 16.
                && path.last() != Some(&node.bounds)
            {
                path.push(node.bounds);
            }
            let Some(parent) = node.parent else {
                break;
            };
            current = parent;
        }
        path.push(bounds);
        path
    }
}

impl WindowTree {
    fn load_children(
        &mut self,
        index: usize,
        request: &IUIAutomationCacheRequest,
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
    ) -> bool {
        if Instant::now() >= deadline || cancelled() {
            return false;
        }
        if self.nodes[index].children.is_some() {
            return true;
        }
        let bounds = self.nodes[0].bounds;
        let element = self.nodes[index].element.clone();
        let mut children = Vec::new();
        let first_new = self.nodes.len();
        unsafe {
            // A timed-out or cancelled batch must never poison the cached tree.
            let updated = if index == 0 {
                element
            } else {
                let Ok(updated) = element.BuildUpdatedCache(request) else {
                    return false;
                };
                updated
            };
            let mut raw = std::ptr::null_mut();
            // S_OK + null is a normal UIA leaf, not a failed provider call.
            if (updated.vtable().GetCachedChildren)(updated.as_raw(), &mut raw).is_err() {
                return false;
            }
            if !raw.is_null() {
                let batch = IUIAutomationElementArray::from_raw(raw);
                let Ok(count) = batch.Length() else {
                    return false;
                };
                for i in 0..count {
                    if Instant::now() >= deadline || cancelled() || self.nodes.len() >= 32768 {
                        self.nodes.truncate(first_new);
                        return false;
                    }
                    let Ok(child) = batch.GetElement(i) else {
                        continue;
                    };
                    if !child.CachedIsOffscreen().is_ok_and(|v| !v.as_bool()) {
                        continue;
                    }
                    let Ok(rect) = child.CachedBoundingRectangle() else {
                        continue;
                    };
                    let Some(rect) = rectangle(rect).intersection(bounds) else {
                        continue;
                    };
                    let structural = child.CachedControlType().is_ok_and(|kind| {
                        kind == UIA_PaneControlTypeId || kind == UIA_GroupControlTypeId
                    });
                    children.push(self.nodes.len());
                    self.nodes.push(CachedNode {
                        element: child,
                        bounds: rect,
                        parent: Some(index),
                        structural,
                        children: None,
                    });
                }
            }
        }
        self.nodes[index].children = Some(children);
        true
    }
}

struct ProbeRequest {
    hwnd: isize,
    point: Point,
    bounds: Rect,
    generation: u64,
    wake: isize,
}
struct ProbeResult {
    hwnd: isize,
    point: Point,
    generation: u64,
    valid: bool,
    rectangles: Vec<Rect>,
}
struct UiaProbe {
    request: Arc<(Mutex<Option<ProbeRequest>>, std::sync::Condvar)>,
    stop: Arc<AtomicBool>,
    results: mpsc::Receiver<ProbeResult>,
}
impl UiaProbe {
    fn new() -> Result<Self> {
        let request = Arc::new((Mutex::new(None::<ProbeRequest>), std::sync::Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, results) = mpsc::channel();
        let pending = request.clone();
        let quitting = stop.clone();
        thread::Builder::new()
            .name("orbment-uia-probe".into())
            .spawn(move || {
                unsafe {
                    let _ =
                        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                }
                let apartment = Apartment::new().ok();
                let backend =
                    apartment.as_ref().and_then(|_| unsafe {
                        (|| -> windows::core::Result<(IUIAutomation2, IUIAutomationCacheRequest)> {
                        let automation: IUIAutomation2 =
                            CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
                        automation.SetConnectionTimeout(120)?;
                        automation.SetTransactionTimeout(120)?;
                        let cache = automation.CreateCacheRequest()?;
                        cache.SetTreeScope(TreeScope(TreeScope_Element.0 | TreeScope_Children.0))?;
                        cache.SetTreeFilter(&automation.ControlViewCondition()?)?;
                        cache.SetAutomationElementMode(AutomationElementMode_Full)?;
                        cache.AddProperty(UIA_BoundingRectanglePropertyId)?;
                        cache.AddProperty(UIA_IsOffscreenPropertyId)?;
                        cache.AddProperty(UIA_ControlTypePropertyId)?;
                        Ok((automation, cache))
                    })().ok()
                    });
                let mut health = ProbeHealth::default();
                let mut trees = HashMap::<isize, WindowTree>::new();
                loop {
                    let req = {
                        let mut guard = pending.0.lock().unwrap();
                        while guard.is_none() && !quitting.load(Ordering::Acquire) {
                            guard = pending.1.wait(guard).unwrap();
                        }
                        if quitting.load(Ordering::Acquire) {
                            break;
                        }
                        guard.take().unwrap()
                    };
                    let hwnd = HWND(req.hwnd as *mut _);
                    let mut valid = probe_window_alive(hwnd, req.bounds);
                    let mut rectangles = Vec::new();
                    if !valid {
                        trees.remove(&req.hwnd);
                    } else if let Some((automation, cache_request)) = &backend
                        && health.available(req.hwnd)
                    {
                        let started = Instant::now();
                        if let std::collections::hash_map::Entry::Vacant(entry) =
                            trees.entry(req.hwnd)
                        {
                            // Provider calls stay on this worker, including liveness probes.
                            let responsive = unsafe {
                                !IsHungAppWindow(hwnd).as_bool()
                                    && SendMessageTimeoutW(
                                        hwnd,
                                        WM_NULL,
                                        WPARAM(0),
                                        LPARAM(0),
                                        SMTO_ABORTIFHUNG | SMTO_BLOCK,
                                        50,
                                        None,
                                    )
                                    .0 != 0
                            };
                            let element = responsive
                                .then(|| unsafe {
                                    automation.ElementFromHandleBuildCache(hwnd, cache_request)
                                })
                                .and_then(|result| result.ok());
                            if let Some(element) = element {
                                entry.insert(WindowTree {
                                    nodes: vec![CachedNode {
                                        element,
                                        bounds: req.bounds,
                                        parent: None,
                                        structural: false,
                                        children: None,
                                    }],
                                });
                            }
                        }
                        if let Some(tree) = trees.get_mut(&req.hwnd) {
                            let deadline = Instant::now() + Duration::from_millis(180);
                            rectangles = tree.query(req.point, |tree, current| {
                                tree.load_children(current, cache_request, deadline, &|| {
                                    quitting.load(Ordering::Acquire)
                                        || pending.0.lock().unwrap().is_some()
                                })
                            });
                        }
                        if !health.record(
                            req.hwnd,
                            started.elapsed(),
                            trees.contains_key(&req.hwnd),
                        ) {
                            trees.remove(&req.hwnd);
                            rectangles.clear();
                        }
                    }
                    // A provider can move or destroy its window while answering a query.
                    valid = valid && probe_window_alive(hwnd, req.bounds);
                    if valid {
                        if backend.is_none() || !health.available(req.hwnd) || rectangles.len() <= 1
                        {
                            rectangles = hwnd_point_hierarchy(hwnd, req.point, req.bounds);
                        }
                    } else {
                        trees.remove(&req.hwnd);
                        rectangles.clear();
                    }
                    if quitting.load(Ordering::Acquire) {
                        break;
                    }
                    if pending.0.lock().unwrap().is_some() {
                        continue;
                    }
                    let _ = tx.send(ProbeResult {
                        hwnd: req.hwnd,
                        point: req.point,
                        generation: req.generation,
                        valid,
                        rectangles,
                    });
                    unsafe {
                        let _ = PostMessageW(
                            Some(HWND(req.wake as *mut _)),
                            PROBE_MESSAGE,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                }
            })?;
        Ok(Self {
            request,
            stop,
            results,
        })
    }
    fn submit(&self, request: ProbeRequest) {
        *self.request.0.lock().unwrap() = Some(request);
        self.request.1.notify_one();
    }
}
// Never join a provider blocked inside third-party UIA code on the UI thread.
impl Drop for UiaProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.request.1.notify_one();
    }
}

#[derive(Clone, Debug, Default)]
pub enum SelectionShape {
    #[default]
    Rectangle,
    Rounded {
        radius: f64,
    },
    Polygon(Vec<Point>),
    Ratio {
        width: f64,
        height: f64,
    },
    Size {
        width: u32,
        height: u32,
    },
}
impl SelectionShape {
    fn constrain(&self, origin: Point, point: Point) -> Rect {
        let mut rect = Rect::from_points(origin, point);
        match self {
            Self::Ratio { width, height } if *width > 0.0 && *height > 0.0 => {
                rect.height = rect.width * height / width;
                if point.y < origin.y {
                    rect.y = origin.y - rect.height;
                }
            }
            Self::Size { width, height } => {
                rect.width = *width as f64;
                rect.height = *height as f64;
            }
            _ => {}
        }
        rect
    }
    fn includes(&self, point: Point, rect: Rect) -> bool {
        if !rect.contains(point) {
            return false;
        }
        match self {
            Self::Rounded { radius } => {
                let r = radius.max(0.0).min(rect.width / 2.0).min(rect.height / 2.0);
                let cx = point.x.clamp(rect.x + r, rect.x + rect.width - r);
                let cy = point.y.clamp(rect.y + r, rect.y + rect.height - r);
                (point.x - cx).powi(2) + (point.y - cy).powi(2) <= r * r
            }
            Self::Polygon(points) if points.len() >= 3 => {
                let mut inside = false;
                let mut previous = points[points.len() - 1];
                for current in points {
                    if (current.y > point.y) != (previous.y > point.y)
                        && point.x
                            < (previous.x - current.x) * (point.y - current.y)
                                / (previous.y - current.y)
                                + current.x
                    {
                        inside = !inside;
                    }
                    previous = *current;
                }
                inside
            }
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureAction {
    Confirm,
    Copy,
    Pin,
    Save,
    QuickSave,
    Longshot,
    Record,
    Ocr,
    QuickAsk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolbarIcon {
    Rectangle,
    Rounded,
    Circle,
    Pen,
    Arrow,
    Text,
    Serial,
    Mosaic,
    Eraser,
    Undo,
    Longshot,
    Record,
    Ocr,
    QuickAsk,
    Pin,
    Save,
    Cancel,
    Copy,
    Fill,
}
impl ToolbarIcon {
    fn annotation(self) -> bool {
        matches!(
            self,
            Self::Rectangle
                | Self::Rounded
                | Self::Circle
                | Self::Pen
                | Self::Arrow
                | Self::Text
                | Self::Serial
                | Self::Mosaic
                | Self::Eraser
        )
    }
    fn action(self) -> Option<CaptureAction> {
        Some(match self {
            Self::Copy => CaptureAction::Copy,
            Self::Pin => CaptureAction::Pin,
            Self::Save => CaptureAction::Save,
            Self::Longshot => CaptureAction::Longshot,
            Self::Record => CaptureAction::Record,
            Self::Ocr => CaptureAction::Ocr,
            Self::QuickAsk => CaptureAction::QuickAsk,
            _ => return None,
        })
    }
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Rectangle => "矩形标注",
            Self::Rounded => "圆角矩形",
            Self::Circle => "椭圆",
            Self::Pen => "画笔",
            Self::Arrow => "箭头",
            Self::Text => "文字",
            Self::Serial => "序号",
            Self::Mosaic => "马赛克",
            Self::Eraser => "橡皮擦",
            Self::Undo => "撤销 (Ctrl+Z)",
            Self::Longshot => "长截图 (L)",
            Self::Record => "区域录屏",
            Self::Ocr => "文字识别",
            Self::QuickAsk => "AI 提问",
            Self::Pin => "贴图到桌面 (F2)",
            Self::Save => "保存图片 (Ctrl+S)",
            Self::Cancel => "关闭 (Esc)",
            Self::Copy => "复制并完成 (Enter)",
            Self::Fill => "填充",
        }
    }
}
const CAPTURE_TOOLS: [ToolbarIcon; 17] = [
    ToolbarIcon::Rectangle,
    ToolbarIcon::Circle,
    ToolbarIcon::Pen,
    ToolbarIcon::Arrow,
    ToolbarIcon::Text,
    ToolbarIcon::Serial,
    ToolbarIcon::Mosaic,
    ToolbarIcon::Eraser,
    ToolbarIcon::Undo,
    ToolbarIcon::Longshot,
    ToolbarIcon::Record,
    ToolbarIcon::Ocr,
    ToolbarIcon::QuickAsk,
    ToolbarIcon::Pin,
    ToolbarIcon::Save,
    ToolbarIcon::Cancel,
    ToolbarIcon::Copy,
];
const COLORS: [u32; 7] = [
    0xff1677ff, 0xfff5222d, 0xfffaad14, 0xff52c41a, 0xff13c2c2, 0xff262626, 0xffffffff,
];
const WIDTHS: [f64; 5] = [2.0, 4.0, 8.0, 12.0, 18.0];
const COLOR_FORMATS: [&str; 9] = [
    "%HEX", "%RGB", "%HSL", "%HSB", "%CMYK", "%Oklab", "%Oklch", "%Lab", "%BGR",
];

pub(crate) fn ui_rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

pub(crate) struct UiCanvas(tiny_skia::Pixmap, tiny_skia::Transform);
impl UiCanvas {
    pub(crate) fn new(width: u32, height: u32) -> Result<Self> {
        Ok(Self(
            tiny_skia::Pixmap::new(width, height).context("Cannot allocate screenshot UI")?,
            tiny_skia::Transform::identity(),
        ))
    }
    fn paint(color: u32) -> tiny_skia::Paint<'static> {
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(
            (color >> 16) as u8,
            (color >> 8) as u8,
            color as u8,
            (color >> 24) as u8,
        );
        paint
    }
    fn path(&mut self, path: tiny_skia::Path, color: u32, stroke: Option<f32>) {
        if let Some(width) = stroke {
            self.0.stroke_path(
                &path,
                &Self::paint(color),
                &tiny_skia::Stroke {
                    width,
                    line_cap: tiny_skia::LineCap::Round,
                    line_join: tiny_skia::LineJoin::Round,
                    ..Default::default()
                },
                self.1,
                None,
            );
        } else {
            self.0.fill_path(
                &path,
                &Self::paint(color),
                tiny_skia::FillRule::Winding,
                self.1,
                None,
            );
        }
    }
    pub(crate) fn rounded(&mut self, rect: Rect, radius: f64, color: u32, stroke: Option<f32>) {
        let (x, y, w, h) = (
            rect.x as f32,
            rect.y as f32,
            rect.width as f32,
            rect.height as f32,
        );
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let r = (radius as f32).min(w / 2.0).min(h / 2.0).max(0.0);
        let mut p = tiny_skia::PathBuilder::new();
        p.move_to(x + r, y);
        p.line_to(x + w - r, y);
        p.quad_to(x + w, y, x + w, y + r);
        p.line_to(x + w, y + h - r);
        p.quad_to(x + w, y + h, x + w - r, y + h);
        p.line_to(x + r, y + h);
        p.quad_to(x, y + h, x, y + h - r);
        p.line_to(x, y + r);
        p.quad_to(x, y, x + r, y);
        p.close();
        if let Some(path) = p.finish() {
            self.path(path, color, stroke);
        }
    }
    pub(crate) fn circle(&mut self, x: f64, y: f64, radius: f64, color: u32, stroke: Option<f32>) {
        if let Some(path) = tiny_skia::PathBuilder::from_circle(x as f32, y as f32, radius as f32) {
            self.path(path, color, stroke);
        }
    }
    pub(crate) fn line(&mut self, points: &[(f64, f64)], color: u32, width: f32) {
        let Some(first) = points.first() else { return };
        let mut p = tiny_skia::PathBuilder::new();
        p.move_to(first.0 as f32, first.1 as f32);
        for point in &points[1..] {
            p.line_to(point.0 as f32, point.1 as f32);
        }
        if let Some(path) = p.finish() {
            self.path(path, color, Some(width));
        }
    }
    pub(crate) fn panel(&mut self, rect: Rect) {
        self.panel_scaled(rect, 9., 1.);
    }
    fn panel_scaled(&mut self, rect: Rect, radius: f64, scale: f64) {
        for (spread, alpha) in [(3.0, 8), (2.0, 12), (1.0, 18)] {
            let spread = spread * scale;
            self.rounded(
                ui_rect(
                    rect.x - spread,
                    rect.y + 2. * scale - spread,
                    rect.width + spread * 2.,
                    rect.height + spread * 2.,
                ),
                radius + 2. * scale,
                (alpha << 24) | 0x0f172a,
                None,
            );
        }
        self.rounded(rect, radius, 0xfff8fafc, None);
        self.rounded(rect, radius, 0xffcbd5e1, Some(scale as f32));
    }
    pub(crate) fn icon_scaled(
        &mut self,
        icon: ToolbarIcon,
        x: f64,
        y: f64,
        scale: f64,
        color: u32,
    ) {
        let previous = self.1;
        self.1 = tiny_skia::Transform::from_scale(scale as f32, scale as f32)
            .post_translate(x as f32, y as f32);
        self.icon(icon, 0.0, 0.0, color);
        self.1 = previous;
    }
    pub(crate) fn icon(&mut self, icon: ToolbarIcon, x: f64, y: f64, color: u32) {
        let line = |this: &mut Self, pts: &[(f64, f64)]| {
            this.line(
                &pts.iter().map(|(a, b)| (x + a, y + b)).collect::<Vec<_>>(),
                color,
                2.1,
            );
        };
        match icon {
            ToolbarIcon::Rectangle | ToolbarIcon::Rounded => {
                self.rounded(
                    ui_rect(x + 4., y + 5., 16., 14.),
                    if icon == ToolbarIcon::Rounded {
                        4.
                    } else {
                        0.5
                    },
                    color,
                    Some(2.1),
                );
            }
            ToolbarIcon::Circle => self.circle(x + 12., y + 12., 8., color, Some(2.1)),
            ToolbarIcon::Pen => {
                line(
                    self,
                    &[
                        (4., 20.),
                        (5., 14.),
                        (16., 3.),
                        (21., 8.),
                        (10., 19.),
                        (4., 20.),
                    ],
                );
                line(self, &[(14., 5.), (19., 10.)]);
            }
            ToolbarIcon::Arrow => {
                line(self, &[(5., 19.), (19., 5.)]);
                line(self, &[(9., 5.), (19., 5.), (19., 15.)]);
            }
            ToolbarIcon::Text => {
                line(self, &[(4., 5.), (20., 5.)]);
                line(self, &[(12., 5.), (12., 20.)]);
                line(self, &[(8., 20.), (16., 20.)]);
            }
            ToolbarIcon::Serial => {
                self.circle(x + 12., y + 12., 9., color, Some(2.1));
                line(self, &[(9., 9.), (12., 7.), (12., 17.)]);
                line(self, &[(9., 17.), (15., 17.)]);
            }
            ToolbarIcon::Mosaic => {
                for row in 0..3 {
                    for col in 0..3 {
                        self.rounded(
                            ui_rect(x + 4. + col as f64 * 6., y + 4. + row as f64 * 6., 4., 4.),
                            0.5,
                            if (row + col) % 2 == 0 {
                                color
                            } else {
                                (color & 0xffffff) | 0x60000000
                            },
                            None,
                        );
                    }
                }
            }
            ToolbarIcon::Eraser => {
                line(
                    self,
                    &[
                        (3., 14.),
                        (13., 4.),
                        (21., 12.),
                        (13., 20.),
                        (9., 20.),
                        (3., 14.),
                    ],
                );
                line(self, &[(8., 9.), (16., 17.)]);
                line(self, &[(13., 20.), (22., 20.)]);
            }
            ToolbarIcon::Undo => {
                let mut p = tiny_skia::PathBuilder::new();
                p.move_to((x + 5.) as f32, (y + 9.) as f32);
                p.cubic_to(
                    (x + 20.) as f32,
                    (y + 2.) as f32,
                    (x + 25.) as f32,
                    (y + 22.) as f32,
                    (x + 10.) as f32,
                    (y + 20.) as f32,
                );
                if let Some(path) = p.finish() {
                    self.path(path, color, Some(2.1));
                }
                line(self, &[(5., 3.), (5., 9.), (11., 9.)]);
            }
            ToolbarIcon::Longshot => {
                self.rounded(ui_rect(x + 5., y + 3., 14., 18.), 2., color, Some(2.1));
                line(self, &[(12., 6.), (12., 18.)]);
                line(self, &[(9., 15.), (12., 18.), (15., 15.)]);
            }
            ToolbarIcon::Record => {
                self.rounded(ui_rect(x + 3., y + 6., 12., 12.), 2., color, Some(2.1));
                line(self, &[(15., 9.), (21., 6.), (21., 18.), (15., 15.)]);
            }
            ToolbarIcon::Ocr => {
                for pts in [
                    &[(3., 8.), (3., 3.), (8., 3.)][..],
                    &[(16., 3.), (21., 3.), (21., 8.)],
                    &[(3., 16.), (3., 21.), (8., 21.)],
                    &[(16., 21.), (21., 21.), (21., 16.)],
                ] {
                    line(self, pts);
                }
                line(self, &[(7., 17.), (12., 7.), (17., 17.)]);
                line(self, &[(9., 14.), (15., 14.)]);
            }
            ToolbarIcon::QuickAsk => {
                line(
                    self,
                    &[
                        (19., 12.),
                        (19., 19.),
                        (10., 19.),
                        (5., 22.),
                        (5., 18.),
                        (3., 18.),
                        (3., 5.),
                        (12., 5.),
                    ],
                );
                line(
                    self,
                    &[
                        (18., 2.),
                        (19., 6.),
                        (23., 7.),
                        (19., 8.),
                        (18., 12.),
                        (17., 8.),
                        (13., 7.),
                        (17., 6.),
                        (18., 2.),
                    ],
                );
            }
            ToolbarIcon::Pin => {
                line(
                    self,
                    &[
                        (9., 3.),
                        (20., 14.),
                        (16., 14.),
                        (12., 18.),
                        (6., 12.),
                        (10., 8.),
                        (9., 3.),
                    ],
                );
                line(self, &[(9., 15.), (3., 21.)]);
            }
            ToolbarIcon::Save => {
                line(self, &[(12., 3.), (12., 16.)]);
                line(self, &[(7., 11.), (12., 16.), (17., 11.)]);
                line(self, &[(4., 16.), (4., 21.), (20., 21.), (20., 16.)]);
            }
            ToolbarIcon::Cancel => {
                line(self, &[(5., 5.), (19., 19.)]);
                line(self, &[(19., 5.), (5., 19.)]);
            }
            ToolbarIcon::Copy => {
                self.rounded(ui_rect(x + 7., y + 7., 13., 14.), 1.5, color, Some(2.1));
                line(
                    self,
                    &[(16., 7.), (16., 3.), (3., 3.), (3., 17.), (7., 17.)],
                );
            }
            ToolbarIcon::Fill => {
                line(
                    self,
                    &[(4., 12.), (12., 4.), (20., 12.), (12., 20.), (4., 12.)],
                );
                self.line(&[(x + 6., y + 13.), (x + 18., y + 13.)], color, 4.);
            }
        }
    }
    pub(crate) fn pixmap(&mut self, pixmap: &tiny_skia::Pixmap, x: f64, y: f64) {
        self.0.draw_pixmap(
            x.round() as i32,
            y.round() as i32,
            pixmap.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            tiny_skia::Transform::identity(),
            None,
        );
    }
    pub(crate) fn centered(&mut self, pixmap: &tiny_skia::Pixmap, rect: Rect) {
        self.pixmap(
            pixmap,
            rect.x + (rect.width - f64::from(pixmap.width())) / 2.,
            rect.y + (rect.height - f64::from(pixmap.height())) / 2.,
        );
    }
    pub(crate) fn tooltip(&mut self, text: &str, anchor: Point, scale: f64) -> Result<()> {
        let text = render_ui_text(text, 16. * scale, true, 0xffffffff)?;
        let rect = tooltip_bounds(
            &text,
            anchor,
            scale,
            ui_rect(
                0.,
                0.,
                f64::from(self.0.width()),
                f64::from(self.0.height()),
            ),
        );
        self.tooltip_box(&text, rect, scale);
        Ok(())
    }
    fn tooltip_box(&mut self, text: &tiny_skia::Pixmap, rect: Rect, scale: f64) {
        self.rounded(rect, 8. * scale, 0xf218191c, None);
        self.rounded(rect, 8. * scale, 0xff383c47, Some(scale as f32));
        self.centered(text, rect);
    }
    pub(crate) fn present_layered(self, hwnd: HWND) -> Result<()> {
        let size = SIZE {
            cx: self.0.width() as i32,
            cy: self.0.height() as i32,
        };
        let dib = Dib::new(self.0.width(), self.0.height())?;
        let mut pixels = self.0.take();
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), dib.bits, pixels.len());
            UpdateLayeredWindow(
                hwnd,
                None,
                None,
                Some(&size),
                Some(dib.dc),
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
        }
        Ok(())
    }
    pub(crate) fn image(self) -> CapturedImage {
        let (width, height) = (self.0.width(), self.0.height());
        let mut pixels = self.0.take();
        for p in pixels.as_chunks_mut::<4>().0 {
            if p[3] != 0 {
                for c in 0..3 {
                    p[c] = (p[c] as u32 * 255 / p[3] as u32).min(255) as u8;
                }
            }
            p.swap(0, 2);
        }
        CapturedImage {
            width,
            height,
            pixels,
        }
    }
}

fn composite_image(target: &mut CapturedImage, source: &CapturedImage, x: i32, y: i32) {
    for sy in 0.max(-y)..(source.height as i32).min(target.height as i32 - y) {
        for sx in 0.max(-x)..(source.width as i32).min(target.width as i32 - x) {
            let s = &source.pixels[(sy as usize * source.width as usize + sx as usize) * 4..][..4];
            let d = &mut target.pixels
                [((sy + y) as usize * target.width as usize + (sx + x) as usize) * 4..][..4];
            let alpha = s[3] as u32;
            if alpha == 0 {
                continue;
            }
            if alpha == 255 {
                d.copy_from_slice(s);
                continue;
            }
            let out = alpha + d[3] as u32 * (255 - alpha) / 255;
            for c in 0..3 {
                d[c] = ((s[c] as u32 * alpha + d[c] as u32 * d[3] as u32 * (255 - alpha) / 255)
                    / out) as u8;
            }
            d[3] = out as u8;
        }
    }
}

fn tooltip_bounds(text: &tiny_skia::Pixmap, anchor: Point, scale: f64, viewport: Rect) -> Rect {
    let width = f64::from(text.width()) + 24. * scale;
    let height = (f64::from(text.height()) + 14. * scale).max(36. * scale);
    let x = (anchor.x - width / 2.).clamp(
        viewport.x + 4. * scale,
        (viewport.x + viewport.width - width - 4. * scale).max(viewport.x + 4. * scale),
    );
    let y = (anchor.y - height - 6. * scale).max(viewport.y + 4. * scale);
    ui_rect(x, y, width, height)
}

fn local_canvas_bounds(rectangles: &[Rect], viewport: Rect, padding: f64) -> Option<Rect> {
    let first = *rectangles.first()?;
    let (mut left, mut top) = (first.x, first.y);
    let (mut right, mut bottom) = (first.x + first.width, first.y + first.height);
    for rect in &rectangles[1..] {
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.x + rect.width);
        bottom = bottom.max(rect.y + rect.height);
    }
    let left = (left - padding).floor().max(viewport.x);
    let top = (top - padding).floor().max(viewport.y);
    let right = (right + padding).ceil().min(viewport.x + viewport.width);
    let bottom = (bottom + padding).ceil().min(viewport.y + viewport.height);
    (right > left && bottom > top).then(|| ui_rect(left, top, right - left, bottom - top))
}

fn fill_ui_rect(image: &mut CapturedImage, rect: Rect, color: u32) {
    let Some(rect) = rect.intersection(ui_rect(
        0.,
        0.,
        f64::from(image.width),
        f64::from(image.height),
    )) else {
        return;
    };
    let left = rect.x.floor() as usize;
    let right = ((rect.x + rect.width).ceil() as usize).min(image.width as usize);
    let top = rect.y.floor() as usize;
    let bottom = ((rect.y + rect.height).ceil() as usize).min(image.height as usize);
    let pixel = color.to_le_bytes();
    for row in top..bottom {
        let start = (row * image.width as usize + left) * 4;
        let end = (row * image.width as usize + right) * 4;
        for destination in image.pixels[start..end].as_chunks_mut::<4>().0 {
            *destination = pixel;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CapturePhase {
    Selecting,
    Editing,
}
#[derive(Clone, Copy)]
enum SelectionDrag {
    Move { start: Point, rect: Rect },
    Resize { handle: usize, rect: Rect },
}

#[derive(Clone, Copy)]
enum MarkDrag {
    Move { last: Point },
    Handle { handle: usize },
}

struct CaptureEditor {
    phase: CapturePhase,
    scene: Scene,
    tool: Option<ToolbarIcon>,
    style: MarkStyle,
    points: Vec<Point>,
    drag: Option<SelectionDrag>,
    mark_drag: Option<MarkDrag>,
    text: Option<ArrowCommentEditor>,
    probe_point: Option<Point>,
    probe_window: Option<isize>,
    pressed_tool: Option<ToolbarIcon>,
    pressed_style: Option<usize>,
    pressing_controls: bool,
}
impl Default for CaptureEditor {
    fn default() -> Self {
        Self {
            phase: CapturePhase::Selecting,
            scene: Scene::default(),
            tool: None,
            style: MarkStyle {
                argb: COLORS[0],
                width: 4.,
                ..Default::default()
            },
            points: Vec::new(),
            drag: None,
            mark_drag: None,
            text: None,
            probe_point: None,
            probe_window: None,
            pressed_tool: None,
            pressed_style: None,
            pressing_controls: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureTextKind {
    Color,
    Coordinate,
}

pub enum CaptureEvent {
    Selected {
        image: CapturedImage,
        rect: Rect,
        action: CaptureAction,
    },
    TextCopied {
        text: String,
        kind: CaptureTextKind,
    },
    Cancelled,
    Error(String),
}

#[derive(Default)]
struct SelectionHistory(Mutex<Option<Rect>>);
impl SelectionHistory {
    fn remember(&self, event: &CaptureEvent) {
        if let CaptureEvent::Selected { rect, action, .. } = event
            && !matches!(action, CaptureAction::Longshot | CaptureAction::Record)
        {
            *self.0.lock().unwrap() = Some(*rect);
        }
    }
    fn last(&self) -> Option<Rect> {
        *self.0.lock().unwrap()
    }
}
static SELECTION_HISTORY: SelectionHistory = SelectionHistory(Mutex::new(None));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureKey {
    Handled,
    Cancel,
    Image(CaptureAction),
    Text(CaptureTextKind),
}

struct OverlayState {
    windows: Vec<isize>,
    snapshots: Vec<WindowSnapshot>,
    monitors: Vec<(Monitor, Arc<CapturedImage>)>,
    selection: Option<Rect>,
    origin: Option<Point>,
    cursor: Point,
    shape: SelectionShape,
    hierarchy: Vec<Rect>,
    level: usize,
    generation: u64,
    probe: UiaProbe,
    events: Vec<CaptureEvent>,
    finished: bool,
    had_menu: bool,
    wake: Option<isize>,
    passive: bool,
    mask: u32,
    color_format: usize,
    smart: bool,
    editing: CaptureEditor,
}
struct OverlaySurface {
    state: Rc<RefCell<OverlayState>>,
    monitor: usize,
    target: RefCell<PresentTarget>,
    masked: RefCell<Option<(u32, CapturedImage)>>,
    scratch: RefCell<Option<CapturedImage>>,
}
pub struct CaptureAndPinWindows {
    windows: Vec<(HWND, Box<OverlaySurface>)>,
    state: Rc<RefCell<OverlayState>>,
}

impl CaptureAndPinWindows {
    pub fn capture(graphics: Rc<D2dContext>, ignore: &[isize]) -> Result<Self> {
        Self::create(graphics, ignore, false)
    }
    pub fn passive_capture(graphics: Rc<D2dContext>, ignore: &[isize]) -> Result<Self> {
        Self::create(graphics, ignore, true)
    }
    fn create(graphics: Rc<D2dContext>, ignore: &[isize], passive: bool) -> Result<Self> {
        let had_menu = menu_active();
        let snapshots = window_snapshot(ignore)?;
        let mut images = Vec::new();
        for monitor in monitors()? {
            let image = capture_rect(monitor.bounds)?;
            images.push((monitor, Arc::new(image)));
        }
        let probe = UiaProbe::new()?;
        let state = Rc::new(RefCell::new(OverlayState {
            windows: Vec::new(),
            snapshots,
            monitors: images,
            selection: None,
            origin: None,
            cursor: Point::default(),
            shape: SelectionShape::default(),
            hierarchy: Vec::new(),
            level: 0,
            generation: 0,
            probe,
            events: Vec::new(),
            finished: false,
            had_menu,
            wake: None,
            passive,
            mask: 0x80000000,
            color_format: 0,
            smart: true,
            editing: CaptureEditor::default(),
        }));
        let mut result = Self {
            windows: Vec::new(),
            state,
        };
        unsafe {
            let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let creation = (|| -> Result<()> {
                let instance = GetModuleHandleW(None)?;
                let class = WNDCLASSW {
                    style: CS_DBLCLKS,
                    lpfnWndProc: Some(overlay_proc),
                    hInstance: instance.into(),
                    hCursor: LoadCursorW(None, IDC_CROSS)?,
                    lpszClassName: w!("OrbmentCaptureOverlay"),
                    ..Default::default()
                };
                RegisterClassW(&class);
                let count = result.state.borrow().monitors.len();
                for index in 0..count {
                    let rect = result.state.borrow().monitors[index].0.bounds;
                    let ex = WS_EX_TOPMOST
                        | WS_EX_TOOLWINDOW
                        | if passive {
                            WS_EX_TRANSPARENT | WS_EX_NOACTIVATE
                        } else {
                            WINDOW_EX_STYLE(0)
                        };
                    let hwnd = CreateWindowExW(
                        ex,
                        w!("OrbmentCaptureOverlay"),
                        w!("Orbment"),
                        WS_POPUP,
                        rect.x as i32,
                        rect.y as i32,
                        rect.width as i32,
                        rect.height as i32,
                        None,
                        None,
                        Some(instance.into()),
                        None,
                    )?;
                    let target = match PresentTarget::new(
                        graphics.clone(),
                        hwnd,
                        rect.width as u32,
                        rect.height as u32,
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = DestroyWindow(hwnd);
                            return Err(e);
                        }
                    };
                    let surface = Box::new(OverlaySurface {
                        state: result.state.clone(),
                        monitor: index,
                        target: RefCell::new(target),
                        masked: RefCell::new(None),
                        scratch: RefCell::new(None),
                    });
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*surface as *const _ as isize);
                    if let Err(e) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                        let _ = DestroyWindow(hwnd);
                        return Err(e.into());
                    }
                    result.state.borrow_mut().windows.push(hwnd.0 as isize);
                    result.windows.push((hwnd, surface));
                }
                for (hwnd, _) in &result.windows {
                    let _ = ShowWindow(*hwnd, if passive { SW_SHOWNOACTIVATE } else { SW_SHOW });
                    if !passive {
                        SetTimer(Some(*hwnd), 1, 16, None);
                    }
                    let _ = InvalidateRect(Some(*hwnd), None, false);
                }
                if !passive && let Some((hwnd, _)) = result.windows.first() {
                    let _ = SetForegroundWindow(*hwnd);
                }
                Ok(())
            })();
            SetThreadDpiAwarenessContext(previous);
            creation?;
        }
        Ok(result)
    }
    pub fn set_shape(&mut self, shape: SelectionShape) {
        self.state.borrow_mut().shape = shape;
    }
    pub fn set_wake_hwnd(&mut self, hwnd: isize) {
        self.state.borrow_mut().wake = Some(hwnd);
    }
    pub fn configure(&mut self, config: &orbment_core::config::VisionScreenshotConfig) {
        let mut state = self.state.borrow_mut();
        state.mask = u32::from_str_radix(&config.unselected_mask_color, 16).unwrap_or(0x80000000);
        state.smart = config.select_mode.eq_ignore_ascii_case("Smart");
    }
    pub fn set_passive(&mut self, passive: bool) -> Result<()> {
        self.state.borrow_mut().passive = passive;
        for (hwnd, _) in &self.windows {
            unsafe {
                let mut style = GetWindowLongPtrW(*hwnd, GWL_EXSTYLE) as u32;
                if passive {
                    style |= WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0;
                } else {
                    style &= !(WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0);
                }
                SetWindowLongPtrW(*hwnd, GWL_EXSTYLE, style as isize);
                SetWindowPos(
                    *hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                )?;
            }
        }
        Ok(())
    }
    pub fn preview(&mut self, rect: Rect) {
        let mut state = self.state.borrow_mut();
        state.selection = Some(rect);
        state.redraw();
    }
    pub fn had_active_menu(&self) -> bool {
        self.state.borrow().had_menu
    }
    pub fn finish_as_pin(&mut self) -> Result<bool> {
        let (windows, wake) = {
            let mut state = self.state.borrow_mut();
            if state.finished || state.passive {
                return Ok(false);
            }
            if let Some(origin) = state.origin {
                state.selection = Some(state.shape.constrain(origin, state.cursor));
            }
            if !state
                .selection
                .is_some_and(|r| r.width >= 1. && r.height >= 1.)
            {
                return Ok(false);
            }
            (state.finish(CaptureAction::Pin)?, state.wake)
        };
        hide_capture_windows(&windows, wake);
        Ok(true)
    }
    pub fn poll(&mut self) -> Vec<CaptureEvent> {
        std::mem::take(&mut self.state.borrow_mut().events)
    }
    pub fn finished(&self) -> bool {
        self.state.borrow().finished
    }
}
impl Drop for CaptureAndPinWindows {
    fn drop(&mut self) {
        {
            let mut state = self.state.borrow_mut();
            state.finished = true;
            state.windows.clear();
        }
        for (hwnd, _) in &self.windows {
            unsafe {
                SetWindowLongPtrW(*hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(*hwnd);
            }
        }
    }
}

impl OverlayState {
    fn redraw(&self) {
        for hwnd in &self.windows {
            unsafe {
                let _ = InvalidateRect(Some(HWND(*hwnd as *mut _)), None, false);
            }
        }
    }
    fn selection_image(&self, preview: bool) -> Result<(CapturedImage, Rect)> {
        let rect = self
            .selection
            .context("No capture selection")?
            .intersection(virtual_desktop())
            .context("Capture is outside the desktop")?
            .validate()?;
        let mut image = CapturedImage {
            width: rect.width as u32,
            height: rect.height as u32,
            pixels: vec![0; rect.width as usize * rect.height as usize * 4],
        };
        for (monitor, source) in &self.monitors {
            if let Some(overlap) = rect.intersection(monitor.bounds) {
                for y in overlap.y as i32..(overlap.y + overlap.height) as i32 {
                    for x in overlap.x as i32..(overlap.x + overlap.width) as i32 {
                        let point = Point {
                            x: x as f64,
                            y: y as f64,
                        };
                        if !self.shape.includes(point, rect) {
                            continue;
                        }
                        let src = (((y as f64 - monitor.bounds.y) as usize)
                            * source.width as usize
                            + (x as f64 - monitor.bounds.x) as usize)
                            * 4;
                        let dst = (((y as f64 - rect.y) as usize) * image.width as usize
                            + (x as f64 - rect.x) as usize)
                            * 4;
                        if dst + 4 <= image.pixels.len() && src + 4 <= source.pixels.len() {
                            image.pixels[dst..dst + 4]
                                .copy_from_slice(&source.pixels[src..src + 4]);
                        }
                    }
                }
            }
        }
        let mut scene = self.editing.scene.clone();
        if preview {
            self.add_pending_mark(&mut scene);
        }
        let mut image = if scene.marks().is_empty() {
            image
        } else if preview {
            scene.render_with_selection(&image, self.editing.text.as_ref())?
        } else {
            scene.render(&image)?
        };
        // Annotation strokes must not fill the transparent corners of shaped captures.
        for y in 0..image.height {
            for x in 0..image.width {
                if !self.shape.includes(
                    Point {
                        x: rect.x + x as f64,
                        y: rect.y + y as f64,
                    },
                    rect,
                ) {
                    image.pixels[(y as usize * image.width as usize + x as usize) * 4..][..4]
                        .fill(0);
                }
            }
        }
        Ok((image, rect))
    }
    fn finish(&mut self, action: CaptureAction) -> Result<Vec<isize>> {
        self.commit_mark_drag();
        self.commit_text();
        self.commit_mark();
        self.cancel_pointer();
        let (image, rect) = self.selection_image(false)?;
        let event = CaptureEvent::Selected {
            image,
            rect,
            action,
        };
        SELECTION_HISTORY.remember(&event);
        self.events.push(event);
        self.finished = true;
        Ok(self.windows.clone())
    }
    fn sampled_color(&self) -> Option<[u8; 3]> {
        self.monitors
            .iter()
            .find_map(|(monitor, image)| sample_color(image, monitor.bounds, self.cursor))
    }
    fn finish_text(&mut self, kind: CaptureTextKind) -> Result<Vec<isize>> {
        let text = match kind {
            CaptureTextKind::Color => format_color(
                self.sampled_color()
                    .context("Capture pointer is outside the monitors")?,
                COLOR_FORMATS[self.color_format],
            ),
            CaptureTextKind::Coordinate => format_coordinate(self.cursor),
        };
        self.cancel_pointer();
        self.events.push(CaptureEvent::TextCopied { text, kind });
        self.finished = true;
        Ok(self.windows.clone())
    }
    fn cancel(&mut self) -> Vec<isize> {
        self.cancel_pointer();
        self.finished = true;
        self.events.push(CaptureEvent::Cancelled);
        self.windows.clone()
    }
    fn enter_editing(&mut self, desktop: Rect) {
        if let Some(rect) = self.selection.and_then(|r| r.intersection(desktop))
            && rect.width >= 1.
            && rect.height >= 1.
        {
            self.selection = Some(ui_rect(
                rect.x.floor(),
                rect.y.floor(),
                rect.width.floor(),
                rect.height.floor(),
            ));
            self.editing.phase = CapturePhase::Editing;
            self.generation += 1;
            self.origin = None;
        }
    }
    fn restore_selection(&mut self, rect: Rect, desktop: Rect) -> bool {
        let Some(rect) = rect
            .intersection(desktop)
            .filter(|rect| rect.width >= 1. && rect.height >= 1.)
        else {
            return false;
        };
        self.editing = CaptureEditor::default();
        self.shape = SelectionShape::Rectangle;
        self.selection = Some(rect);
        self.hierarchy.clear();
        self.level = 0;
        self.enter_editing(desktop);
        true
    }
    fn key_down(
        &mut self,
        key: u16,
        control: bool,
        shift: bool,
        desktop: Rect,
        history: &SelectionHistory,
    ) -> Option<CaptureKey> {
        if self.editing.text.is_some() {
            return None;
        }
        let selecting = self.editing.phase == CapturePhase::Selecting;
        if matches!(key, 0x25..=0x28) {
            if self.editing.mark_drag.is_some() {
                return Some(CaptureKey::Handled);
            }
            if !selecting
                && self.editing.drag.is_none()
                && self.editing.points.is_empty()
                && self.editing.scene.selected_mark().is_some()
            {
                let step = if shift { 10. } else { 1. };
                let (dx, dy) = match key {
                    0x25 => (-step, 0.),
                    0x26 => (0., -step),
                    0x27 => (step, 0.),
                    _ => (0., step),
                };
                self.editing
                    .scene
                    .edit_selected(|mark| mark.translate(dx, dy));
                return Some(CaptureKey::Handled);
            }
            if self.origin.is_none()
                && self.editing.drag.is_none()
                && self.editing.points.is_empty()
                && let Some(previous) = self.selection
                && let Some(rect) = keyboard_selection(previous, key, control, shift, desktop)
            {
                if control && matches!(self.shape, SelectionShape::Polygon(_)) {
                    self.shape = SelectionShape::Rectangle;
                } else if let SelectionShape::Polygon(points) = &mut self.shape {
                    for point in points {
                        point.x += rect.x - previous.x;
                        point.y += rect.y - previous.y;
                    }
                }
                self.selection = Some(rect);
                self.enter_editing(desktop);
            }
            return Some(CaptureKey::Handled);
        }
        match key {
            key if selecting && key == VK_TAB.0 => {
                if self.origin.is_none() && !self.hierarchy.is_empty() {
                    self.level = if self.level == 0 {
                        self.hierarchy.len() - 1
                    } else {
                        0
                    };
                    self.selection = Some(self.hierarchy[self.level]);
                }
            }
            key if !selecting && (key == VK_DELETE.0 || key == VK_BACK.0) => {
                self.cancel_pointer();
                self.editing.scene.delete_selected();
            }
            key if key == VK_ESCAPE.0 => return Some(CaptureKey::Cancel),
            key if key == VK_RETURN.0 => {
                if selecting {
                    self.enter_editing(desktop);
                } else {
                    return Some(CaptureKey::Image(CaptureAction::Confirm));
                }
            }
            key if key == VK_F2.0 => return Some(CaptureKey::Image(CaptureAction::Pin)),
            0x4c if !selecting && !control && !shift => {
                return Some(CaptureKey::Image(CaptureAction::Longshot));
            }
            0x43 if control => return Some(CaptureKey::Image(CaptureAction::Copy)),
            0x53 if control => {
                if shift && selecting {
                    return None;
                }
                return Some(CaptureKey::Image(if shift {
                    CaptureAction::QuickSave
                } else {
                    CaptureAction::Save
                }));
            }
            0x54 if control => return Some(CaptureKey::Image(CaptureAction::Pin)),
            0x5a if control => {
                self.cancel_pointer();
                self.editing.scene.undo();
            }
            0x59 if control => {
                self.cancel_pointer();
                self.editing.scene.redo();
            }
            0x43 | 0x46 if selecting && !control && (key == 0x46 || shift) => {
                self.color_format = (self.color_format + 1) % COLOR_FORMATS.len();
            }
            0x43 if selecting => return Some(CaptureKey::Text(CaptureTextKind::Color)),
            0x58 if selecting && !control => {
                return Some(CaptureKey::Text(CaptureTextKind::Coordinate));
            }
            key if selecting && !control && (key == 0x48 || key == VK_OEM_COMMA.0) => {
                if let Some(rect) = history.last() {
                    self.restore_selection(rect, desktop);
                }
            }
            _ => return None,
        }
        Some(CaptureKey::Handled)
    }
    fn commit_text(&mut self) {
        if let Some(editor) = self.editing.text.take() {
            self.editing
                .scene
                .apply_comment(editor.mark_id, editor.preview());
            self.editing.scene.commit_edit();
        }
    }
    fn edit_text_at(&mut self, point: Point) -> bool {
        if self.editing.tool == Some(ToolbarIcon::Eraser) {
            return false;
        }
        self.commit_mark_drag();
        self.commit_text();
        let local = self.mark_point(point);
        let editor = self
            .editing
            .scene
            .double_click_text(local, 8.)
            .or_else(|| self.editing.scene.double_click_comment(local, 8., true));
        let Some(editor) = editor else {
            return false;
        };
        if let Some(mark) = self.editing.scene.selected_mark() {
            self.editing.style = mark.style.clone();
        }
        self.editing.scene.begin_edit();
        self.editing.text = Some(editor);
        true
    }
    fn add_pending_mark(&self, scene: &mut Scene) {
        let Some(first) = self.editing.points.first().copied() else {
            return;
        };
        let last = self.editing.points.last().copied().unwrap_or(first);
        let bounds = Rect::from_points(first, last);
        let style = self.editing.style.clone();
        let points = self.editing.points.clone();
        let kind = match self.editing.tool {
            Some(ToolbarIcon::Rectangle) => MarkKind::Rectangle { bounds },
            Some(ToolbarIcon::Circle) => MarkKind::Ellipse { bounds },
            Some(ToolbarIcon::Pen) => MarkKind::Brush { points },
            Some(ToolbarIcon::Arrow) => MarkKind::Arrow {
                points: vec![first, last],
                comment: None,
            },
            Some(ToolbarIcon::Mosaic) => MarkKind::Mosaic {
                bounds,
                mode: MosaicMode::Pixel,
                strength: style.width.max(4.),
            },
            Some(ToolbarIcon::Eraser) => {
                let mut style = style;
                style.width = style.width.max(12.);
                scene.add(MarkKind::Eraser { points }, style);
                return;
            }
            Some(ToolbarIcon::Rounded) => MarkKind::RoundedRectangle {
                bounds,
                radius: 12f64.min(bounds.width / 2.).min(bounds.height / 2.),
            },
            _ => return,
        };
        scene.add(kind, style);
    }
    fn commit_mark(&mut self) {
        if !self.editing.points.is_empty() {
            let mut scene = std::mem::take(&mut self.editing.scene);
            scene.begin_edit();
            self.add_pending_mark(&mut scene);
            scene.commit_edit();
            self.editing.scene = scene;
            self.editing.points.clear();
        }
    }
    fn mark_point(&self, point: Point) -> Point {
        let rect = self.selection.unwrap_or_default();
        Point {
            x: point.x - rect.x,
            y: point.y - rect.y,
        }
    }
    fn mark_handle_at(&self, point: Point) -> Option<usize> {
        if self.editing.tool == Some(ToolbarIcon::Eraser) {
            return None;
        }
        self.editing
            .scene
            .selected_mark()?
            .handle_at(self.mark_point(point), 8.)
    }
    fn mark_cursor(&self, point: Point) -> Option<PCWSTR> {
        if self.editing.tool == Some(ToolbarIcon::Eraser) {
            return None;
        }
        if let Some(handle) = self.mark_handle_at(point) {
            let mark = self.editing.scene.selected_mark()?;
            return Some(match mark.kind {
                MarkKind::Arrow { .. }
                | MarkKind::Polyline { .. }
                | MarkKind::Magnifier { .. }
                | MarkKind::Sequence { .. } => IDC_SIZEALL,
                _ => match handle {
                    0 | 4 => IDC_SIZENWSE,
                    2 | 6 => IDC_SIZENESW,
                    1 | 5 => IDC_SIZENS,
                    _ => IDC_SIZEWE,
                },
            });
        }
        if self.selection.is_some_and(|rect| rect.contains(point))
            && self
                .editing
                .scene
                .hit_test_or_selected(self.mark_point(point), 8.)
                .is_some()
        {
            Some(IDC_SIZEALL)
        } else {
            None
        }
    }
    fn begin_mark_drag(&mut self, point: Point) -> bool {
        if self.editing.tool == Some(ToolbarIcon::Eraser) {
            self.editing.scene.clear_selection();
            return false;
        }
        let local = self.mark_point(point);
        let selected_handle = self.mark_handle_at(point);
        if selected_handle.is_none()
            && (!self.selection.is_some_and(|rect| rect.contains(point))
                || self.editing.scene.select(local, 8.).is_none())
        {
            self.editing.scene.clear_selection();
            return false;
        }
        let Some(mark) = self.editing.scene.selected_mark() else {
            return false;
        };
        let handle = selected_handle.or_else(|| mark.handle_at(local, 8.));
        self.editing.style = mark.style.clone();
        self.editing.mark_drag = Some(match handle {
            Some(handle) => MarkDrag::Handle { handle },
            None => MarkDrag::Move { last: local },
        });
        self.editing.scene.begin_edit();
        true
    }
    fn drag_mark(&mut self, point: Point) {
        let local = self.mark_point(point);
        match self.editing.mark_drag {
            Some(MarkDrag::Move { last }) => {
                self.editing
                    .scene
                    .edit_selected(|mark| mark.translate(local.x - last.x, local.y - last.y));
                self.editing.mark_drag = Some(MarkDrag::Move { last: local });
            }
            Some(MarkDrag::Handle { handle }) => {
                self.editing
                    .scene
                    .edit_selected(|mark| mark.resize_by_handle(handle, local));
            }
            None => {}
        }
    }
    fn commit_mark_drag(&mut self) {
        if self.editing.mark_drag.take().is_some() {
            self.editing.scene.commit_edit();
        }
    }
    fn local_point(&self, point: Point) -> Point {
        let rect = self.selection.unwrap_or_default();
        Point {
            x: (point.x - rect.x).clamp(0., rect.width),
            y: (point.y - rect.y).clamp(0., rect.height),
        }
    }
    fn handles(&self) -> [Point; 8] {
        let r = self.selection.unwrap_or_default();
        [
            (0., 0.),
            (0.5, 0.),
            (1., 0.),
            (1., 0.5),
            (1., 1.),
            (0.5, 1.),
            (0., 1.),
            (0., 0.5),
        ]
        .map(|(x, y)| Point {
            x: r.x + r.width * x,
            y: r.y + r.height * y,
        })
    }
    fn scale_at(&self, point: Point) -> f64 {
        self.monitors
            .iter()
            .find(|(m, _)| m.bounds.contains(point))
            .map_or(1.25, |(m, _)| (f64::from(m.dpi) / 96.).max(1.25))
    }
    fn toolbar_monitor(&self) -> Option<&Monitor> {
        let rect = self.selection?;
        let anchor = Point {
            x: rect.x + rect.width - 1.,
            y: rect.y + rect.height - 1.,
        };
        self.monitors
            .iter()
            .find(|(m, _)| m.bounds.contains(anchor))
            .or_else(|| {
                self.monitors
                    .iter()
                    .find(|(m, _)| m.bounds.intersection(rect).is_some())
            })
            .map(|(m, _)| m)
    }
    fn ui_scale(&self) -> f64 {
        self.toolbar_monitor()
            .map_or(1.25, |m| (f64::from(m.dpi) / 96.).max(1.25))
    }
    fn handle_at(&self, point: Point) -> Option<usize> {
        self.handles()
            .iter()
            .position(|p| (p.x - point.x).hypot(p.y - point.y) <= 9. * self.scale_at(*p))
    }
    fn styles_visible(&self) -> bool {
        self.editing.tool.is_some() || self.editing.scene.selected_mark().is_some()
    }
    fn toolbar(&self) -> Option<(Rect, Rect)> {
        if self.editing.phase != CapturePhase::Editing {
            return None;
        }
        let rect = self.selection?;
        let monitor = self.toolbar_monitor()?.bounds;
        let scale = self.ui_scale();
        let columns = ((monitor.width / scale - 24.) / 44.).floor().clamp(1., 17.) as usize;
        let style_columns = ((monitor.width / scale - 28.) / 34.).floor().clamp(1., 16.) as usize;
        let width = (columns as f64 * 44. + 16.) * scale;
        let bar_height = CAPTURE_TOOLS.len().div_ceil(columns) as f64 * 52. * scale;
        let style_width = (style_columns as f64 * 34. + 20.) * scale;
        let style_height = 16_usize.div_ceil(style_columns) as f64 * 46. * scale;
        let height = bar_height
            + if self.styles_visible() {
                6. * scale + style_height
            } else {
                0.
            };
        let x = (rect.x + rect.width - width).clamp(
            monitor.x + 4. * scale,
            (monitor.x + monitor.width - width - 4. * scale).max(monitor.x + 4. * scale),
        );
        let below = rect.y + rect.height + 8. * scale;
        let y = if below + height + 4. * scale <= monitor.y + monitor.height {
            below
        } else if rect.y - height - 8. * scale >= monitor.y {
            rect.y - height - 8. * scale
        } else {
            (monitor.y + monitor.height - height - 4. * scale).max(monitor.y + 4. * scale)
        };
        let style_x = x.min((monitor.x + monitor.width - style_width - 4. * scale).max(monitor.x));
        Some((
            ui_rect(x, y, width, bar_height),
            ui_rect(
                style_x,
                y + bar_height + 6. * scale,
                style_width,
                style_height,
            ),
        ))
    }
    fn tool_rect(&self, bar: Rect, index: usize) -> Rect {
        let scale = self.ui_scale();
        let columns = ((bar.width / scale - 16.) / 44.).round().max(1.) as usize;
        ui_rect(
            bar.x + (10. + (index % columns) as f64 * 44.) * scale,
            bar.y + (6. + (index / columns) as f64 * 52.) * scale,
            40. * scale,
            40. * scale,
        )
    }
    fn style_rect(&self, bar: Rect, index: usize) -> Rect {
        let scale = self.ui_scale();
        let columns = ((bar.width / scale - 20.) / 34.).round().max(1.) as usize;
        ui_rect(
            bar.x + (10. + (index % columns) as f64 * 34.) * scale,
            bar.y + (7. + (index / columns) as f64 * 46.) * scale,
            32. * scale,
            32. * scale,
        )
    }
    fn tool_at(&self, point: Point) -> Option<ToolbarIcon> {
        let (bar, _) = self.toolbar()?;
        CAPTURE_TOOLS
            .iter()
            .enumerate()
            .find_map(|(i, tool)| self.tool_rect(bar, i).contains(point).then_some(*tool))
    }
    fn style_at(&self, point: Point) -> Option<usize> {
        let (_, bar) = self.toolbar()?;
        if !self.styles_visible() {
            return None;
        }
        (0..16).find(|i| self.style_rect(bar, *i).contains(point))
    }
    fn apply_style(&mut self, index: usize) {
        match index {
            0..=2 => {
                self.editing.tool = Some(
                    [
                        ToolbarIcon::Rectangle,
                        ToolbarIcon::Rounded,
                        ToolbarIcon::Circle,
                    ][index],
                )
            }
            3 => self.editing.style.fill = !self.editing.style.fill,
            4..=8 => {
                self.editing.style.width = WIDTHS[index - 4];
                self.editing.style.font.size = WIDTHS[index - 4];
            }
            9..=15 => self.editing.style.argb = COLORS[index - 9],
            _ => {}
        }
        if (3..=15).contains(&index) {
            let style = self.editing.style.clone();
            self.editing.scene.begin_edit();
            self.editing.scene.edit_selected(|mark| mark.style = style);
            self.editing.scene.commit_edit();
        }
    }
    fn press_control(&mut self, point: Point) -> bool {
        let Some((bar, style)) = self.toolbar() else {
            return false;
        };
        if !(bar.contains(point) || self.styles_visible() && style.contains(point)) {
            return false;
        }
        self.editing.pressed_tool = self.tool_at(point);
        self.editing.pressed_style = self.style_at(point);
        self.editing.pressing_controls = true;
        true
    }
    fn release_control(&mut self, point: Point) -> (bool, Option<ToolbarIcon>) {
        let pressed = std::mem::take(&mut self.editing.pressing_controls);
        let tool = self.editing.pressed_tool.take();
        let style = self.editing.pressed_style.take();
        if !pressed {
            return (false, None);
        }
        if let Some(tool) = tool.filter(|tool| self.tool_at(point) == Some(*tool)) {
            if tool.annotation() {
                self.editing.tool = (self.editing.tool != Some(tool)).then_some(tool);
            } else if tool == ToolbarIcon::Undo {
                self.cancel_pointer();
                self.editing.scene.undo();
            } else {
                return (true, Some(tool));
            }
        } else if let Some(index) = style.filter(|index| self.style_at(point) == Some(*index)) {
            self.apply_style(index);
        }
        (true, None)
    }
    fn cancel_pointer(&mut self) {
        self.origin = None;
        self.editing.drag = None;
        if self.editing.mark_drag.take().is_some() {
            self.editing.scene.cancel_edit();
        }
        self.editing.points.clear();
        self.editing.pressed_tool = None;
        self.editing.pressed_style = None;
        self.editing.pressing_controls = false;
    }
    fn smart_hover(&mut self, hwnd: HWND) -> bool {
        if !self.smart
            || self.origin.is_some()
            || self.editing.phase != CapturePhase::Selecting
            || self.editing.probe_point == Some(self.cursor)
        {
            return false;
        }
        let previous = self.selection;
        let point = self.cursor;
        self.editing.probe_point = Some(point);
        self.generation += 1;
        let Some(window) = self
            .snapshots
            .iter()
            .find(|w| w.bounds.contains(point))
            .cloned()
        else {
            self.editing.probe_window = None;
            self.selection = None;
            self.hierarchy.clear();
            return previous != self.selection;
        };
        let switched = self.editing.probe_window != Some(window.hwnd);
        self.editing.probe_window = Some(window.hwnd);
        // Keep the last resolved element illuminated until this point's probe completes.
        if switched || self.selection.is_none() {
            self.set_hierarchy(vec![window.bounds]);
        }
        self.probe.submit(ProbeRequest {
            hwnd: window.hwnd,
            point,
            bounds: window.bounds,
            generation: self.generation,
            wake: hwnd.0 as isize,
        });
        previous != self.selection
    }
    fn apply_probe_result(&mut self, result: ProbeResult) -> bool {
        if result.generation != self.generation
            || result.point != self.cursor
            || self.editing.probe_window != Some(result.hwnd)
            || self.origin.is_some()
            || self.editing.phase != CapturePhase::Selecting
        {
            return false;
        }
        if !result.valid {
            self.snapshots.retain(|window| window.hwnd != result.hwnd);
            let changed = self.selection.take().is_some();
            self.hierarchy.clear();
            self.level = 0;
            self.editing.probe_point = None;
            self.editing.probe_window = None;
            return changed;
        }
        if !self
            .snapshots
            .iter()
            .any(|window| window.hwnd == result.hwnd)
        {
            return false;
        }
        let previous = self.selection;
        self.set_hierarchy(result.rectangles);
        previous != self.selection
    }
    fn set_hierarchy(&mut self, mut candidates: Vec<Rect>) {
        candidates.retain(|r| r.width >= 12. && r.height >= 12. && r.contains(self.cursor));
        candidates.dedup();
        if let Some(window) = self
            .snapshots
            .iter()
            .find(|window| Some(window.hwnd) == self.editing.probe_window)
            && candidates.last() != Some(&window.bounds)
        {
            candidates.push(window.bounds);
        }
        self.hierarchy = candidates;
        self.level = 0;
        self.selection = self.hierarchy.first().copied();
    }
    fn paint_controls(&self, image: &mut CapturedImage, monitor: Rect) -> Result<()> {
        let Some(rect) = self.selection else {
            return Ok(());
        };
        let scale = self.scale_at(Point {
            x: monitor.x,
            y: monitor.y,
        });
        let local = ui_rect(
            rect.x - monitor.x,
            rect.y - monitor.y,
            rect.width,
            rect.height,
        );
        let editing = self.editing.phase == CapturePhase::Editing;
        let stroke = (if editing { 2.5 } else { 3. }) * scale;
        let half = stroke / 2.;
        for edge in [
            ui_rect(local.x - half, local.y - half, local.width + stroke, stroke),
            ui_rect(
                local.x - half,
                local.y + local.height - half,
                local.width + stroke,
                stroke,
            ),
            ui_rect(
                local.x - half,
                local.y + half,
                stroke,
                (local.height - stroke).max(0.),
            ),
            ui_rect(
                local.x + local.width - half,
                local.y + half,
                stroke,
                (local.height - stroke).max(0.),
            ),
        ] {
            fill_ui_rect(image, edge, 0xff1677ff);
        }
        if editing {
            let diameter = (14. * scale).ceil() as u32;
            let center = f64::from(diameter) / 2.;
            let mut handle = UiCanvas::new(diameter, diameter)?;
            handle.circle(center, center, 5. * scale, 0xff1677ff, None);
            handle.circle(
                center,
                center,
                5. * scale,
                0xffffffff,
                Some(1.5 * scale as f32),
            );
            let handle = handle.image();
            for point in self.handles() {
                composite_image(
                    image,
                    &handle,
                    (point.x - monitor.x - center).round() as i32,
                    (point.y - monitor.y - center).round() as i32,
                );
            }
        }
        if let Some(editor) = &self.editing.text
            && let Some(mark) = self.editing.scene.mark(editor.mark_id)
            && let MarkKind::Text { position, .. } = &mark.kind
        {
            let x = local.x + position.x + 3.;
            let y = local.y + position.y + 3.;
            fill_ui_rect(image, ui_rect(x, y, 1.5, mark.style.font.size), 0xff1677ff);
        }
        let label = format!(
            "{},{}  {} × {} px",
            rect.x as i32, rect.y as i32, rect.width as u32, rect.height as u32
        );
        let text = render_ui_text(&label, 15.5 * scale, true, 0xffffffff)?;
        let width = f64::from(text.width()) + 24. * scale;
        let height = (f64::from(text.height()) + 14. * scale).max(36. * scale);
        let x = local.x.clamp(
            4. * scale,
            (monitor.width - width - 4. * scale).max(4. * scale),
        );
        let y = if local.y >= height + 8. * scale {
            local.y - height - 6. * scale
        } else {
            local.y + 10. * scale
        };
        if rect.intersection(monitor).is_some()
            && let Some(bounds) = local_canvas_bounds(
                &[ui_rect(x, y, width, height)],
                ui_rect(0., 0., monitor.width, monitor.height),
                scale,
            )
        {
            let mut badge = UiCanvas::new(bounds.width as u32, bounds.height as u32)?;
            let capsule = ui_rect(x - bounds.x, y - bounds.y, width, height);
            badge.rounded(capsule, height / 2., 0xf218191c, None);
            badge.rounded(capsule, height / 2., 0xff383c47, Some(scale as f32));
            badge.centered(&text, capsule);
            composite_image(image, &badge.image(), bounds.x as i32, bounds.y as i32);
        }
        if !self.passive
            && let Some((bar, properties)) = self.toolbar()
            && bar.intersection(monitor).is_some()
        {
            let scale = self.ui_scale();
            let tooltip = if let Some(tool) = self.tool_at(self.cursor) {
                let text = render_ui_text(tool.title(), 16. * scale, true, 0xffffffff)?;
                let bounds = tooltip_bounds(
                    &text,
                    Point {
                        x: self.cursor.x,
                        y: bar.y,
                    },
                    scale,
                    monitor,
                );
                Some((text, bounds))
            } else {
                None
            };
            let mut parts = vec![bar];
            if self.styles_visible() {
                parts.push(properties);
            }
            if let Some((_, bounds)) = &tooltip {
                parts.push(*bounds);
            }
            let bounds = local_canvas_bounds(&parts, monitor, 6. * scale)
                .context("Capture toolbar is outside its monitor")?;
            let mut ui = UiCanvas::new(bounds.width as u32, bounds.height as u32)?;
            let offset = |r: Rect| ui_rect(r.x - bounds.x, r.y - bounds.y, r.width, r.height);
            ui.panel(offset(bar));
            for (index, tool) in CAPTURE_TOOLS.iter().enumerate() {
                let cell = self.tool_rect(bar, index);
                let selected = self.editing.tool == Some(*tool)
                    || (*tool == ToolbarIcon::Rectangle
                        && self.editing.tool == Some(ToolbarIcon::Rounded));
                let pressed =
                    self.editing.pressed_tool == Some(*tool) && cell.contains(self.cursor);
                if selected || cell.contains(self.cursor) {
                    ui.rounded(
                        offset(cell),
                        6. * scale,
                        if pressed {
                            0xffcbd5e1
                        } else if selected {
                            0xffdbeafe
                        } else {
                            0xffe2e8f0
                        },
                        None,
                    );
                }
                ui.icon_scaled(
                    *tool,
                    cell.x - bounds.x + 8. * scale,
                    cell.y - bounds.y + 8. * scale,
                    scale,
                    if selected || *tool == ToolbarIcon::Copy {
                        0xff1677ff
                    } else {
                        0xff334155
                    },
                );
            }
            if self.styles_visible() {
                ui.panel(offset(properties));
                for index in 0..16 {
                    let cell = offset(self.style_rect(properties, index));
                    let (x, y) = (cell.x, cell.y);
                    let pressed = self.editing.pressed_style == Some(index)
                        && self.style_at(self.cursor) == Some(index);
                    if self.style_at(self.cursor) == Some(index) {
                        ui.rounded(
                            cell,
                            5. * scale,
                            if self.editing.pressed_style == Some(index) {
                                0xffcbd5e1
                            } else {
                                0xffe2e8f0
                            },
                            None,
                        );
                    }
                    if index < 4 {
                        let tool = [
                            ToolbarIcon::Rectangle,
                            ToolbarIcon::Rounded,
                            ToolbarIcon::Circle,
                            ToolbarIcon::Fill,
                        ][index];
                        let active = if index == 3 {
                            self.editing.style.fill
                        } else {
                            self.editing.tool == Some(tool)
                        };
                        if active && !pressed {
                            ui.rounded(cell, 5. * scale, 0xffdbeafe, None);
                        }
                        ui.icon_scaled(
                            tool,
                            x + 4. * scale,
                            y + 4. * scale,
                            scale,
                            if active { 0xff1677ff } else { 0xff334155 },
                        );
                    } else if index < 9 {
                        let width = WIDTHS[index - 4];
                        if width == self.editing.style.width && !pressed {
                            ui.rounded(cell, 5. * scale, 0xffdbeafe, None);
                        }
                        ui.centered(
                            &render_ui_text(
                                &format!("{}", width as u32),
                                15. * scale,
                                true,
                                0xff334155,
                            )?,
                            cell,
                        );
                    } else {
                        let color = COLORS[index - 9];
                        ui.circle(x + 16. * scale, y + 16. * scale, 10. * scale, color, None);
                        ui.circle(
                            x + 16. * scale,
                            y + 16. * scale,
                            10. * scale,
                            0xff94a3b8,
                            Some(0.8 * scale as f32),
                        );
                        if color == self.editing.style.argb {
                            ui.circle(
                                x + 16. * scale,
                                y + 16. * scale,
                                13.5 * scale,
                                0xff1677ff,
                                Some(2. * scale as f32),
                            );
                        }
                    }
                }
            }
            if let Some((text, rect)) = tooltip {
                ui.tooltip_box(&text, offset(rect), scale);
            }
            composite_image(
                image,
                &ui.image(),
                (bounds.x - monitor.x) as i32,
                (bounds.y - monitor.y) as i32,
            );
        }
        Ok(())
    }
}
fn prepare_frame<'a>(
    scratch: &'a mut Option<CapturedImage>,
    background: &CapturedImage,
) -> &'a mut CapturedImage {
    if let Some(frame) = scratch {
        frame.pixels.copy_from_slice(&background.pixels);
        frame
    } else {
        scratch.insert(background.clone())
    }
}

impl OverlaySurface {
    fn paint(&self) -> Result<()> {
        let state = self.state.borrow();
        let (monitor, source) = &state.monitors[self.monitor];
        let mut masked = self.masked.borrow_mut();
        if masked
            .as_ref()
            .is_none_or(|(color, _)| *color != state.mask)
        {
            let mut image = source.as_ref().clone();
            let alpha = (state.mask >> 24) as u16;
            for pixel in image.pixels.as_chunks_mut::<4>().0 {
                for (channel, shift) in pixel[..3].iter_mut().zip([0, 8, 16]) {
                    *channel = ((*channel as u16 * (255 - alpha)
                        + ((state.mask >> shift) & 255) as u16 * alpha)
                        / 255) as u8;
                }
            }
            *masked = Some((state.mask, image));
        }
        let mut scratch = self.scratch.borrow_mut();
        let image = prepare_frame(&mut scratch, &masked.as_ref().unwrap().1);
        drop(masked);
        if let Some(rect) = state.selection
            && let Some(overlap) = rect.intersection(monitor.bounds)
        {
            let x0 = (overlap.x - monitor.bounds.x).max(0.) as usize;
            let y0 = (overlap.y - monitor.bounds.y).max(0.) as usize;
            let x1 = ((overlap.x + overlap.width - monitor.bounds.x).ceil() as usize)
                .min(image.width as usize);
            let y1 = ((overlap.y + overlap.height - monitor.bounds.y).ceil() as usize)
                .min(image.height as usize);
            let shaped = matches!(
                state.shape,
                SelectionShape::Rounded { .. } | SelectionShape::Polygon(_)
            );
            for y in y0..y1 {
                if !shaped {
                    let start = (y * image.width as usize + x0) * 4;
                    let end = (y * image.width as usize + x1) * 4;
                    image.pixels[start..end].copy_from_slice(&source.pixels[start..end]);
                } else {
                    for x in x0..x1 {
                        if state.shape.includes(
                            Point {
                                x: monitor.bounds.x + x as f64,
                                y: monitor.bounds.y + y as f64,
                            },
                            rect,
                        ) {
                            let i = (y * image.width as usize + x) * 4;
                            image.pixels[i..i + 4].copy_from_slice(&source.pixels[i..i + 4]);
                        }
                    }
                }
            }
        }
        if state.editing.phase == CapturePhase::Editing
            && (!state.editing.scene.marks().is_empty() || !state.editing.points.is_empty())
        {
            let (selected, rect) = state.selection_image(true)?;
            composite_image(
                image,
                &selected,
                (rect.x - monitor.bounds.x) as i32,
                (rect.y - monitor.bounds.y) as i32,
            );
        }
        if !state.passive
            && state.editing.phase == CapturePhase::Selecting
            && monitor.bounds.contains(state.cursor)
        {
            let cx = (state.cursor.x - monitor.bounds.x) as i32;
            let cy = (state.cursor.y - monitor.bounds.y) as i32;
            let scale = (f64::from(monitor.dpi) / 96.).max(1.25);
            let zoom = (10. * scale).round() as i32;
            let extent = 11 * zoom;
            let gap = (24. * scale).round() as i32;
            let left = (cx + gap).min(image.width as i32 - extent - 4).max(0);
            let top = (cy + gap).min(image.height as i32 - extent - 4).max(0);
            for y in 0..extent {
                for x in 0..extent {
                    let sx = (cx + x / zoom - 5).clamp(0, source.width as i32 - 1);
                    let sy = (cy + y / zoom - 5).clamp(0, source.height as i32 - 1);
                    let dx = left + x;
                    let dy = top + y;
                    if dx >= image.width as i32 || dy >= image.height as i32 {
                        continue;
                    }
                    let src = (sy as usize * source.width as usize + sx as usize) * 4;
                    let dst = (dy as usize * image.width as usize + dx as usize) * 4;
                    image.pixels[dst..dst + 4].copy_from_slice(&source.pixels[src..src + 4]);
                    if x % zoom == 0 || y % zoom == 0 {
                        image.pixels[dst..dst + 3].fill(80);
                    }
                    if ((5 * zoom..6 * zoom).contains(&x) && (y == 5 * zoom || y == 6 * zoom - 1))
                        || ((5 * zoom..6 * zoom).contains(&y)
                            && (x == 5 * zoom || x == 6 * zoom - 1))
                    {
                        image.pixels[dst..dst + 3].copy_from_slice(&[0, 0, 255]);
                    }
                }
            }
            let rgb = sample_color(source, monitor.bounds, state.cursor)
                .context("Capture pointer is outside the monitor image")?;
            let color = format_color(rgb, COLOR_FORMATS[state.color_format]);
            let label = format!("{}  ({})", color, format_coordinate(state.cursor));
            draw_pixel_label(image, &label, left, top + extent + 2, scale)?;
        }
        state.paint_controls(image, monitor.bounds)?;
        self.target.borrow_mut().draw(image)
    }
}

fn hide_capture_windows(windows: &[isize], wake: Option<isize>) {
    // ShowWindow synchronously re-enters overlay_proc. Call only after releasing OverlayState.
    for &window in windows {
        unsafe {
            let _ = ShowWindow(HWND(window as *mut _), SW_HIDE);
        }
    }
    if let Some(wake) = wake {
        orbment_ui::wake(HWND(wake as *mut _));
    }
}

fn keyboard_selection(
    rect: Rect,
    key: u16,
    resize: bool,
    shift: bool,
    desktop: Rect,
) -> Option<Rect> {
    let step = if shift { 10. } else { 1. };
    let (dx, dy) = match key {
        0x25 => (-step, 0.),
        0x26 => (0., -step),
        0x27 => (step, 0.),
        0x28 => (0., step),
        _ => return None,
    };
    let mut rect = rect.intersection(desktop)?;
    if resize {
        rect.width = (rect.width + dx).clamp(1., desktop.x + desktop.width - rect.x);
        rect.height = (rect.height + dy).clamp(1., desktop.y + desktop.height - rect.y);
    } else {
        rect.x = (rect.x + dx).clamp(desktop.x, desktop.x + desktop.width - rect.width);
        rect.y = (rect.y + dy).clamp(desktop.y, desktop.y + desktop.height - rect.height);
    }
    Some(rect)
}

fn sample_color(image: &CapturedImage, bounds: Rect, point: Point) -> Option<[u8; 3]> {
    if !bounds.contains(point) {
        return None;
    }
    let x = (point.x - bounds.x).floor() as u32;
    let y = (point.y - bounds.y).floor() as u32;
    if x >= image.width || y >= image.height {
        return None;
    }
    let offset = (y as usize * image.width as usize + x as usize) * 4;
    let pixel = image.pixels.get(offset..offset + 4)?;
    Some([pixel[2], pixel[1], pixel[0]])
}

fn format_coordinate(point: Point) -> String {
    format!("{}, {}", point.x as i32, point.y as i32)
}

fn resized_selection(rect: Rect, handle: usize, point: Point, desktop: Rect) -> Rect {
    let (mut left, mut top, mut right, mut bottom) =
        (rect.x, rect.y, rect.x + rect.width, rect.y + rect.height);
    let x = point.x.clamp(desktop.x, desktop.x + desktop.width);
    let y = point.y.clamp(desktop.y, desktop.y + desktop.height);
    if matches!(handle, 0 | 6 | 7) {
        left = x.min(right - 1.);
    }
    if matches!(handle, 2..=4) {
        right = x.max(left + 1.);
    }
    if matches!(handle, 0..=2) {
        top = y.min(bottom - 1.);
    }
    if matches!(handle, 4..=6) {
        bottom = y.max(top + 1.);
    }
    ui_rect(left, top, right - left, bottom - top)
}

fn screen_pointer(hwnd: HWND, lparam: LPARAM) -> Result<Point> {
    let mut point = POINT {
        x: lparam.0 as u16 as i16 as i32,
        y: (lparam.0 >> 16) as u16 as i16 as i32,
    };
    ensure!(
        unsafe { ClientToScreen(hwnd, &mut point) }.as_bool(),
        "Cannot locate capture pointer"
    );
    Ok(Point {
        x: f64::from(point.x),
        y: f64::from(point.y),
    })
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const OverlaySurface;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let surface = &*ptr;
        let shared = surface.state.clone();
        let monitor_index = surface.monitor;
        let Ok(state) = shared.try_borrow() else {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        };
        let passive = state.passive;
        let finished = state.finished;
        drop(state);
        if finished || (passive && !matches!(message, WM_PAINT | WM_ERASEBKGND)) {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_ERASEBKGND {
            return LRESULT(1);
        }
        if message == WM_PAINT {
            let mut paint = PAINTSTRUCT::default();
            BeginPaint(hwnd, &mut paint);
            let result = surface.paint();
            let _ = EndPaint(hwnd, &paint);
            if let Err(error) = result {
                let mut state = shared.borrow_mut();
                state.events.push(CaptureEvent::Error(error.to_string()));
                let wake = state.wake;
                drop(state);
                hide_capture_windows(&[], wake);
            }
            return LRESULT(0);
        }
        let Ok(mut state) = shared.try_borrow_mut() else {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        };
        let mut complete = None;
        let mut copy_text = None;
        let mut cancel = false;
        let mut release_capture = false;
        let result = (|| -> Result<bool> {
            if let Some(editor) = &mut state.editing.text {
                let id = editor.mark_id;
                match editor.handle_message(hwnd, message, wparam, lparam) {
                    EditResult::Changed => {
                        state.redraw();
                        return Ok(true);
                    }
                    EditResult::Commit(comment) => {
                        state.editing.scene.apply_comment(id, comment);
                        state.editing.scene.commit_edit();
                        state.editing.text = None;
                        state.redraw();
                        return Ok(true);
                    }
                    EditResult::Cancel => {
                        state.editing.scene.cancel_edit();
                        state.editing.text = None;
                        state.redraw();
                        return Ok(true);
                    }
                    EditResult::Unhandled => {}
                }
            }
            match message {
                WM_TIMER if wparam.0 == 1 => {
                    if state.editing.phase == CapturePhase::Selecting
                        && state.origin.is_none()
                        && state.editing.probe_point != Some(state.cursor)
                        && state.smart_hover(hwnd)
                    {
                        state.redraw();
                    }
                }
                WM_SETCURSOR => {
                    let cursor = if state.editing.phase == CapturePhase::Selecting {
                        IDC_CROSS
                    } else if state.tool_at(state.cursor).is_some()
                        || state.style_at(state.cursor).is_some()
                    {
                        IDC_HAND
                    } else if let Some(cursor) = state.mark_cursor(state.cursor) {
                        cursor
                    } else if let Some(handle) = state.handle_at(state.cursor) {
                        match handle {
                            0 | 4 => IDC_SIZENWSE,
                            2 | 6 => IDC_SIZENESW,
                            1 | 5 => IDC_SIZENS,
                            _ => IDC_SIZEWE,
                        }
                    } else if state.selection.is_some_and(|r| r.contains(state.cursor)) {
                        match state.editing.tool {
                            Some(ToolbarIcon::Text) => IDC_IBEAM,
                            Some(_) => IDC_CROSS,
                            None => IDC_SIZEALL,
                        }
                    } else {
                        IDC_ARROW
                    };
                    SetCursor(Some(LoadCursorW(None, cursor)?));
                }
                WM_MOUSEMOVE => {
                    let mut cursor = POINT::default();
                    GetCursorPos(&mut cursor)?;
                    let point = Point {
                        x: cursor.x as f64,
                        y: cursor.y as f64,
                    };
                    if point == state.cursor {
                        return Ok(true);
                    }
                    state.cursor = point;
                    if state.editing.phase == CapturePhase::Selecting {
                        if let Some(origin) = state.origin
                            && (point.x - origin.x).hypot(point.y - origin.y) > 3.
                        {
                            state.selection = Some(state.shape.constrain(origin, point));
                            if let SelectionShape::Polygon(points) = &mut state.shape {
                                points.push(point);
                            }
                        }
                    } else if state.editing.mark_drag.is_some() {
                        state.drag_mark(point);
                    } else if let Some(drag) = state.editing.drag {
                        let desktop = virtual_desktop();
                        state.selection = Some(match drag {
                            SelectionDrag::Move { start, rect } => {
                                let x = (rect.x + point.x - start.x).clamp(
                                    desktop.x,
                                    (desktop.x + desktop.width - rect.width).max(desktop.x),
                                );
                                let y = (rect.y + point.y - start.y).clamp(
                                    desktop.y,
                                    (desktop.y + desktop.height - rect.height).max(desktop.y),
                                );
                                if let Some(previous) = state.selection
                                    && let SelectionShape::Polygon(points) = &mut state.shape
                                {
                                    for p in points {
                                        p.x += x - previous.x;
                                        p.y += y - previous.y;
                                    }
                                }
                                ui_rect(x, y, rect.width, rect.height)
                            }
                            SelectionDrag::Resize { handle, rect } => {
                                if matches!(state.shape, SelectionShape::Polygon(_)) {
                                    state.shape = SelectionShape::Rectangle;
                                }
                                resized_selection(rect, handle, point, desktop)
                            }
                        });
                    } else if !state.editing.points.is_empty() {
                        let local = state.local_point(point);
                        state.editing.points.push(local);
                    }
                    state.redraw();
                }
                PROBE_MESSAGE => {
                    let mut changed = false;
                    while let Ok(result) = state.probe.results.try_recv() {
                        changed |= state.apply_probe_result(result);
                    }
                    if changed {
                        state.redraw();
                    }
                }
                WM_LBUTTONDOWN => {
                    state.cursor = screen_pointer(hwnd, lparam)?;
                    let point = state.cursor;
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(Some(hwnd));
                    if state.editing.phase == CapturePhase::Selecting {
                        state.smart_hover(hwnd);
                        state.origin = Some(point);
                        if let SelectionShape::Polygon(points) = &mut state.shape {
                            points.clear();
                            points.push(point);
                        }
                        SetCapture(hwnd);
                    } else {
                        state.commit_text();
                        if state.press_control(point) || state.begin_mark_drag(point) {
                            SetCapture(hwnd);
                        } else if let Some(rect) = state.selection {
                            if let Some(handle) = state.handle_at(point) {
                                state.editing.drag = Some(SelectionDrag::Resize { handle, rect });
                                SetCapture(hwnd);
                            } else if rect.contains(point) {
                                let local = state.local_point(point);
                                match state.editing.tool {
                                    Some(ToolbarIcon::Serial) => {
                                        let style = state.editing.style.clone();
                                        state.editing.scene.add_sequence(
                                            local,
                                            style.font.size.max(12.),
                                            style,
                                        );
                                    }
                                    Some(ToolbarIcon::Text) => {
                                        let style = state.editing.style.clone();
                                        state.editing.scene.begin_edit();
                                        let id = state.editing.scene.add(
                                            MarkKind::Text {
                                                position: local,
                                                text: String::new(),
                                                angle: 0.,
                                            },
                                            style.clone(),
                                        );
                                        let editor = ArrowCommentEditor::new(
                                            id,
                                            ArrowComment {
                                                font: style.font,
                                                ..Default::default()
                                            },
                                        );
                                        let monitor = state.monitors[monitor_index].0.bounds;
                                        editor.reposition(
                                            hwnd,
                                            &[point],
                                            Point {
                                                x: monitor.x,
                                                y: monitor.y,
                                            },
                                            1.,
                                        );
                                        state.editing.text = Some(editor);
                                    }
                                    Some(_) => {
                                        state.editing.points = vec![local];
                                        SetCapture(hwnd);
                                    }
                                    None => {
                                        state.editing.drag =
                                            Some(SelectionDrag::Move { start: point, rect });
                                        SetCapture(hwnd);
                                    }
                                }
                            }
                        }
                    }
                    state.redraw();
                }
                WM_LBUTTONUP => {
                    release_capture = true;
                    state.cursor = screen_pointer(hwnd, lparam)?;
                    let point = state.cursor;
                    let (control_press, tool) = state.release_control(point);
                    if let Some(tool) = tool {
                        cancel = tool == ToolbarIcon::Cancel;
                        complete = tool.action();
                    }
                    if control_press {
                        state.redraw();
                        return Ok(true);
                    }
                    if state.editing.phase == CapturePhase::Selecting {
                        if let Some(origin) = state.origin.take() {
                            if (origin.x - state.cursor.x).hypot(origin.y - state.cursor.y) > 3. {
                                state.selection = Some(state.shape.constrain(origin, state.cursor));
                            }
                            if let SelectionShape::Polygon(points) = &state.shape
                                && let Some(first) = points.first().copied()
                                && points.len() > 2
                            {
                                let (mut min, mut max) = (first, first);
                                for p in points {
                                    min.x = min.x.min(p.x);
                                    min.y = min.y.min(p.y);
                                    max.x = max.x.max(p.x);
                                    max.y = max.y.max(p.y);
                                }
                                state.selection = Some(Rect::from_points(min, max));
                            }
                            state.enter_editing(virtual_desktop());
                        }
                    } else {
                        state.drag_mark(point);
                        state.commit_mark_drag();
                        state.editing.drag = None;
                        state.commit_mark();
                    }
                    state.redraw();
                }
                WM_CANCELMODE | WM_CAPTURECHANGED => {
                    state.cancel_pointer();
                    release_capture = message == WM_CANCELMODE;
                    state.redraw();
                }
                WM_LBUTTONDBLCLK => {
                    state.cursor = screen_pointer(hwnd, lparam)?;
                    let point = state.cursor;
                    if state.press_control(point) {
                        SetCapture(hwnd);
                        state.redraw();
                    } else if state.editing.phase == CapturePhase::Editing
                        && state.selection.is_some_and(|r| r.contains(state.cursor))
                    {
                        if state.edit_text_at(point) {
                            if let Some(editor) = &state.editing.text {
                                let monitor = state.monitors[monitor_index].0.bounds;
                                editor.reposition(
                                    hwnd,
                                    &[point],
                                    Point {
                                        x: monitor.x,
                                        y: monitor.y,
                                    },
                                    1.,
                                );
                            }
                            state.redraw();
                        } else {
                            complete = Some(CaptureAction::Copy);
                        }
                    }
                }
                WM_MOUSEWHEEL if state.editing.phase == CapturePhase::Selecting => {
                    let n = state.hierarchy.len();
                    if n > 0 && state.origin.is_none() {
                        let backwards = ((wparam.0 >> 16) as u16 as i16) < 0;
                        state.level = if backwards {
                            (state.level + n - 1) % n
                        } else {
                            (state.level + 1) % n
                        };
                        state.selection = Some(state.hierarchy[state.level]);
                        state.redraw();
                    }
                }
                WM_KEYDOWN => {
                    let control = GetKeyState(VK_CONTROL.0 as i32) < 0;
                    let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                    match state.key_down(
                        wparam.0 as u16,
                        control,
                        shift,
                        virtual_desktop(),
                        &SELECTION_HISTORY,
                    ) {
                        Some(CaptureKey::Cancel) => cancel = true,
                        Some(CaptureKey::Image(action)) => complete = Some(action),
                        Some(CaptureKey::Text(kind)) => {
                            let mut point = POINT::default();
                            GetCursorPos(&mut point)?;
                            state.cursor = Point {
                                x: point.x as f64,
                                y: point.y as f64,
                            };
                            copy_text = Some(kind);
                        }
                        Some(CaptureKey::Handled) => state.redraw(),
                        None => return Ok(false),
                    }
                }
                WM_RBUTTONUP => {
                    if state.editing.phase == CapturePhase::Selecting
                        || !state.selection.is_some_and(|r| r.contains(state.cursor))
                    {
                        cancel = true;
                    } else {
                        state.commit_text();
                        state.cancel_pointer();
                        release_capture = true;
                        state.editing.scene.clear_selection();
                        state.editing.tool = None;
                        state.redraw();
                    }
                }
                WM_CLOSE => cancel = true,
                WM_DPICHANGED => {}
                _ => return Ok(false),
            }
            Ok(true)
        })();
        let mut windows = Vec::new();
        if cancel {
            release_capture = true;
            windows = state.cancel();
        } else if let Some(kind) = copy_text {
            release_capture = true;
            match state.finish_text(kind) {
                Ok(handles) => windows = handles,
                Err(error) => state.events.push(CaptureEvent::Error(error.to_string())),
            }
        } else if let Some(action) = complete {
            release_capture = true;
            // Save is dispatched by the owner after it destroys the completed overlay.
            match state.finish(action) {
                Ok(handles) => windows = handles,
                Err(error) => state.events.push(CaptureEvent::Error(error.to_string())),
            }
        }
        let handled = match result {
            Ok(handled) => handled,
            Err(error) => {
                state.events.push(CaptureEvent::Error(error.to_string()));
                true
            }
        };
        let wake = if state.events.is_empty() {
            None
        } else {
            state.wake
        };
        drop(state);
        if release_capture && GetCapture() == hwnd {
            let _ = ReleaseCapture();
        }
        hide_capture_windows(&windows, wake);
        if handled {
            LRESULT(0)
        } else {
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
    }
}

/// RGB channel template tokens encode the component and format (`%ReX`, `%GrD`).
pub fn format_color(rgb: [u8; 3], template: &str) -> String {
    orbment_core::utilities::Rgb {
        r: rgb[0],
        g: rgb[1],
        b: rgb[2],
    }
    .format_template(template)
}

fn draw_pixel_label(
    image: &mut CapturedImage,
    label: &str,
    left: i32,
    top: i32,
    scale: f64,
) -> Result<()> {
    let text = render_ui_text(label, 15.5 * scale, true, 0xffffffff)?;
    let width = (f64::from(text.width()) + 24. * scale).ceil() as u32;
    let height = (f64::from(text.height()) + 14. * scale)
        .max(36. * scale)
        .ceil() as u32;
    let left = left.clamp(0, image.width.saturating_sub(width) as i32);
    let top = top.clamp(0, image.height.saturating_sub(height) as i32);
    let mut ui = UiCanvas::new(width, height)?;
    let rect = ui_rect(
        0.5 * scale,
        0.5 * scale,
        f64::from(width) - scale,
        f64::from(height) - scale,
    );
    ui.rounded(rect, 8. * scale, 0xf218191c, None);
    ui.rounded(rect, 8. * scale, 0xff383c47, Some(scale as f32));
    ui.centered(&text, rect);
    composite_image(image, &ui.image(), left, top);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, collections::VecDeque};
    use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, WM_SHOWWINDOW};

    #[test]
    fn stream_transfers_each_new_frame_without_copying_pixels() {
        let stream = CaptureStream {
            frame: Arc::new(Mutex::new(None)),
            changed: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            error: Arc::new(Mutex::new(None)),
            wake: Arc::new(Mutex::new(None)),
        };
        assert!(stream.latest_timed().is_none());
        for timestamp in [123, 456] {
            let image = CapturedImage {
                width: 2,
                height: 1,
                pixels: vec![17; 8],
            };
            let pixels = image.pixels.as_ptr();
            *stream.frame.lock().unwrap() = Some((timestamp, image));
            stream.changed.store(true, Ordering::Release);

            let (received_at, received) = stream.latest_timed().unwrap();
            assert_eq!(received_at, timestamp);
            assert_eq!(received.pixels.as_ptr(), pixels);
            assert_eq!(received.pixels, [17; 8]);
            assert!(stream.frame.lock().unwrap().is_none());
            assert!(stream.latest().is_none());
        }
    }

    #[test]
    fn staging_map_retries_only_gpu_busy_with_one_blocking_attempt() {
        let mut flags = Vec::new();
        let result = map_staging_read(|flag| {
            flags.push(flag);
            if flag != 0 {
                Err(windows::core::Error::from_hresult(
                    DXGI_ERROR_WAS_STILL_DRAWING,
                ))
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(flags, [D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, 0]);

        flags.clear();
        let error = map_staging_read(|flag| {
            flags.push(flag);
            Err(windows::core::Error::from_hresult(
                DXGI_ERROR_WAS_STILL_DRAWING,
            ))
        })
        .unwrap_err();
        assert_eq!(error.code(), DXGI_ERROR_WAS_STILL_DRAWING);
        assert_eq!(flags, [D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, 0]);
    }

    #[test]
    fn staging_map_success_and_non_busy_errors_never_block_or_retry() {
        let mut flags = Vec::new();
        map_staging_read(|flag| {
            flags.push(flag);
            Ok(())
        })
        .unwrap();
        assert_eq!(flags, [D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32]);

        flags.clear();
        let error = map_staging_read(|flag| {
            flags.push(flag);
            Err(windows::core::Error::from_hresult(
                DXGI_ERROR_DEVICE_REMOVED,
            ))
        })
        .unwrap_err();
        assert_eq!(error.code(), DXGI_ERROR_DEVICE_REMOVED);
        assert_eq!(flags, [D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32]);
    }

    #[test]
    fn static_capture_releases_and_drops_every_queued_frame_before_waiting() -> Result<()> {
        struct Frame {
            id: u32,
            log: Rc<RefCell<Vec<(&'static str, u32)>>>,
        }
        impl Drop for Frame {
            fn drop(&mut self) {
                self.log.borrow_mut().push(("drop", self.id));
            }
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut queued: VecDeque<_> = (1..=3)
            .map(|id| Frame {
                id,
                log: log.clone(),
            })
            .collect();
        let image = drain_then_capture(
            &mut queued,
            |queued| {
                let frame = queued.pop_front();
                log.borrow_mut()
                    .push(("take", frame.as_ref().map_or(0, |frame| frame.id)));
                Ok(frame)
            },
            |frame| {
                log.borrow_mut().push(("close", frame.id));
                Ok(())
            },
            |queued| {
                assert!(queued.is_empty());
                log.borrow_mut().push(("start", 4));
                Ok(())
            },
            |queued| {
                assert!(queued.is_empty());
                log.borrow_mut().push(("wait", 4));
                Ok(4)
            },
        )?;
        assert_eq!(image, 4);
        assert_eq!(
            *log.borrow(),
            [
                ("take", 1),
                ("close", 1),
                ("drop", 1),
                ("take", 2),
                ("close", 2),
                ("drop", 2),
                ("take", 3),
                ("close", 3),
                ("drop", 3),
                ("take", 0),
                ("start", 4),
                ("wait", 4),
            ]
        );
        Ok(())
    }

    #[test]
    fn static_capture_starts_after_drain_even_when_frame_content_is_unchanged() -> Result<()> {
        for stale in [
            vec![],
            vec![(1, "unchanged pixels"), (2, "unchanged pixels")],
        ] {
            let stale_count = stale.len();
            let mut queued = VecDeque::from(stale);
            let released = Cell::new(0);
            let starts = Cell::new(0);
            let frame = drain_then_capture(
                &mut queued,
                |queued| Ok(queued.pop_front()),
                |_| {
                    released.set(released.get() + 1);
                    Ok(())
                },
                |queued| {
                    assert!(queued.is_empty());
                    starts.set(starts.get() + 1);
                    queued.push_back((3, "unchanged pixels"));
                    Ok(())
                },
                |queued| {
                    queued
                        .pop_front()
                        .context("Initial session frame was discarded")
                },
            )?;
            assert_eq!(frame, (3, "unchanged pixels"));
            assert_eq!(released.get(), stale_count);
            assert_eq!(starts.get(), 1);
        }
        Ok(())
    }

    #[test]
    fn static_capture_drain_errors_prevent_waiting_for_a_new_frame() {
        let waited = Cell::new(false);
        let released = Cell::new(false);
        let started = Cell::new(false);
        let take_error = drain_then_capture(
            &mut (),
            |_| Err::<Option<()>, _>(anyhow!("Frame pool closed")),
            |_| {
                released.set(true);
                Ok(())
            },
            |_| {
                started.set(true);
                Ok(())
            },
            |_| {
                waited.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(take_error.to_string(), "Frame pool closed");
        assert!(!released.get());
        assert!(!started.get());
        assert!(!waited.get());

        let mut queued = VecDeque::from([1, 2]);
        let release_error = drain_then_capture(
            &mut queued,
            |queued| Ok(queued.pop_front()),
            |_| Err(anyhow!("Frame close failed")),
            |_| {
                started.set(true);
                Ok(())
            },
            |_| {
                waited.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(release_error.to_string(), "Frame close failed");
        assert_eq!(queued, VecDeque::from([2]));
        assert!(!started.get());
        assert!(!waited.get());

        let start_error = drain_then_capture(
            &mut (),
            |_| Ok(None::<()>),
            |_| Ok(()),
            |_| Err(anyhow!("Capture session start failed")),
            |_| {
                waited.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(start_error.to_string(), "Capture session start failed");
        assert!(!waited.get());
    }

    thread_local! {
        static HIDE_BORROWABLE: Cell<Option<bool>> = const { Cell::new(None) };
    }

    unsafe extern "system" fn test_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if message == WM_SHOWWINDOW && wparam.0 == 0 {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<OverlayState>;
                if let Some(state) = state.as_ref() {
                    HIDE_BORROWABLE.set(Some(state.try_borrow_mut().is_ok()));
                }
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }

    struct TestWindow(HWND);
    impl Drop for TestWindow {
        fn drop(&mut self) {
            unsafe {
                SetWindowLongPtrW(self.0, GWLP_USERDATA, 0);
                let _ = DestroyWindow(self.0);
            }
        }
    }

    fn test_state(rect: Rect) -> OverlayState {
        OverlayState {
            windows: Vec::new(),
            snapshots: Vec::new(),
            monitors: vec![(
                Monitor {
                    handle: 0,
                    bounds: rect,
                    dpi: 96,
                    name: String::new(),
                    hdr: false,
                },
                Arc::new(CapturedImage {
                    width: rect.width as u32,
                    height: rect.height as u32,
                    pixels: vec![17; (rect.width * rect.height) as usize * 4],
                }),
            )],
            selection: Some(rect),
            origin: None,
            cursor: Point::default(),
            shape: SelectionShape::default(),
            hierarchy: Vec::new(),
            level: 0,
            generation: 0,
            probe: UiaProbe {
                request: Arc::new((Mutex::new(None), std::sync::Condvar::new())),
                stop: Arc::new(AtomicBool::new(false)),
                results: mpsc::channel().1,
            },
            events: Vec::new(),
            finished: false,
            had_menu: false,
            wake: None,
            passive: false,
            mask: 0,
            color_format: 0,
            smart: false,
            editing: CaptureEditor::default(),
        }
    }

    fn center(rect: Rect) -> Point {
        Point {
            x: rect.x + rect.width / 2.,
            y: rect.y + rect.height / 2.,
        }
    }

    fn pointer_lparam(point: Point) -> LPARAM {
        LPARAM(((point.x as i32 as u16 as u32) | ((point.y as i32 as u16 as u32) << 16)) as isize)
    }

    #[test]
    fn mark_drag_uses_capture_coordinates_and_commits_one_undo_step() {
        let desktop = ui_rect(-200., -200., 400., 400.);
        let mut state = test_state(desktop);
        state.enter_editing(desktop);
        state.editing.tool = Some(ToolbarIcon::Pen);
        let style = MarkStyle {
            argb: COLORS[1],
            fill: true,
            width: 8.,
            ..Default::default()
        };
        let bounds = ui_rect(40., 40., 120., 80.);
        let id = state
            .editing
            .scene
            .add(MarkKind::Rectangle { bounds }, style.clone());
        state.editing.scene.clear_selection();
        let start = Point { x: -100., y: -120. };
        assert_eq!(state.mark_cursor(start), Some(IDC_SIZEALL));
        assert!(state.begin_mark_drag(start));
        assert_eq!(state.editing.style, style);
        assert_eq!(state.editing.tool, Some(ToolbarIcon::Pen));
        state.drag_mark(Point { x: -90., y: -100. });
        state.drag_mark(Point { x: -75., y: -90. });
        state.commit_mark_drag();
        state.commit_mark_drag();
        assert_eq!(
            state.editing.scene.mark(id).unwrap().bounds(),
            ui_rect(65., 70., 120., 80.)
        );
        assert_eq!(state.selection, Some(desktop));
        assert!(state.editing.scene.undo());
        assert_eq!(state.editing.scene.mark(id).unwrap().bounds(), bounds);
        assert!(state.editing.scene.undo());
        assert!(state.editing.scene.marks().is_empty());
    }

    #[test]
    fn selected_mark_handles_win_over_newer_marks_and_capture_edges() {
        let desktop = ui_rect(-100., -100., 300., 300.);
        let mut state = test_state(desktop);
        state.enter_editing(desktop);
        let bounds = ui_rect(0., 0., 80., 60.);
        let id = state.editing.scene.add(
            MarkKind::Rectangle { bounds },
            MarkStyle {
                fill: true,
                ..Default::default()
            },
        );
        state.editing.scene.add(
            MarkKind::Rectangle {
                bounds: ui_rect(0., 0., 20., 20.),
            },
            MarkStyle::default(),
        );
        assert_eq!(
            state.editing.scene.select(Point { x: 60., y: 40. }, 8.),
            Some(id)
        );
        let point = Point { x: -100., y: -100. };
        assert_eq!(state.handle_at(point), Some(0));
        assert_eq!(state.mark_cursor(point), Some(IDC_SIZENWSE));
        assert!(state.begin_mark_drag(point));
        assert_eq!(state.editing.scene.selected(), Some(id));
        state.drag_mark(Point { x: -90., y: -80. });
        state.commit_mark_drag();
        assert_eq!(
            state.editing.scene.mark(id).unwrap().bounds(),
            ui_rect(10., 20., 70., 40.)
        );
        assert_eq!(state.selection, Some(desktop));
        assert!(state.editing.scene.undo());
        assert_eq!(state.editing.scene.mark(id).unwrap().bounds(), bounds);
    }

    #[test]
    fn unselected_arrow_endpoint_resizes_and_cancel_restores_without_history() {
        let desktop = ui_rect(0., 0., 300., 300.);
        let mut state = test_state(desktop);
        state.enter_editing(desktop);
        let points = vec![Point { x: 30., y: 30. }, Point { x: 130., y: 60. }];
        let id = state.editing.scene.add(
            MarkKind::Arrow {
                points: points.clone(),
                comment: None,
            },
            MarkStyle::default(),
        );
        state.editing.scene.clear_selection();
        assert!(state.begin_mark_drag(points[1]));
        assert_eq!(state.mark_cursor(points[1]), Some(IDC_SIZEALL));
        state.drag_mark(Point { x: 160., y: 100. });
        assert_eq!(
            state.editing.scene.mark(id).unwrap().points().unwrap(),
            [points[0], Point { x: 160., y: 100. }]
        );
        state.cancel_pointer();
        assert!(state.editing.mark_drag.is_none());
        assert_eq!(
            state.editing.scene.mark(id).unwrap().points().unwrap(),
            points
        );
        assert!(state.editing.scene.undo());
        assert!(state.editing.scene.marks().is_empty());
    }

    #[test]
    fn blank_canvas_and_eraser_clear_object_selection_without_editing() {
        let desktop = ui_rect(0., 0., 300., 300.);
        let mut state = test_state(desktop);
        state.enter_editing(desktop);
        let bounds = ui_rect(20., 20., 80., 60.);
        let id = state
            .editing
            .scene
            .add(MarkKind::Rectangle { bounds }, MarkStyle::default());
        assert!(!state.begin_mark_drag(Point { x: 200., y: 200. }));
        assert!(state.editing.scene.selected().is_none());
        state.editing.scene.select(Point { x: 20., y: 20. }, 8.);
        state.editing.tool = Some(ToolbarIcon::Eraser);
        assert_eq!(state.mark_cursor(Point { x: 20., y: 20. }), None);
        assert!(!state.begin_mark_drag(Point { x: 20., y: 20. }));
        assert!(state.editing.scene.selected().is_none());
        assert!(state.editing.mark_drag.is_none());
        assert_eq!(state.editing.scene.mark(id).unwrap().bounds(), bounds);
    }

    #[test]
    fn selected_object_styles_are_visible_without_a_tool_and_undoable() {
        let desktop = ui_rect(0., 0., 1000., 700.);
        let mut state = test_state(desktop);
        state.selection = Some(ui_rect(100., 100., 400., 200.));
        state.enter_editing(desktop);
        let style = state.editing.style.clone();
        let id = state.editing.scene.add(
            MarkKind::Rectangle {
                bounds: ui_rect(40., 40., 120., 80.),
            },
            style.clone(),
        );
        assert_eq!(state.editing.tool, None);
        let (_, properties) = state.toolbar().unwrap();
        assert_eq!(
            state.style_at(center(state.style_rect(properties, 10))),
            Some(10)
        );
        state.apply_style(9);
        for index in [3, 7, 10] {
            state.apply_style(index);
            assert_eq!(
                state.editing.scene.mark(id).unwrap().style,
                state.editing.style
            );
        }
        let changed = &state.editing.scene.mark(id).unwrap().style;
        assert!(changed.fill);
        assert_eq!(changed.width, 12.);
        assert_eq!(changed.font.size, 12.);
        assert_eq!(changed.argb, COLORS[1]);
        for _ in 0..3 {
            assert!(state.editing.scene.undo());
        }
        assert_eq!(state.editing.scene.mark(id).unwrap().style, style);
        assert!(state.editing.scene.undo());
        assert!(state.editing.scene.marks().is_empty());
    }

    #[test]
    fn longshot_shortcut_requires_editing_without_modifiers_or_text_input() {
        let desktop = ui_rect(0., 0., 1000., 700.);
        let mut state = test_state(desktop);
        let history = SelectionHistory::default();
        assert!(
            state
                .key_down(0x4c, false, false, desktop, &history)
                .is_none()
        );
        state.enter_editing(desktop);
        assert!(matches!(
            state.key_down(0x4c, false, false, desktop, &history),
            Some(CaptureKey::Image(CaptureAction::Longshot))
        ));
        for (control, shift) in [(true, false), (false, true), (true, true)] {
            assert!(
                state
                    .key_down(0x4c, control, shift, desktop, &history)
                    .is_none()
            );
        }
        state.editing.scene.add(
            MarkKind::Text {
                position: Point { x: 20., y: 20. },
                text: "Label".into(),
                angle: 0.,
            },
            MarkStyle::default(),
        );
        assert!(state.edit_text_at(Point { x: 20., y: 20. }));
        assert!(
            state
                .key_down(0x4c, false, false, desktop, &history)
                .is_none()
        );
    }

    #[test]
    fn object_keyboard_edits_precede_capture_adjustments_and_are_undoable() {
        let desktop = ui_rect(-100., -100., 400., 400.);
        let mut state = test_state(desktop);
        state.enter_editing(desktop);
        let history = SelectionHistory::default();
        let bounds = ui_rect(20., 30., 80., 60.);
        let id = state
            .editing
            .scene
            .add(MarkKind::Rectangle { bounds }, MarkStyle::default());
        state.key_down(VK_RIGHT.0, false, false, desktop, &history);
        state.key_down(VK_DOWN.0, true, true, desktop, &history);
        assert_eq!(
            state.editing.scene.mark(id).unwrap().bounds(),
            ui_rect(21., 40., 80., 60.)
        );
        assert_eq!(state.selection, Some(desktop));
        for key in [VK_DELETE.0, VK_BACK.0] {
            state.key_down(key, false, false, desktop, &history);
            assert!(state.editing.scene.marks().is_empty());
            assert!(state.editing.scene.undo());
            assert_eq!(
                state.editing.scene.select(Point { x: 21., y: 40. }, 8.),
                Some(id)
            );
        }
        assert!(state.editing.scene.undo());
        assert!(state.editing.scene.undo());
        assert_eq!(state.editing.scene.mark(id).unwrap().bounds(), bounds);
    }

    #[test]
    fn rounded_annotations_are_single_objects_for_filled_and_outline_styles() {
        for fill in [false, true] {
            let mut state = test_state(ui_rect(0., 0., 300., 300.));
            state.editing.tool = Some(ToolbarIcon::Rounded);
            state.editing.style.fill = fill;
            state.editing.points = vec![Point { x: 10., y: 20. }, Point { x: 110., y: 80. }];
            state.commit_mark();
            assert_eq!(state.editing.scene.marks().len(), 1);
            let mark = state.editing.scene.selected_mark().unwrap();
            assert_eq!(
                mark.kind,
                MarkKind::RoundedRectangle {
                    bounds: ui_rect(10., 20., 100., 60.),
                    radius: 12.,
                }
            );
            assert_eq!(mark.style.fill, fill);
            assert!(state.editing.scene.undo());
            assert!(state.editing.scene.marks().is_empty());
        }
    }

    #[test]
    fn existing_text_and_arrow_comments_open_for_editing_without_new_marks() {
        let mut state = test_state(ui_rect(-100., -100., 300., 300.));
        let text = state.editing.scene.add(
            MarkKind::Text {
                position: Point { x: 20., y: 20. },
                text: "Label".into(),
                angle: 0.,
            },
            MarkStyle::default(),
        );
        let arrow = state.editing.scene.add(
            MarkKind::Arrow {
                points: vec![Point { x: 20., y: 150. }, Point { x: 180., y: 150. }],
                comment: None,
            },
            MarkStyle::default(),
        );
        assert!(state.edit_text_at(Point { x: -80., y: -80. }));
        let editor = state.editing.text.as_ref().unwrap();
        assert_eq!(editor.mark_id, text);
        assert_eq!(editor.preview().text, "Label");
        state.commit_text();
        assert!(state.edit_text_at(Point { x: 0., y: 50. }));
        assert_eq!(state.editing.text.as_ref().unwrap().mark_id, arrow);
        state.commit_text();
        assert_eq!(state.editing.scene.marks().len(), 2);
        assert!(!state.edit_text_at(Point { x: 180., y: 180. }));
    }

    fn test_tree(nodes: &[(Rect, Option<usize>, bool)]) -> WindowTree<()> {
        WindowTree {
            nodes: nodes
                .iter()
                .enumerate()
                .map(|(index, &(bounds, parent, structural))| CachedNode {
                    element: (),
                    bounds,
                    parent,
                    structural,
                    children: Some(
                        nodes
                            .iter()
                            .enumerate()
                            .filter_map(|(child, &(_, parent, _))| {
                                (parent == Some(index)).then_some(child)
                            })
                            .collect(),
                    ),
                })
                .collect(),
        }
    }

    #[test]
    fn later_overlay_branch_blocks_smaller_background_elements() {
        let bounds = ui_rect(-1000., -200., 800., 600.);
        let background = ui_rect(-1000., -200., 800., 550.);
        let small = ui_rect(-660., 60., 24., 24.);
        let overlay = ui_rect(-900., -100., 600., 400.);
        let image = ui_rect(-800., 0., 400., 200.);
        let mut tree = test_tree(&[
            (bounds, None, false),
            (background, Some(0), true),
            (overlay, Some(0), true),
            (small, Some(1), false),
            (image, Some(2), false),
        ]);
        for point in [Point { x: -650., y: 70. }, Point { x: -620., y: 90. }] {
            let mut visited = Vec::new();
            assert_eq!(
                tree.query(point, |_, current| {
                    visited.push(current);
                    true
                }),
                vec![image, overlay, bounds],
            );
            assert_eq!(visited, vec![0, 2, 4]);
        }
    }

    #[test]
    fn full_size_structural_layers_backtrack_to_real_content() {
        let bounds = ui_rect(0., 0., 800., 600.);
        let content = ui_rect(100., 100., 500., 350.);
        let button = ui_rect(200., 200., 80., 40.);
        let outside = ui_rect(700., 500., 50., 50.);
        let mut tree = test_tree(&[
            (bounds, None, false),
            (content, Some(0), true),
            (bounds, Some(0), true),
            (bounds, Some(0), true),
            (button, Some(1), false),
            (bounds, Some(3), true),
            (outside, Some(5), false),
        ]);
        let mut visited = Vec::new();
        assert_eq!(
            tree.query(center(button), |_, current| {
                visited.push(current);
                true
            }),
            vec![button, content, bounds],
        );
        assert_eq!(visited, vec![0, 3, 5, 2, 1, 4]);
    }

    #[test]
    fn independent_overlay_and_full_size_controls_never_expose_earlier_siblings() {
        let bounds = ui_rect(0., 0., 800., 600.);
        let small = ui_rect(200., 200., 24., 24.);
        let overlay = ui_rect(100., 100., 500., 350.);
        for (front, structural) in [(overlay, true), (bounds, false)] {
            let mut tree = test_tree(&[
                (bounds, None, false),
                (small, Some(0), false),
                (front, Some(0), structural),
                (front, Some(2), true),
            ]);
            let mut visited = Vec::new();
            let expected = if front == bounds {
                vec![bounds]
            } else {
                vec![front, bounds]
            };
            assert_eq!(
                tree.query(center(small), |_, current| {
                    visited.push(current);
                    true
                }),
                expected,
            );
            assert_eq!(visited, vec![0, 2, 3]);
        }
    }

    #[test]
    fn hit_path_omits_layout_duplicates_and_tiny_fragments_without_changing_branch() {
        let bounds = ui_rect(0., 0., 800., 600.);
        let panel = ui_rect(100., 100., 500., 350.);
        let element = ui_rect(200., 200., 16., 16.);
        let fragment = ui_rect(201., 201., 8., 8.);
        let mut tree = test_tree(&[
            (bounds, None, false),
            (bounds, Some(0), true),
            (panel, Some(1), true),
            (panel, Some(2), true),
            (element, Some(3), false),
            (fragment, Some(4), false),
        ]);
        assert_eq!(
            tree.query(center(fragment), |_, _| true),
            vec![element, panel, bounds],
        );
    }

    #[test]
    fn query_stops_on_incomplete_loading_without_backtracking_through_overlay() {
        let bounds = ui_rect(0., 0., 800., 600.);
        let small = ui_rect(200., 200., 24., 24.);
        let mut tree = test_tree(&[
            (bounds, None, false),
            (small, Some(0), false),
            (bounds, Some(0), true),
        ]);
        tree.nodes[2].children = None;
        assert_eq!(
            tree.query(center(small), |_, current| current != 2),
            vec![bounds],
        );
        assert!(tree.nodes[2].children.is_none());
    }

    #[test]
    fn query_limits_deep_provider_trees_to_eighty_steps() {
        let bounds = ui_rect(0., 0., 800., 600.);
        let panel = ui_rect(100., 100., 500., 350.);
        let mut nodes = vec![(bounds, None, false)];
        nodes.extend((1..100).map(|index| (panel, Some(index - 1), true)));
        let mut tree = test_tree(&nodes);
        let mut steps = 0;
        assert_eq!(
            tree.query(center(panel), |_, _| {
                steps += 1;
                true
            }),
            vec![panel, bounds],
        );
        assert_eq!(steps, 80);
    }

    #[test]
    fn hierarchy_preserves_provider_ancestry_order_and_filters_only_invalid_candidates() {
        let bounds = ui_rect(0., 0., 400., 300.);
        let mut state = test_state(bounds);
        state.cursor = Point { x: 50., y: 50. };
        state.snapshots.push(WindowSnapshot { hwnd: 7, bounds });
        state.editing.probe_window = Some(7);
        // Providers can report children extending beyond a parent's own bounds.
        let deepest = ui_rect(20., 20., 100., 100.);
        let parent = ui_rect(30., 30., 40., 40.);
        let outside = ui_rect(200., 200., 40., 40.);
        let fragment = ui_rect(49., 49., 11., 20.);
        state.set_hierarchy(vec![deepest, deepest, fragment, outside, parent, parent]);
        assert_eq!(state.hierarchy, vec![deepest, parent, bounds]);
        assert_eq!(state.level, 0);
        assert_eq!(state.selection, Some(deepest));

        let minimum = ui_rect(45., 45., 12., 12.);
        state.set_hierarchy(vec![minimum, parent, bounds, bounds]);
        assert_eq!(state.hierarchy, vec![minimum, parent, bounds]);
        state.set_hierarchy(vec![fragment, outside]);
        assert_eq!(state.hierarchy, vec![bounds]);
        assert_eq!(state.selection, Some(bounds));
    }

    #[test]
    fn tab_toggles_deepest_element_and_whole_window_only_while_selecting() {
        let bounds = ui_rect(0., 0., 400., 300.);
        let deepest = ui_rect(20., 20., 40., 40.);
        let parent = ui_rect(10., 10., 200., 200.);
        let mut state = test_state(bounds);
        let history = SelectionHistory::default();
        state.cursor = center(deepest);
        state.set_hierarchy(vec![deepest, parent, bounds]);
        for (shift, expected_level) in [(false, 2), (false, 0), (true, 2), (true, 0)] {
            assert_eq!(
                state.key_down(VK_TAB.0, false, shift, bounds, &history),
                Some(CaptureKey::Handled),
            );
            assert_eq!(state.level, expected_level);
            assert_eq!(state.selection, Some(state.hierarchy[expected_level]));
        }
        state.level = 1;
        state.selection = Some(parent);
        state.key_down(VK_TAB.0, false, false, bounds, &history);
        assert_eq!(state.selection, Some(deepest));
        state.origin = Some(state.cursor);
        state.key_down(VK_TAB.0, false, false, bounds, &history);
        assert_eq!(state.selection, Some(deepest));
        state.origin = None;
        state.editing.phase = CapturePhase::Editing;
        assert_eq!(
            state.key_down(VK_TAB.0, false, false, bounds, &history),
            None
        );
        assert_eq!(state.selection, Some(deepest));

        state.editing.phase = CapturePhase::Selecting;
        state.set_hierarchy(vec![bounds]);
        state.key_down(VK_TAB.0, false, false, bounds, &history);
        assert_eq!(state.selection, Some(bounds));
        state.set_hierarchy(Vec::new());
        state.key_down(VK_TAB.0, false, false, bounds, &history);
        assert_eq!(state.selection, None);
    }

    #[test]
    fn slow_uia_degrades_only_its_window_permanently_after_300ms() {
        let mut health = ProbeHealth::default();
        assert!(health.record(7, Duration::from_millis(300), true));
        assert!(!health.record(7, Duration::from_millis(301), true));
        assert!(!health.record(7, Duration::ZERO, true));
        assert!(health.available(8));
        assert!(!health.record(8, Duration::ZERO, false));
    }

    #[test]
    fn stale_window_bounds_clear_hover_selection_and_cached_candidates() {
        let bounds = ui_rect(-200., -100., 100., 80.);
        assert!(contains_rect(bounds, bounds));
        assert!(contains_rect(ui_rect(-210., -110., 120., 100.), bounds));
        assert!(!contains_rect(ui_rect(-199., -100., 100., 80.), bounds));
        assert!(!contains_rect(ui_rect(-200., -100., 99., 80.), bounds));
        assert!(!contains_rect(Rect::default(), bounds));
        let mut state = test_state(bounds);
        state.snapshots.push(WindowSnapshot { hwnd: 7, bounds });
        state.editing.probe_window = Some(7);
        state.editing.probe_point = Some(state.cursor);
        state.hierarchy.push(bounds);
        assert!(state.apply_probe_result(ProbeResult {
            hwnd: 7,
            point: state.cursor,
            generation: state.generation,
            valid: false,
            rectangles: vec![bounds],
        }));
        assert!(state.snapshots.is_empty());
        assert!(state.hierarchy.is_empty());
        assert!(state.selection.is_none());
        assert!(state.editing.probe_point.is_none());
        assert!(state.editing.probe_window.is_none());
    }

    #[test]
    fn arrow_keys_move_and_resize_in_physical_pixels_with_desktop_clamping() {
        let desktop = ui_rect(-100., -50., 200., 100.);
        let rect = ui_rect(-90., -40., 30., 20.);
        assert_eq!(
            keyboard_selection(rect, VK_RIGHT.0, false, false, desktop),
            Some(ui_rect(-89., -40., 30., 20.))
        );
        assert_eq!(
            keyboard_selection(rect, VK_DOWN.0, false, true, desktop),
            Some(ui_rect(-90., -30., 30., 20.))
        );
        let corner = ui_rect(-99., -49., 30., 20.);
        assert_eq!(
            keyboard_selection(corner, VK_LEFT.0, false, true, desktop),
            Some(ui_rect(-100., -49., 30., 20.))
        );
        assert_eq!(
            keyboard_selection(corner, VK_UP.0, false, true, desktop),
            Some(ui_rect(-99., -50., 30., 20.))
        );
        let edge = ui_rect(69., 29., 30., 20.);
        assert_eq!(
            keyboard_selection(edge, VK_RIGHT.0, false, true, desktop),
            Some(ui_rect(70., 29., 30., 20.))
        );
        assert_eq!(
            keyboard_selection(edge, VK_DOWN.0, false, true, desktop),
            Some(ui_rect(69., 30., 30., 20.))
        );
        assert_eq!(
            keyboard_selection(rect, VK_RIGHT.0, true, true, desktop),
            Some(ui_rect(-90., -40., 40., 20.))
        );
        assert_eq!(
            keyboard_selection(rect, VK_DOWN.0, true, false, desktop),
            Some(ui_rect(-90., -40., 30., 21.))
        );
        assert_eq!(
            keyboard_selection(edge, VK_RIGHT.0, true, true, desktop),
            Some(ui_rect(69., 29., 31., 20.))
        );
        assert_eq!(
            keyboard_selection(edge, VK_DOWN.0, true, true, desktop),
            Some(ui_rect(69., 29., 30., 21.))
        );
        let tiny = ui_rect(-50., -20., 2., 3.);
        assert_eq!(
            keyboard_selection(tiny, VK_LEFT.0, true, true, desktop),
            Some(ui_rect(-50., -20., 1., 3.))
        );
        assert_eq!(
            keyboard_selection(tiny, VK_UP.0, true, true, desktop),
            Some(ui_rect(-50., -20., 2., 1.))
        );
    }

    #[test]
    fn keyboard_adjustment_locks_hover_and_translates_global_polygon_points() {
        let desktop = ui_rect(-100., -50., 200., 100.);
        let history = SelectionHistory::default();
        let mut state = test_state(desktop);
        let points = vec![
            Point { x: -80., y: -30. },
            Point { x: -60., y: -30. },
            Point { x: -70., y: -10. },
        ];
        state.selection = Some(ui_rect(-80., -30., 20., 20.));
        state.shape = SelectionShape::Polygon(points.clone());
        state.key_down(VK_RIGHT.0, false, true, desktop, &history);
        assert_eq!(state.editing.phase, CapturePhase::Editing);
        assert_eq!(state.selection, Some(ui_rect(-70., -30., 20., 20.)));
        let SelectionShape::Polygon(moved) = &state.shape else {
            panic!("Expected polygon")
        };
        assert_eq!(
            moved[0],
            Point {
                x: points[0].x + 10.,
                y: points[0].y
            }
        );
        state.key_down(VK_DOWN.0, true, false, desktop, &history);
        assert_eq!(state.selection, Some(ui_rect(-70., -30., 20., 21.)));
        assert!(matches!(state.shape, SelectionShape::Rectangle));
        state.editing.text = Some(ArrowCommentEditor::new(1, ArrowComment::default()));
        let previous = state.selection;
        assert_eq!(
            state.key_down(VK_LEFT.0, false, false, desktop, &history),
            None
        );
        assert_eq!(state.selection, previous);
    }

    #[test]
    fn selecting_color_copy_uses_monitor_local_pixels_and_the_selected_format() -> Result<()> {
        let bounds = ui_rect(-2., -1., 2., 1.);
        let history = SelectionHistory::default();
        let mut state = test_state(bounds);
        let image = CapturedImage {
            width: 2,
            height: 1,
            pixels: vec![0, 0, 255, 255, 0x56, 0x34, 0x12, 255],
        };
        assert_eq!(sample_color(&image, bounds, Point { x: 0., y: -1. }), None);
        state.monitors[0].1 = Arc::new(image);
        state.cursor = Point { x: -1., y: -1. };
        state.selection = Some(ui_rect(-2., -1., 1., 1.));
        assert_eq!(
            state.key_down(0x43, false, false, bounds, &history),
            Some(CaptureKey::Text(CaptureTextKind::Color)),
        );
        let mut formats = HashSet::new();
        for _ in 0..COLOR_FORMATS.len() {
            formats.insert(format_color(
                state.sampled_color().unwrap(),
                COLOR_FORMATS[state.color_format],
            ));
            assert_eq!(
                state.key_down(0x46, false, false, bounds, &history),
                Some(CaptureKey::Handled)
            );
        }
        assert_eq!(formats.len(), COLOR_FORMATS.len());
        assert_eq!(state.color_format, 0);
        assert!(formats.contains("#123456"));
        state.key_down(0x43, false, true, bounds, &history);
        assert_eq!(state.color_format, 1);
        state.finish_text(CaptureTextKind::Color)?;
        assert!(state.finished);
        assert!(matches!(&state.events[..],
            [CaptureEvent::TextCopied { text, kind: CaptureTextKind::Color }] if text == "rgb(18, 52, 86)"
        ));
        Ok(())
    }

    #[test]
    fn selecting_coordinate_copy_preserves_global_negative_coordinates() -> Result<()> {
        let bounds = ui_rect(-1920., -200., 4., 4.);
        let mut state = test_state(bounds);
        state.cursor = Point {
            x: -1919.,
            y: -199.,
        };
        assert_eq!(
            state.key_down(0x58, false, false, bounds, &SelectionHistory::default()),
            Some(CaptureKey::Text(CaptureTextKind::Coordinate)),
        );
        state.finish_text(CaptureTextKind::Coordinate)?;
        assert!(state.finished);
        assert!(matches!(&state.events[..],
            [CaptureEvent::TextCopied { text, kind: CaptureTextKind::Coordinate }] if text == "-1919, -199"
        ));
        Ok(())
    }

    fn selected_event(rect: Rect, action: CaptureAction) -> CaptureEvent {
        CaptureEvent::Selected {
            rect,
            action,
            image: CapturedImage {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
            },
        }
    }

    #[test]
    fn selection_history_keeps_completed_images_and_ignores_non_image_completion() {
        let history = SelectionHistory::default();
        let first = ui_rect(-100., -50., 80., 40.);
        let second = ui_rect(10., 20., 30., 40.);
        history.remember(&selected_event(first, CaptureAction::Copy));
        for event in [
            CaptureEvent::Cancelled,
            CaptureEvent::Error("No image".into()),
            CaptureEvent::TextCopied {
                text: "#123456".into(),
                kind: CaptureTextKind::Color,
            },
            CaptureEvent::TextCopied {
                text: "1, 2".into(),
                kind: CaptureTextKind::Coordinate,
            },
            selected_event(second, CaptureAction::Longshot),
            selected_event(second, CaptureAction::Record),
        ] {
            history.remember(&event);
            assert_eq!(history.last(), Some(first));
        }
        for action in [
            CaptureAction::Confirm,
            CaptureAction::Pin,
            CaptureAction::Save,
            CaptureAction::QuickSave,
            CaptureAction::Ocr,
            CaptureAction::QuickAsk,
        ] {
            history.remember(&selected_event(second, action));
            assert_eq!(history.last(), Some(second));
            history.remember(&selected_event(first, CaptureAction::Copy));
        }
    }

    #[test]
    fn history_shortcuts_restore_clipped_global_rectangle_into_editing() {
        let history = SelectionHistory::default();
        let desktop = ui_rect(-100., -50., 200., 100.);
        history.remember(&selected_event(
            ui_rect(-120., -60., 50., 40.),
            CaptureAction::Copy,
        ));
        for key in [0x48, VK_OEM_COMMA.0] {
            let mut state = test_state(desktop);
            state.shape = SelectionShape::Rounded { radius: 10. };
            assert_eq!(
                state.key_down(key, false, false, desktop, &history),
                Some(CaptureKey::Handled)
            );
            assert_eq!(state.selection, Some(ui_rect(-100., -50., 30., 30.)));
            assert_eq!(state.editing.phase, CapturePhase::Editing);
            assert!(matches!(state.shape, SelectionShape::Rectangle));
            assert!(state.events.is_empty());
        }
        let mut state = test_state(desktop);
        history.remember(&selected_event(
            ui_rect(200., 100., 30., 40.),
            CaptureAction::Copy,
        ));
        state.key_down(0x48, false, false, desktop, &history);
        assert_eq!(state.selection, Some(desktop));
        assert_eq!(state.editing.phase, CapturePhase::Selecting);
    }

    #[test]
    fn quick_save_shortcut_requires_editing_and_preserves_normal_save() {
        let desktop = ui_rect(-100., -50., 200., 100.);
        let history = SelectionHistory::default();
        let mut state = test_state(desktop);
        assert_eq!(state.key_down(0x53, true, true, desktop, &history), None);
        state.enter_editing(desktop);
        assert_eq!(
            state.key_down(0x53, true, true, desktop, &history),
            Some(CaptureKey::Image(CaptureAction::QuickSave))
        );
        assert_eq!(
            state.key_down(0x53, true, false, desktop, &history),
            Some(CaptureKey::Image(CaptureAction::Save))
        );
        assert_eq!(
            state.key_down(0x43, true, false, desktop, &history),
            Some(CaptureKey::Image(CaptureAction::Copy))
        );
        assert_eq!(state.key_down(0x43, false, false, desktop, &history), None);
        assert!(state.events.is_empty());
    }

    #[test]
    fn capture_tools_and_styles_require_a_matching_uncancelled_release() {
        let mut state = test_state(ui_rect(-1600., -200., 1600., 1000.));
        state.selection = Some(ui_rect(-1200., 100., 500., 200.));
        state.editing.phase = CapturePhase::Editing;
        let (bar, _) = state.toolbar().unwrap();
        let pen = center(state.tool_rect(bar, 2));
        let save = center(state.tool_rect(bar, 14));
        assert!(state.press_control(pen));
        assert_eq!(state.editing.tool, None);
        assert!(state.events.is_empty());
        assert_eq!(state.release_control(save), (true, None));
        assert_eq!(state.editing.tool, None);
        assert!(state.press_control(pen));
        assert_eq!(state.release_control(pen), (true, None));
        assert_eq!(state.editing.tool, Some(ToolbarIcon::Pen));
        let (bar, style) = state.toolbar().unwrap();
        let fill = center(state.style_rect(style, 3));
        let color = center(state.style_rect(style, 10));
        assert!(state.press_control(fill));
        assert!(!state.editing.style.fill);
        assert_eq!(state.release_control(color), (true, None));
        assert!(!state.editing.style.fill);
        assert!(state.press_control(fill));
        assert_eq!(state.release_control(fill), (true, None));
        assert!(state.editing.style.fill);
        assert!(state.press_control(color));
        assert_eq!(state.editing.style.argb, COLORS[0]);
        assert_eq!(state.release_control(color), (true, None));
        assert_eq!(state.editing.style.argb, COLORS[1]);
        let save = center(state.tool_rect(bar, 14));
        assert!(state.press_control(save));
        state.cancel_pointer();
        assert_eq!(state.release_control(save), (false, None));
        assert!(state.events.is_empty());
        let gap = Point {
            x: bar.x + 2.,
            y: bar.y + 2.,
        };
        assert!(state.press_control(gap));
        assert_eq!(state.release_control(save), (true, None));
        assert_eq!(state.release_control(save), (false, None));
        assert!(state.editing.points.is_empty());
        assert!(state.editing.drag.is_none());
    }

    #[test]
    fn save_mouse_release_hides_overlay_and_emits_deferred_save() -> Result<()> {
        let _desktop = crate::pin_window::NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let monitor = monitors()?.remove(0);
        let rect = ui_rect(monitor.bounds.x, monitor.bounds.y, 12., 12.);
        let mut state = test_state(rect);
        state.monitors[0].0 = monitor;
        state.editing.phase = CapturePhase::Editing;
        let state = Rc::new(RefCell::new(state));
        let window = unsafe {
            let instance = GetModuleHandleW(None)?;
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(overlay_proc),
                hInstance: instance.into(),
                lpszClassName: w!("OrbmentCaptureSaveReleaseTest"),
                ..Default::default()
            });
            TestWindow(CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("OrbmentCaptureSaveReleaseTest"),
                w!("Save release"),
                WS_POPUP,
                rect.x as i32,
                rect.y as i32,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                None,
            )?)
        };
        let surface = Box::new(OverlaySurface {
            state: state.clone(),
            monitor: 0,
            target: RefCell::new(PresentTarget::new(
                Rc::new(D2dContext::new()?),
                window.0,
                1,
                1,
            )?),
            masked: RefCell::new(None),
            scratch: RefCell::new(None),
        });
        let hwnd = window.0;
        state.borrow_mut().windows.push(hwnd.0 as isize);
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*surface as *const _ as isize);
        }
        let mut capture = CaptureAndPinWindows {
            windows: vec![(hwnd, surface)],
            state: state.clone(),
        };
        std::mem::forget(window);
        let save = {
            let state = state.borrow();
            let (bar, _) = state.toolbar().unwrap();
            center(state.tool_rect(bar, 14))
        };
        let mut client = POINT {
            x: save.x.round() as i32,
            y: save.y.round() as i32,
        };
        ensure!(
            unsafe { ScreenToClient(hwnd, &mut client) }.as_bool(),
            "Cannot locate test Save button"
        );
        let pointer = pointer_lparam(Point {
            x: f64::from(client.x),
            y: f64::from(client.y),
        });
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        for message in [WM_CANCELMODE, WM_CAPTURECHANGED] {
            unsafe {
                SendMessageW(hwnd, WM_LBUTTONDOWN, Some(WPARAM(0)), Some(pointer));
                SendMessageW(hwnd, message, None, None);
                SendMessageW(hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(pointer));
            }
            assert!(capture.poll().is_empty());
            assert!(!capture.finished());
            assert!(state.borrow().editing.pressed_tool.is_none());
        }
        unsafe {
            SendMessageW(hwnd, WM_LBUTTONDOWN, Some(WPARAM(0)), Some(pointer));
        }
        assert!(capture.poll().is_empty());
        assert!(!capture.finished());
        assert_eq!(state.borrow().editing.pressed_tool, Some(ToolbarIcon::Save));
        assert!(unsafe { IsWindowVisible(hwnd).as_bool() });
        unsafe {
            SendMessageW(hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(pointer));
        }
        assert!(capture.finished());
        assert!(!unsafe { IsWindowVisible(hwnd).as_bool() });
        assert_ne!(unsafe { GetCapture() }, hwnd);
        assert!(matches!(
            capture.poll().as_slice(),
            [CaptureEvent::Selected { image, rect: selected, action: CaptureAction::Save }]
                if *selected == rect && image.width == 12 && image.height == 12
        ));
        unsafe {
            SendMessageW(hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(pointer));
            assert_eq!(SendMessageW(hwnd, WM_GETTEXTLENGTH, None, None).0, 12);
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                3,
                4,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
        }
        assert!(capture.poll().is_empty());
        drop(capture);
        assert!(!unsafe { IsWindow(Some(hwnd)).as_bool() });
        Ok(())
    }

    fn test_recording_state(work: Rect, scale: f64) -> RecordingHudState {
        RecordingHudState {
            region: ui_rect(work.x + 60., work.y + 100., 640., 480.),
            work,
            scale,
            frame: HWND::default(),
            toolbar: HWND::default(),
            system_audio: true,
            microphone: false,
            encode_format: 0,
            active: false,
            pending: false,
            closed: false,
            started: None,
            pause_started: None,
            paused_elapsed: Duration::ZERO,
            position: None,
            drag: None,
            pressed: None,
            hovered: None,
            card: Rect::default(),
            buttons: Vec::new(),
            events: Vec::new(),
            wake: None,
        }
    }

    #[test]
    fn recording_layout_keeps_tooltips_and_hit_targets_inside_its_window() -> Result<()> {
        let work = ui_rect(-1920., -200., 1920., 1080.);
        for scale in [1., 1.25, 2.] {
            for active in [false, true] {
                for y in [work.y, work.y + work.height] {
                    let mut state = test_recording_state(work, scale);
                    state.active = active;
                    state.position = Some(Point { x: work.x, y });
                    let (_, bounds) = state.toolbar_canvas()?;
                    assert_eq!(state.card.height, 52. * scale);
                    assert_eq!(state.buttons.len(), if active { 3 } else { 5 });
                    let buttons = state.buttons.clone();
                    for (button, control) in buttons {
                        assert_eq!(button.height, 40. * scale);
                        assert_eq!(
                            button.width,
                            if control == RecordingControl::Format {
                                56. * scale
                            } else {
                                40. * scale
                            }
                        );
                        assert_eq!(state.control_at(center(button)), Some(control));
                        assert_eq!(state.card.intersection(button), Some(button));
                        state.hovered = Some(control);
                        let (_, hovered_bounds) = state.toolbar_canvas()?;
                        assert_eq!(hovered_bounds, bounds);
                        assert_eq!(state.control_at(center(button)), Some(control));
                        let text = render_ui_text(
                            &state.control_title(control),
                            16. * scale,
                            true,
                            0xffffffff,
                        )?;
                        let card = ui_rect(
                            state.card.x + bounds.x,
                            state.card.y + bounds.y,
                            state.card.width,
                            state.card.height,
                        );
                        let button = ui_rect(
                            button.x + bounds.x,
                            button.y + bounds.y,
                            button.width,
                            button.height,
                        );
                        let tooltip = recording_tooltip_bounds(&text, button, card, scale, work);
                        assert_eq!(bounds.intersection(tooltip), Some(tooltip));
                        assert_eq!(work.intersection(tooltip), Some(tooltip));
                        assert!(card.intersection(tooltip).is_none());
                        assert_eq!(
                            state.control_at(Point {
                                x: center(tooltip).x - bounds.x,
                                y: center(tooltip).y - bounds.y,
                            }),
                            None
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn recording_buttons_require_matching_release_and_obey_pending_state() -> Result<()> {
        let mut state = test_recording_state(ui_rect(0., 0., 1920., 1080.), 1.25);
        state.toolbar_canvas()?;
        let point = |state: &RecordingHudState, control| {
            center(state.buttons.iter().find(|(_, c)| *c == control).unwrap().0)
        };
        let begin = point(&state, RecordingControl::Begin);
        let cancel = point(&state, RecordingControl::Cancel);
        state.press_control(begin);
        assert!(!state.pending);
        assert!(state.events.is_empty());
        assert_eq!(state.release_control(cancel), None);
        state.press_control(begin);
        assert_eq!(state.release_control(begin), Some(RecordingControl::Begin));
        assert_eq!(state.release_control(begin), None);
        state.press_control(begin);
        state.cancel_pointer();
        assert_eq!(state.release_control(begin), None);
        state.pending = true;
        state.press_control(begin);
        assert_eq!(state.release_control(begin), None);
        state.press_control(cancel);
        assert_eq!(
            state.release_control(cancel),
            Some(RecordingControl::Cancel)
        );
        state.closed = true;
        state.press_control(cancel);
        assert_eq!(state.release_control(cancel), None);
        Ok(())
    }

    #[test]
    fn switching_windows_highlights_only_the_new_window_until_probe_completion() {
        let desktop = ui_rect(0., 0., 800., 600.);
        let first = ui_rect(0., 0., 400., 600.);
        let second = ui_rect(400., 0., 400., 600.);
        let mut state = test_state(desktop);
        state.smart = true;
        state.snapshots = vec![
            WindowSnapshot {
                hwnd: 7,
                bounds: first,
            },
            WindowSnapshot {
                hwnd: 8,
                bounds: second,
            },
        ];
        state.editing.probe_window = Some(7);
        state.cursor = Point { x: 50., y: 50. };
        state.editing.probe_point = Some(state.cursor);
        state.set_hierarchy(vec![ui_rect(20., 20., 100., 100.)]);
        state.cursor = Point { x: 450., y: 50. };
        assert!(state.smart_hover(HWND::default()));
        assert_eq!(state.hierarchy, vec![second]);
        assert_eq!(state.selection, Some(second));
        assert_eq!(state.editing.probe_window, Some(8));
        let request = state.probe.request.0.lock().unwrap();
        assert_eq!(request.as_ref().unwrap().hwnd, 8);
    }

    #[test]
    fn hover_preserves_resolved_selection_until_current_probe_arrives() {
        let bounds = ui_rect(-200., -100., 400., 300.);
        let mut state = test_state(bounds);
        state.smart = true;
        state.selection = None;
        state.snapshots = vec![WindowSnapshot { hwnd: 7, bounds }];
        state.cursor = Point { x: -180., y: -80. };
        assert!(state.smart_hover(HWND::default()));
        assert_eq!(state.selection, Some(bounds));
        let first = ui_rect(-190., -90., 80., 30.);
        assert!(state.apply_probe_result(ProbeResult {
            hwnd: 7,
            point: state.cursor,
            generation: state.generation,
            valid: true,
            rectangles: vec![first],
        }));
        let previous_point = state.cursor;
        state.cursor.x += 10.;
        // Mouse movement may precede the timer that assigns a new generation.
        assert!(!state.apply_probe_result(ProbeResult {
            hwnd: 7,
            point: previous_point,
            generation: state.generation,
            valid: true,
            rectangles: vec![bounds],
        }));
        assert!(!state.smart_hover(HWND::default()));
        assert_eq!(state.selection, Some(first));
        let current = ui_rect(-200., -100., 200., 80.);
        assert!(state.apply_probe_result(ProbeResult {
            hwnd: 7,
            point: state.cursor,
            generation: state.generation,
            valid: true,
            rectangles: vec![current],
        }));
        assert_eq!(state.selection, Some(current));
        assert!(!state.hierarchy.contains(&first));
        assert!(!state.apply_probe_result(ProbeResult {
            hwnd: 7,
            point: state.cursor,
            generation: state.generation,
            valid: true,
            rectangles: vec![current],
        }));
        assert!(!state.apply_probe_result(ProbeResult {
            hwnd: 7,
            point: state.cursor,
            generation: state.generation - 1,
            valid: true,
            rectangles: vec![first],
        }));
        assert_eq!(state.selection, Some(current));
    }

    #[test]
    fn pin_hotkey_finishes_an_in_progress_drag_without_mouse_release() -> Result<()> {
        let desktop = virtual_desktop();
        let bounds = ui_rect(desktop.x, desktop.y, 12., 12.);
        let mut state = test_state(bounds);
        state.selection = None;
        state.origin = Some(Point {
            x: bounds.x + 2.,
            y: bounds.y + 3.,
        });
        state.cursor = Point {
            x: bounds.x + 9.,
            y: bounds.y + 10.,
        };
        let expected = state.shape.constrain(state.origin.unwrap(), state.cursor);
        let mut capture = CaptureAndPinWindows {
            windows: Vec::new(),
            state: Rc::new(RefCell::new(state)),
        };
        assert!(capture.finish_as_pin()?);
        assert!(matches!(capture.poll().as_slice(),
            [CaptureEvent::Selected { image, rect, action: CaptureAction::Pin }]
                if *rect == expected && image.width == 7 && image.height == 7
        ));
        assert!(capture.finished());
        Ok(())
    }

    #[test]
    fn local_controls_use_only_their_clipped_bounds_on_large_negative_monitors() {
        let monitor = ui_rect(-3840., -200., 3840., 2160.);
        let parts = [
            ui_rect(-1000., 1500., 760., 65.),
            ui_rect(-1000., 1572., 705., 58.),
            ui_rect(-700., 1435., 220., 50.),
        ];
        let bounds = local_canvas_bounds(&parts, monitor, 8.).unwrap();
        assert_eq!(bounds, ui_rect(-1008., 1427., 776., 211.));
        assert!(bounds.width * bounds.height < monitor.width * monitor.height / 40.);
        assert_eq!(
            local_canvas_bounds(&[ui_rect(-3850., -210., 30., 30.)], monitor, 2.),
            Some(ui_rect(-3840., -200., 22., 22.)),
        );
        assert!(local_canvas_bounds(&[ui_rect(10., 10., 10., 10.)], monitor, 2.).is_none());
    }

    #[test]
    fn repaint_reuses_pixel_storage_and_erases_previous_local_controls() {
        let background = CapturedImage {
            width: 12,
            height: 8,
            pixels: vec![17; 12 * 8 * 4],
        };
        let mut scratch = None;
        let frame = prepare_frame(&mut scratch, &background);
        let allocation = frame.pixels.as_ptr();
        fill_ui_rect(frame, ui_rect(-2., 1., 5., 2.), 0xff1677ff);
        assert_eq!(&frame.pixels[12 * 4..][..4], &[255, 119, 22, 255]);
        assert_eq!(&frame.pixels[15 * 4..][..4], &[17; 4]);
        let frame = prepare_frame(&mut scratch, &background);
        assert_eq!(frame.pixels.as_ptr(), allocation);
        assert_eq!(frame.pixels, background.pixels);
    }

    #[test]
    fn completing_capture_defers_synchronous_hide_until_state_is_released() -> Result<()> {
        let _desktop = crate::pin_window::NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let desktop = virtual_desktop();
        let rect = Rect {
            x: desktop.x,
            y: desktop.y,
            width: 2.0,
            height: 2.0,
        };
        let pixels = vec![17; 16];
        let state = Rc::new(RefCell::new(test_state(rect)));
        let window = unsafe {
            let instance = GetModuleHandleW(None)?;
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(test_window_proc),
                hInstance: instance.into(),
                lpszClassName: w!("OrbmentCaptureReentrancyTest"),
                ..Default::default()
            });
            TestWindow(CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("OrbmentCaptureReentrancyTest"),
                w!("Orbment capture test"),
                WS_POPUP,
                desktop.x as i32 - 10,
                desktop.y as i32 - 10,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                None,
            )?)
        };
        state.borrow_mut().windows.push(window.0.0 as isize);
        unsafe {
            SetWindowLongPtrW(window.0, GWLP_USERDATA, Rc::as_ptr(&state) as isize);
            let _ = ShowWindow(window.0, SW_SHOWNOACTIVATE);
        }
        HIDE_BORROWABLE.set(None);
        let mut borrowed = state.borrow_mut();
        borrowed.enter_editing(desktop);
        assert_eq!(borrowed.editing.phase, CapturePhase::Editing);
        assert!(!borrowed.finished);
        assert!(borrowed.events.is_empty());
        let windows = borrowed.finish(CaptureAction::Copy)?;
        assert!(borrowed.finished);
        assert!(matches!(
            borrowed.events.as_slice(),
            [CaptureEvent::Selected { image, .. }] if image.pixels == pixels
        ));
        assert!(unsafe { IsWindowVisible(window.0).as_bool() });
        assert_eq!(HIDE_BORROWABLE.get(), None);
        let wake = borrowed.wake;
        drop(borrowed);
        hide_capture_windows(&windows, wake);
        assert_eq!(HIDE_BORROWABLE.get(), Some(true));
        assert!(!unsafe { IsWindowVisible(window.0).as_bool() });
        {
            let mut state = state.borrow_mut();
            state.finished = false;
            state.events.clear();
        }
        unsafe {
            let _ = ShowWindow(window.0, SW_SHOWNOACTIVATE);
        }
        let mut capture = CaptureAndPinWindows {
            windows: Vec::new(),
            state: state.clone(),
        };
        HIDE_BORROWABLE.set(None);
        assert!(capture.finish_as_pin()?);
        assert_eq!(HIDE_BORROWABLE.get(), Some(true));
        assert!(matches!(
            capture.poll().as_slice(),
            [CaptureEvent::Selected {
                action: CaptureAction::Pin,
                ..
            }]
        ));
        assert!(!capture.finish_as_pin()?);
        Ok(())
    }

    #[test]
    fn resize_handles_support_negative_desktop_coordinates_and_minimum_size() {
        let desktop = ui_rect(-1920., -200., 3840., 1280.);
        let rect = ui_rect(-300., -100., 200., 150.);
        assert_eq!(
            resized_selection(rect, 0, Point { x: -500., y: -180. }, desktop),
            ui_rect(-500., -180., 400., 230.)
        );
        assert_eq!(
            resized_selection(rect, 3, Point { x: -400., y: 100. }, desktop).width,
            1.
        );
        let clipped = resized_selection(rect, 4, Point { x: 9000., y: 9000. }, desktop);
        assert_eq!(clipped.x + clipped.width, 1920.);
        assert_eq!(clipped.y + clipped.height, 1080.);
    }

    #[test]
    fn recording_hud_excludes_its_windows_and_emits_start_pause_stop_cancel() -> Result<()> {
        let _desktop = crate::pin_window::NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let monitor = monitors()?.remove(0);
        let region = ui_rect(monitor.bounds.x + 40., monitor.bounds.y + 40., 240., 160.);
        let mut hud = RecordingHud::new(region, true, false, 2, None)?;
        let handles = hud.handles();
        for handle in handles {
            let hwnd = HWND(handle as *mut _);
            let mut affinity = 0;
            unsafe {
                GetWindowDisplayAffinity(hwnd, &mut affinity)?;
            }
            assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
            assert!(unsafe { IsWindowVisible(hwnd).as_bool() });
        }
        assert_ne!(
            unsafe { GetWindowLongPtrW(HWND(handles[0] as *mut _), GWL_EXSTYLE) } as u32
                & WS_EX_TRANSPARENT.0,
            0
        );
        let toolbar = HWND(handles[1] as *mut _);
        for control in [
            RecordingControl::SystemAudio,
            RecordingControl::Microphone,
            RecordingControl::Format,
            RecordingControl::Begin,
        ] {
            let (pointer, before) = {
                let state = hud.state.borrow();
                let button = state.buttons.iter().find(|(_, c)| *c == control).unwrap().0;
                (
                    pointer_lparam(center(button)),
                    (
                        state.system_audio,
                        state.microphone,
                        state.encode_format,
                        state.pending,
                    ),
                )
            };
            unsafe {
                SendMessageW(toolbar, WM_MOUSEMOVE, Some(WPARAM(0)), Some(pointer));
            }
            assert_eq!(hud.state.borrow().hovered, Some(control));
            unsafe {
                SendMessageW(toolbar, WM_LBUTTONDOWN, Some(WPARAM(0)), Some(pointer));
            }
            {
                let state = hud.state.borrow();
                assert_eq!(
                    (
                        state.system_audio,
                        state.microphone,
                        state.encode_format,
                        state.pending
                    ),
                    before,
                );
            }
            unsafe {
                SendMessageW(toolbar, WM_LBUTTONUP, Some(WPARAM(0)), Some(pointer));
                SendMessageW(toolbar, WM_MOUSELEAVE, None, None);
            }
            assert_eq!(hud.state.borrow().hovered, None);
        }
        hud.state.borrow_mut().activate(RecordingControl::Begin)?;
        assert!(matches!(
            hud.poll().as_slice(),
            [crate::VisionEvent::RecordBegin {
                system_audio: false,
                microphone: true,
                encode_format: 0,
                ..
            }]
        ));
        hud.set_active(true);
        assert!(hud.state.borrow().pressed.is_none());
        assert!(hud.state.borrow().hovered.is_none());
        {
            let mut state = hud.state.borrow_mut();
            state.activate(RecordingControl::Pause)?;
            let now = Instant::now();
            state.started = Some(now - Duration::from_secs(10));
            state.pause_started = Some(now - Duration::from_secs(3));
            state.paused_elapsed = Duration::from_secs(2);
            assert_eq!(state.elapsed(), Duration::from_secs(5));
            state.activate(RecordingControl::Pause)?;
            state.activate(RecordingControl::Stop)?;
            state.activate(RecordingControl::Stop)?;
        }
        unsafe {
            SendMessageW(
                toolbar,
                WM_KEYDOWN,
                Some(WPARAM(VK_ESCAPE.0 as usize)),
                None,
            );
            SendMessageW(
                toolbar,
                WM_KEYDOWN,
                Some(WPARAM(VK_ESCAPE.0 as usize)),
                None,
            );
        }
        assert!(matches!(
            hud.poll().as_slice(),
            [
                crate::VisionEvent::RecordPause(true),
                crate::VisionEvent::RecordPause(false),
                crate::VisionEvent::RecordStop,
                crate::VisionEvent::RecordCancel,
            ]
        ));
        assert!(hud.closed());
        assert!(
            handles
                .iter()
                .all(|handle| !unsafe { IsWindowVisible(HWND(*handle as *mut _)).as_bool() })
        );
        drop(hud);
        assert!(
            handles
                .iter()
                .all(|handle| !unsafe { IsWindow(Some(HWND(*handle as *mut _))).as_bool() })
        );
        Ok(())
    }

    #[test]
    fn recording_toolbar_wraps_all_controls_inside_narrow_high_dpi_work_areas() -> Result<()> {
        let mut state = RecordingHudState {
            region: ui_rect(-800., 20., 300., 200.),
            work: ui_rect(-1000., -100., 640., 480.),
            scale: 4.,
            frame: HWND::default(),
            toolbar: HWND::default(),
            system_audio: true,
            microphone: false,
            encode_format: 0,
            active: false,
            pending: false,
            closed: false,
            started: None,
            pause_started: None,
            paused_elapsed: Duration::ZERO,
            position: Some(Point { x: 5000., y: 5000. }),
            drag: None,
            pressed: None,
            hovered: None,
            card: Rect::default(),
            buttons: Vec::new(),
            events: Vec::new(),
            wake: None,
        };
        for active in [false, true] {
            state.active = active;
            for scale in [1., 2., 4.] {
                state.scale = scale;
                for width in [240., 320., 640., 1280.] {
                    state.work.width = width;
                    let layout = state.toolbar_layout(120. * scale);
                    assert_eq!(layout.buttons.len(), if active { 3 } else { 5 });
                    for bounds in std::iter::once(layout.card)
                        .chain(std::iter::once(layout.status))
                        .chain(layout.buttons.iter().map(|(rect, _)| *rect))
                    {
                        assert!(bounds.x >= state.work.x && bounds.y >= state.work.y);
                        assert!(bounds.x + bounds.width <= state.work.x + state.work.width + 0.001);
                        assert!(
                            bounds.y + bounds.height <= state.work.y + state.work.height + 0.001
                        );
                    }
                    for (index, (button, _)) in layout.buttons.iter().enumerate() {
                        assert!(button.intersection(layout.status).is_none());
                        for (other, _) in &layout.buttons[index + 1..] {
                            assert!(button.intersection(*other).is_none());
                        }
                    }
                }
            }
            state.work.width = 320.;
            let (_, canvas) = state.toolbar_canvas()?;
            for &(button, control) in &state.buttons {
                assert!(button.x >= 0. && button.y >= 0.);
                assert!(button.x + button.width <= canvas.width);
                assert!(button.y + button.height <= canvas.height);
                assert_eq!(state.control_at(center(button)), Some(control));
            }
        }
        Ok(())
    }

    #[test]
    fn transparent_toolbar_compositing_preserves_underlying_image() {
        let mut image = CapturedImage {
            width: 2,
            height: 1,
            pixels: vec![10, 20, 30, 255, 40, 50, 60, 255],
        };
        let overlay = CapturedImage {
            width: 2,
            height: 1,
            pixels: vec![200, 200, 200, 0, 255, 0, 0, 255],
        };
        composite_image(&mut image, &overlay, 0, 0);
        assert_eq!(&image.pixels[..4], &[10, 20, 30, 255]);
        assert_eq!(&image.pixels[4..], &[255, 0, 0, 255]);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordingControl {
    SystemAudio,
    Microphone,
    Format,
    Begin,
    Pause,
    Stop,
    Cancel,
}

struct RecordingToolbarLayout {
    scale: f64,
    card: Rect,
    status: Rect,
    buttons: Vec<(Rect, RecordingControl)>,
    separators: Vec<f64>,
}

fn recording_tooltip_bounds(
    text: &tiny_skia::Pixmap,
    button: Rect,
    card: Rect,
    scale: f64,
    work: Rect,
) -> Rect {
    let mut bounds = tooltip_bounds(
        text,
        Point {
            x: button.x + button.width / 2.,
            y: card.y,
        },
        scale,
        work,
    );
    if card.y - bounds.height - 6. * scale < work.y + 4. * scale {
        bounds.y = card.y + card.height + 6. * scale;
    }
    bounds
}

struct RecordingHudState {
    region: Rect,
    work: Rect,
    scale: f64,
    frame: HWND,
    toolbar: HWND,
    system_audio: bool,
    microphone: bool,
    encode_format: u32,
    active: bool,
    pending: bool,
    closed: bool,
    started: Option<Instant>,
    pause_started: Option<Instant>,
    paused_elapsed: Duration,
    position: Option<Point>,
    drag: Option<(POINT, Point)>,
    pressed: Option<RecordingControl>,
    hovered: Option<RecordingControl>,
    card: Rect,
    buttons: Vec<(Rect, RecordingControl)>,
    events: Vec<crate::VisionEvent>,
    wake: Option<isize>,
}

pub(crate) struct RecordingHud {
    state: Box<RefCell<RecordingHudState>>,
}

impl RecordingHud {
    pub(crate) fn new(
        region: Rect,
        system_audio: bool,
        microphone: bool,
        encode_format: u32,
        wake: Option<isize>,
    ) -> Result<Self> {
        let region = region.validate()?;
        ensure!(
            region.width >= 2. && region.height >= 2.,
            "Recording region is too small"
        );
        ensure!(
            encode_format <= 2,
            "Recording format must be MP4, GIF or WebP"
        );
        let displays = monitors()?;
        let monitor = displays
            .iter()
            .max_by(|a, b| {
                let overlap = |m: &Monitor| {
                    region
                        .intersection(m.bounds)
                        .map_or(0., |r| r.width * r.height)
                };
                overlap(a).total_cmp(&overlap(b))
            })
            .context("No recording monitor")?;
        ensure!(
            region.intersection(monitor.bounds).is_some(),
            "Recording region is off-screen"
        );
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            ensure!(
                GetMonitorInfoW(HMONITOR(monitor.handle as *mut _), &mut info).as_bool(),
                "Cannot read recording monitor"
            );
        }
        let hud = Self {
            state: Box::new(RefCell::new(RecordingHudState {
                region,
                work: rectangle(info.rcWork),
                scale: (f64::from(monitor.dpi) / 96.).max(1.25),
                frame: HWND::default(),
                toolbar: HWND::default(),
                system_audio,
                microphone,
                encode_format,
                active: false,
                pending: false,
                closed: false,
                started: None,
                pause_started: None,
                paused_elapsed: Duration::ZERO,
                position: None,
                drag: None,
                pressed: None,
                hovered: None,
                card: Rect::default(),
                buttons: Vec::new(),
                events: Vec::new(),
                wake,
            })),
        };
        unsafe {
            let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let creation = (|| -> Result<()> {
                let instance = GetModuleHandleW(None)?;
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(recording_hud_proc),
                    hInstance: instance.into(),
                    hCursor: LoadCursorW(None, IDC_ARROW)?,
                    lpszClassName: w!("OrbmentRecordingHud"),
                    ..Default::default()
                });
                for is_frame in [true, false] {
                    let hwnd = CreateWindowExW(
                        WS_EX_LAYERED
                            | WS_EX_TOPMOST
                            | WS_EX_TOOLWINDOW
                            | if is_frame {
                                WS_EX_TRANSPARENT | WS_EX_NOACTIVATE
                            } else {
                                WINDOW_EX_STYLE(0)
                            },
                        w!("OrbmentRecordingHud"),
                        if is_frame {
                            w!("Orbment Recording Region")
                        } else {
                            w!("Orbment Recording Controls")
                        },
                        WS_POPUP,
                        region.x as i32,
                        region.y as i32,
                        1,
                        1,
                        None,
                        None,
                        Some(instance.into()),
                        None,
                    )?;
                    {
                        let mut state = hud.state.borrow_mut();
                        if is_frame {
                            state.frame = hwnd;
                        } else {
                            state.toolbar = hwnd;
                        }
                    }
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*hud.state as *const _ as isize);
                    SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;
                }
                {
                    let mut state = hud.state.borrow_mut();
                    state.draw_frame()?;
                    state.draw_toolbar()?;
                    ensure!(
                        SetTimer(Some(state.toolbar), 1, 250, None) != 0,
                        "Cannot start recording HUD timer"
                    );
                }
                for hwnd in hud.handles() {
                    let _ = ShowWindow(HWND(hwnd as *mut _), SW_SHOWNOACTIVATE);
                }
                Ok(())
            })();
            SetThreadDpiAwarenessContext(previous);
            creation?;
        }
        Ok(hud)
    }
    pub(crate) fn handles(&self) -> [isize; 2] {
        let state = self.state.borrow();
        [state.frame.0 as isize, state.toolbar.0 as isize]
    }
    pub(crate) fn set_wake_hwnd(&mut self, hwnd: isize) {
        self.state.borrow_mut().wake = Some(hwnd);
    }
    pub(crate) fn set_active(&mut self, active: bool) {
        let mut state = self.state.borrow_mut();
        let changed = state.active != active;
        if changed {
            state.started = active.then(Instant::now);
            state.pause_started = None;
            state.paused_elapsed = Duration::ZERO;
            state.cancel_pointer();
            state.hovered = None;
        }
        state.active = active;
        state.pending = false;
        if let Err(error) = state.draw_frame().and_then(|_| state.draw_toolbar()) {
            state.emit(crate::VisionEvent::Error(format!(
                "Recording controls: {error:#}"
            )));
        }
        let toolbar = state.toolbar;
        drop(state);
        if changed {
            unsafe {
                if GetCapture() == toolbar {
                    let _ = ReleaseCapture();
                }
            }
        }
    }
    pub(crate) fn poll(&mut self) -> Vec<crate::VisionEvent> {
        std::mem::take(&mut self.state.borrow_mut().events)
    }
    pub(crate) fn closed(&self) -> bool {
        self.state.borrow().closed
    }
}

impl Drop for RecordingHud {
    fn drop(&mut self) {
        for handle in self.handles() {
            let hwnd = HWND(handle as *mut _);
            if !hwnd.is_invalid() {
                unsafe {
                    let _ = KillTimer(Some(hwnd), 1);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
    }
}

impl RecordingHudState {
    fn emit(&mut self, event: crate::VisionEvent) {
        self.events.push(event);
        if let Some(wake) = self.wake {
            orbment_ui::wake(HWND(wake as *mut _));
        }
    }
    fn elapsed(&self) -> Duration {
        self.started.map_or(Duration::ZERO, |start| {
            self.pause_started
                .unwrap_or_else(Instant::now)
                .saturating_duration_since(start)
                .saturating_sub(self.paused_elapsed)
        })
    }
    fn draw_frame(&self) -> Result<()> {
        let width = self.region.width.ceil() as u32 + 8;
        let height = self.region.height.ceil() as u32 + 8;
        let color = if self.active { 0xfff5222d } else { 0xff1677ff };
        let mut ui = UiCanvas::new(width, height)?;
        let desktop = virtual_desktop();
        let origin = Point {
            x: self.region.x.floor() - 4.,
            y: self.region.y.floor() - 4.,
        };
        // Keep the border visible even when a selection touches the desktop edge.
        let left = 2f64.max(desktop.x - origin.x + 2.);
        let top = 2f64.max(desktop.y - origin.y + 2.);
        let right = (f64::from(width) - 2.).min(desktop.x + desktop.width - origin.x - 2.);
        let bottom = (f64::from(height) - 2.).min(desktop.y + desktop.height - origin.y - 2.);
        ui.rounded(
            ui_rect(left, top, right - left, bottom - top),
            0.,
            color,
            Some(3.),
        );
        let length = (22. * self.scale).min((right - left).min(bottom - top).max(0.) / 2.);
        for (x, y, dx, dy) in [
            (left, top, 1., 1.),
            (right, top, -1., 1.),
            (left, bottom, 1., -1.),
            (right, bottom, -1., -1.),
        ] {
            ui.line(
                &[(x, y + length * dy), (x, y), (x + length * dx, y)],
                color,
                4.,
            );
        }
        unsafe {
            SetWindowPos(
                self.frame,
                Some(HWND_TOPMOST),
                self.region.x.floor() as i32 - 4,
                self.region.y.floor() as i32 - 4,
                width as i32,
                height as i32,
                SWP_NOACTIVATE,
            )?;
        }
        ui.present_layered(self.frame)
    }
    fn draw_toolbar(&mut self) -> Result<()> {
        let mut monitor = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            let dpi = GetDpiForWindow(self.toolbar);
            if dpi != 0 {
                self.scale = (f64::from(dpi) / 96.).max(1.25);
            }
            if GetMonitorInfoW(
                MonitorFromWindow(self.toolbar, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            )
            .as_bool()
            {
                self.work = rectangle(monitor.rcWork);
            }
        }
        let (ui, bounds) = self.toolbar_canvas()?;
        unsafe {
            SetWindowPos(
                self.toolbar,
                Some(HWND_TOPMOST),
                bounds.x as i32,
                bounds.y as i32,
                bounds.width as i32,
                bounds.height as i32,
                SWP_NOACTIVATE,
            )?;
        }
        ui.present_layered(self.toolbar)
    }
    fn toolbar_layout(&self, status_width: f64) -> RecordingToolbarLayout {
        let controls: &[RecordingControl] = if self.active {
            &[
                RecordingControl::Pause,
                RecordingControl::Stop,
                RecordingControl::Cancel,
            ]
        } else {
            &[
                RecordingControl::SystemAudio,
                RecordingControl::Microphone,
                RecordingControl::Format,
                RecordingControl::Begin,
                RecordingControl::Cancel,
            ]
        };
        let logical_status = (status_width / self.scale + 40.).max(104.);
        // Preserve DPI-sized controls when wrapping suffices; compact only on tiny work areas.
        let mut s = self.scale.min(self.work.width / 80.);
        let (status_width, width, height, mut buttons, mut separators) = loop {
            let available = self.work.width - 8. * s;
            let status_width = (logical_status * s).min(available - 12. * s);
            let mut x = 6. * s + status_width + 16. * s;
            let mut separators = vec![x - 8. * s];
            let mut buttons = Vec::new();
            for &control in controls {
                if control == RecordingControl::Begin {
                    separators.push(x + 4. * s);
                    x += 12. * s;
                }
                let width = if control == RecordingControl::Format {
                    56. * s
                } else {
                    40. * s
                };
                buttons.push((ui_rect(x, 6. * s, width, 40. * s), control));
                x += width + 4. * s;
            }
            let (width, height) = if x + 2. * s <= available {
                (x + 2. * s, 52. * s)
            } else {
                separators.clear();
                let mut x = 6. * s;
                let mut y = 50. * s;
                let mut width = status_width + 12. * s;
                for (button, _) in &mut buttons {
                    if x + button.width + 6. * s > available {
                        x = 6. * s;
                        y += 44. * s;
                    }
                    button.x = x;
                    button.y = y;
                    x += button.width + 4. * s;
                    width = width.max(x + 2. * s);
                }
                (width, y + 46. * s)
            };
            if height + 8. * s <= self.work.height {
                break (status_width, width, height, buttons, separators);
            }
            s *= 0.99 * self.work.height / (height + 8. * s);
        };
        let preferred = self.position.unwrap_or_else(|| {
            let below = self.region.y + self.region.height + 10. * s;
            Point {
                x: self.region.x + self.region.width - width,
                y: if below + height <= self.work.y + self.work.height {
                    below
                } else if self.region.y - height - 10. * s >= self.work.y {
                    self.region.y - height - 10. * s
                } else {
                    self.work.y + self.work.height - height
                },
            }
        });
        let origin = Point {
            x: preferred.x.clamp(
                self.work.x + 4. * s,
                (self.work.x + self.work.width - width - 4. * s).max(self.work.x + 4. * s),
            ),
            y: preferred.y.clamp(
                self.work.y + 4. * s,
                (self.work.y + self.work.height - height - 4. * s).max(self.work.y + 4. * s),
            ),
        };
        for (rect, _) in &mut buttons {
            rect.x += origin.x;
            rect.y += origin.y;
        }
        for x in &mut separators {
            *x += origin.x;
        }
        RecordingToolbarLayout {
            scale: s,
            card: ui_rect(origin.x, origin.y, width, height),
            status: ui_rect(origin.x + 6. * s, origin.y + 10. * s, status_width, 32. * s),
            buttons,
            separators,
        }
    }
    fn control_title(&self, control: RecordingControl) -> String {
        match control {
            RecordingControl::SystemAudio if self.system_audio => "系统声音: 已开启".into(),
            RecordingControl::SystemAudio => "系统声音: 已关闭".into(),
            RecordingControl::Microphone if self.microphone => "麦克风: 已开启".into(),
            RecordingControl::Microphone => "麦克风: 已关闭".into(),
            RecordingControl::Format => format!(
                "导出格式: {} (点击切换)",
                ["MP4", "GIF", "WebP"][self.encode_format as usize],
            ),
            RecordingControl::Begin if self.pending => "正在准备…".into(),
            RecordingControl::Begin => "开始区域录屏".into(),
            RecordingControl::Pause if self.pause_started.is_some() => "继续录屏".into(),
            RecordingControl::Pause => "暂停录屏".into(),
            RecordingControl::Stop if self.pending => "正在保存…".into(),
            RecordingControl::Stop => "完成并保存录屏".into(),
            RecordingControl::Cancel => "取消录屏 (Esc)".into(),
        }
    }
    fn control_enabled(&self, control: RecordingControl) -> bool {
        !self.closed
            && (!self.pending || control == RecordingControl::Cancel)
            && match control {
                RecordingControl::Pause | RecordingControl::Stop => self.active,
                RecordingControl::Cancel => true,
                _ => !self.active,
            }
    }
    fn control_at(&self, point: Point) -> Option<RecordingControl> {
        self.buttons
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, control)| *control)
    }
    fn press_control(&mut self, point: Point) {
        self.pressed = self
            .control_at(point)
            .filter(|control| self.control_enabled(*control));
        self.hovered = self.control_at(point);
    }
    fn release_control(&mut self, point: Point) -> Option<RecordingControl> {
        self.drag = None;
        self.pressed.take().filter(|control| {
            self.control_at(point) == Some(*control) && self.control_enabled(*control)
        })
    }
    fn cancel_pointer(&mut self) -> bool {
        let dragging = self.drag.take().is_some();
        let pressing = self.pressed.take().is_some();
        dragging || pressing
    }
    fn toolbar_canvas(&mut self) -> Result<(UiCanvas, Rect)> {
        let s = self.scale;
        let label = if self.active {
            let seconds = self.elapsed().as_secs();
            format!("{:02}:{:02}", seconds / 60, seconds % 60)
        } else {
            format!(
                "{} × {} px",
                self.region.width as u32, self.region.height as u32
            )
        };
        let text = render_ui_text(&label, 15. * s, true, 0xff1e293b)?;
        let layout = self.toolbar_layout(f64::from(text.width()));
        let s = layout.scale;
        let label_scale =
            ((layout.status.width - 30. * s) / f64::from(text.width())).min(s / self.scale);
        let text = render_ui_text(&label, 15. * self.scale * label_scale, true, 0xff1e293b)?;
        if self.position.is_some() {
            self.position = Some(Point {
                x: layout.card.x,
                y: layout.card.y,
            });
        }
        let mut parts = vec![layout.card];
        let mut tooltips = Vec::new();
        for &(button, control) in &layout.buttons {
            let text = render_ui_text(&self.control_title(control), 16. * s, true, 0xffffffff)?;
            let bounds = recording_tooltip_bounds(&text, button, layout.card, s, self.work);
            parts.push(bounds);
            tooltips.push((control, text, bounds));
        }
        // Reserve every tooltip even when idle, so hover never moves the window or its hit targets.
        let bounds = local_canvas_bounds(&parts, self.work, 6. * s)
            .context("Recording toolbar is outside its monitor")?;
        let offset = |r: Rect| ui_rect(r.x - bounds.x, r.y - bounds.y, r.width, r.height);
        let mut ui = UiCanvas::new(bounds.width as u32, bounds.height as u32)?;
        self.card = offset(layout.card);
        ui.panel_scaled(self.card, 10. * s, s);
        let status = offset(layout.status);
        ui.rounded(status, 7. * s, 0xffe2e8f0, None);
        let radius = if self.active && self.pause_started.is_none() {
            (4. + 0.8 * (self.elapsed().as_secs_f64() * std::f64::consts::PI).sin()) * s
        } else {
            4. * s
        };
        ui.circle(
            status.x + 13. * s,
            status.y + status.height / 2.,
            radius,
            if !self.active {
                0xff1677ff
            } else if self.pause_started.is_some() {
                0xfff59e0b
            } else {
                0xfff5222d
            },
            None,
        );
        ui.centered(
            &text,
            ui_rect(
                status.x + 24. * s,
                status.y,
                status.width - 30. * s,
                status.height,
            ),
        );
        for x in layout.separators {
            ui.line(
                &[
                    (x - bounds.x, self.card.y + 14. * s),
                    (x - bounds.x, self.card.y + 38. * s),
                ],
                0xffcbd5e1,
                s as f32,
            );
        }
        self.buttons = layout
            .buttons
            .iter()
            .map(|&(r, c)| (offset(r), c))
            .collect();
        for &(cell, control) in &self.buttons {
            let enabled = self.control_enabled(control);
            let selected = match control {
                RecordingControl::SystemAudio => self.system_audio,
                RecordingControl::Microphone => self.microphone,
                RecordingControl::Stop => true,
                RecordingControl::Pause => self.pause_started.is_some(),
                _ => false,
            };
            let hovered = self.hovered == Some(control);
            let background = if enabled && hovered && self.pressed == Some(control) {
                Some(0xffcbd5e1)
            } else if enabled && hovered {
                Some(0xffe2e8f0)
            } else if control == RecordingControl::Begin {
                Some(0xfffee2e2)
            } else if selected {
                Some(0xffdbeafe)
            } else if control == RecordingControl::Format {
                Some(0xffe2e8f0)
            } else {
                None
            };
            if let Some(color) = background {
                let background_rect = if control == RecordingControl::Format {
                    ui_rect(cell.x, cell.y + 2. * s, cell.width, 36. * s)
                } else {
                    cell
                };
                ui.rounded(background_rect, 6. * s, color, None);
            }
            let color = if !enabled {
                0xffcbd5e1
            } else if control == RecordingControl::Begin {
                0xfff5222d
            } else if selected {
                0xff1677ff
            } else {
                0xff334155
            };
            if control == RecordingControl::Format {
                ui.centered(
                    &render_ui_text(
                        ["MP4", "GIF", "WebP"][self.encode_format as usize],
                        14. * s,
                        true,
                        if enabled { 0xff1e293b } else { 0xff94a3b8 },
                    )?,
                    cell,
                );
            } else {
                self.control_icon(&mut ui, control, cell.x + 8. * s, cell.y + 8. * s, color, s);
            }
        }
        if let Some((_, text, rect)) = tooltips
            .iter()
            .find(|(control, _, _)| self.hovered == Some(*control))
            && self.drag.is_none()
        {
            ui.tooltip_box(text, offset(*rect), s);
        }
        Ok((ui, bounds))
    }
    fn control_icon(
        &self,
        ui: &mut UiCanvas,
        action: RecordingControl,
        x: f64,
        y: f64,
        color: u32,
        s: f64,
    ) {
        let line = |ui: &mut UiCanvas, points: &[(f64, f64)]| {
            ui.line(
                &points
                    .iter()
                    .map(|(a, b)| (x + a * s, y + b * s))
                    .collect::<Vec<_>>(),
                color,
                2.1 * s as f32,
            );
        };
        match action {
            RecordingControl::Begin => ui.circle(x + 12. * s, y + 12. * s, 7. * s, color, None),
            RecordingControl::Cancel => ui.icon_scaled(ToolbarIcon::Cancel, x, y, s, color),
            RecordingControl::Stop => ui.rounded(
                ui_rect(x + 5. * s, y + 5. * s, 14. * s, 14. * s),
                2. * s,
                color,
                None,
            ),
            RecordingControl::Pause if self.pause_started.is_some() => {
                let mut path = tiny_skia::PathBuilder::new();
                path.move_to((x + 7. * s) as f32, (y + 4. * s) as f32);
                path.line_to((x + 20. * s) as f32, (y + 12. * s) as f32);
                path.line_to((x + 7. * s) as f32, (y + 20. * s) as f32);
                path.close();
                if let Some(path) = path.finish() {
                    ui.path(path, color, None);
                }
            }
            RecordingControl::Pause => {
                for dx in [6., 14.] {
                    ui.rounded(
                        ui_rect(x + dx * s, y + 5. * s, 4. * s, 14. * s),
                        s,
                        color,
                        None,
                    );
                }
            }
            RecordingControl::SystemAudio => {
                line(
                    ui,
                    &[
                        (3., 9.),
                        (7., 9.),
                        (12., 5.),
                        (12., 19.),
                        (7., 15.),
                        (3., 15.),
                        (3., 9.),
                    ],
                );
                if self.system_audio {
                    line(ui, &[(16., 8.), (18., 12.), (16., 16.)]);
                    line(ui, &[(19., 5.), (22., 12.), (19., 19.)]);
                } else {
                    line(ui, &[(3., 3.), (21., 21.)]);
                }
            }
            RecordingControl::Microphone => {
                ui.rounded(
                    ui_rect(x + 9. * s, y + 3. * s, 6. * s, 12. * s),
                    3. * s,
                    color,
                    Some(2.1 * s as f32),
                );
                line(
                    ui,
                    &[(5., 11.), (6., 16.), (12., 19.), (18., 16.), (19., 11.)],
                );
                line(ui, &[(12., 19.), (12., 22.)]);
                if !self.microphone {
                    line(ui, &[(3., 3.), (21., 21.)]);
                }
            }
            RecordingControl::Format => {}
        }
    }
    fn activate(&mut self, control: RecordingControl) -> Result<()> {
        if !self.control_enabled(control) {
            return Ok(());
        }
        match control {
            RecordingControl::SystemAudio => self.system_audio = !self.system_audio,
            RecordingControl::Microphone => self.microphone = !self.microphone,
            RecordingControl::Format => self.encode_format = (self.encode_format + 1) % 3,
            RecordingControl::Begin => {
                self.pending = true;
                self.emit(crate::VisionEvent::RecordBegin {
                    region: self.region,
                    system_audio: self.system_audio,
                    microphone: self.microphone,
                    encode_format: self.encode_format,
                });
            }
            RecordingControl::Pause => {
                if let Some(start) = self.pause_started.take() {
                    self.paused_elapsed += start.elapsed();
                } else {
                    self.pause_started = Some(Instant::now());
                }
                self.emit(crate::VisionEvent::RecordPause(
                    self.pause_started.is_some(),
                ));
            }
            RecordingControl::Stop => {
                self.pending = true;
                self.emit(crate::VisionEvent::RecordStop);
            }
            RecordingControl::Cancel => {
                self.closed = true;
                self.cancel_pointer();
                self.hovered = None;
                self.emit(crate::VisionEvent::RecordCancel);
                unsafe {
                    let _ = KillTimer(Some(self.toolbar), 1);
                    let _ = ShowWindow(self.frame, SW_HIDE);
                    let _ = ShowWindow(self.toolbar, SW_HIDE);
                }
                return Ok(());
            }
        }
        self.draw_toolbar()
    }
}

unsafe extern "system" fn recording_hud_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<RecordingHudState>;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let Ok(mut state) = (*ptr).try_borrow_mut() else {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        };
        if hwnd == state.frame {
            return match message {
                WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
                WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
                _ => DefWindowProcW(hwnd, message, wparam, lparam),
            };
        }
        if state.closed {
            drop(state);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_NCHITTEST {
            let mut point = POINT {
                x: lparam.0 as u16 as i16 as i32,
                y: (lparam.0 >> 16) as u16 as i16 as i32,
            };
            let _ = ScreenToClient(hwnd, &mut point);
            return LRESULT(
                if state.card.contains(Point {
                    x: f64::from(point.x),
                    y: f64::from(point.y),
                }) {
                    HTCLIENT as isize
                } else {
                    HTTRANSPARENT as isize
                },
            );
        }
        let point = Point {
            x: (lparam.0 as u16 as i16) as f64,
            y: ((lparam.0 >> 16) as u16 as i16) as f64,
        };
        let mut release_capture = false;
        let result = (|| -> Result<bool> {
            match message {
                WM_TIMER if wparam.0 == 1 => {
                    if !state.closed {
                        state.draw_toolbar()?;
                    }
                }
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    BeginPaint(hwnd, &mut paint);
                    let result = state.draw_toolbar();
                    let _ = EndPaint(hwnd, &paint);
                    result?;
                }
                WM_LBUTTONDOWN => {
                    if !state.card.contains(point) {
                        return Ok(true);
                    }
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(Some(hwnd));
                    state.press_control(point);
                    if state.control_at(point).is_none() {
                        let mut cursor = POINT::default();
                        let mut rect = RECT::default();
                        GetCursorPos(&mut cursor)?;
                        GetWindowRect(hwnd, &mut rect)?;
                        state.drag = Some((
                            cursor,
                            Point {
                                x: f64::from(rect.left) + state.card.x,
                                y: f64::from(rect.top) + state.card.y,
                            },
                        ));
                    }
                    SetCapture(hwnd);
                    state.draw_toolbar()?;
                }
                WM_MOUSEMOVE => {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut track);
                    if let Some((start, origin)) = state.drag {
                        let mut cursor = POINT::default();
                        GetCursorPos(&mut cursor)?;
                        let position = Point {
                            x: origin.x + f64::from(cursor.x - start.x),
                            y: origin.y + f64::from(cursor.y - start.y),
                        };
                        state.position = Some(position);
                        SetWindowPos(
                            hwnd,
                            None,
                            (position.x - state.card.x).round() as i32,
                            (position.y - state.card.y).round() as i32,
                            0,
                            0,
                            SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
                        )?;
                        state.hovered = None;
                        state.draw_toolbar()?;
                    } else {
                        let hovered = state.control_at(point);
                        if hovered != state.hovered {
                            state.hovered = hovered;
                            state.draw_toolbar()?;
                        }
                    }
                }
                WM_MOUSELEAVE => {
                    if state.hovered.take().is_some() {
                        state.draw_toolbar()?;
                    }
                }
                WM_SETCURSOR => {
                    let cursor = if state
                        .hovered
                        .is_some_and(|control| state.control_enabled(control))
                    {
                        IDC_HAND
                    } else {
                        IDC_ARROW
                    };
                    SetCursor(Some(LoadCursorW(None, cursor)?));
                }
                WM_LBUTTONUP => {
                    release_capture = true;
                    if let Some(action) = state.release_control(point) {
                        state.activate(action)?;
                    } else {
                        state.draw_toolbar()?;
                    }
                }
                WM_CANCELMODE | WM_CAPTURECHANGED => {
                    let changed = state.cancel_pointer();
                    release_capture = message == WM_CANCELMODE;
                    if changed {
                        state.draw_toolbar()?;
                    }
                }
                WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                    release_capture = true;
                    state.activate(RecordingControl::Cancel)?;
                }
                WM_CLOSE => {
                    release_capture = true;
                    state.activate(RecordingControl::Cancel)?;
                }
                WM_ERASEBKGND | WM_MOUSEACTIVATE => {}
                _ => return Ok(false),
            }
            Ok(true)
        })();
        let handled = match result {
            Ok(handled) => handled,
            Err(error) => {
                state.emit(crate::VisionEvent::Error(format!(
                    "Recording controls: {error:#}"
                )));
                true
            }
        };
        drop(state);
        if release_capture && GetCapture() == hwnd {
            let _ = ReleaseCapture();
        }
        match message {
            WM_MOUSEACTIVATE => LRESULT(MA_ACTIVATE as isize),
            WM_ERASEBKGND => LRESULT(1),
            _ if handled => LRESULT(0),
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}
