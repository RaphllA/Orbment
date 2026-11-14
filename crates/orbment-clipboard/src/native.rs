//! The native boundary owns all clipboard handles and the STA's message-only window.
use crate::{ClipboardStore, content, types::*};
use anyhow::{Context, Result, ensure};
use image::{DynamicImage, ImageFormat, RgbaImage};
use orbment_core::{config::ClipboardConfig, i18n::tr};
use std::{
    ffi::c_void,
    io::Cursor,
    mem::{size_of, zeroed},
    path::Path,
    ptr::{null, null_mut},
    thread,
    time::Duration,
};

pub(crate) type Hwnd = isize;
pub(crate) const WM_COMMAND: u32 = 0x8001;
pub(crate) const WM_CLIPBOARDUPDATE: u32 = 0x031d;
pub(crate) const WM_TIMER: u32 = 0x0113;
pub(crate) const WM_CLOSE: u32 = 0x0010;
pub(crate) const CF_UNICODETEXT: u32 = 13;
pub(crate) const CF_HDROP: u32 = 15;
pub(crate) const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
pub(crate) const INTERNAL: &str = "Orbment.InternalClipboardWrite";

#[repr(C)]
#[derive(Default)]
pub(crate) struct Point {
    pub x: i32,
    pub y: i32,
}
#[repr(C)]
#[derive(Default)]
pub(crate) struct Message {
    pub hwnd: Hwnd,
    pub message: u32,
    pub wparam: usize,
    pub lparam: isize,
    pub time: u32,
    pub point: Point,
    pub private: u32,
}
#[repr(C)]
struct WindowClass {
    style: u32,
    proc: Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>,
    class_extra: i32,
    window_extra: i32,
    instance: isize,
    icon: isize,
    cursor: isize,
    background: isize,
    menu: *const u16,
    name: *const u16,
}
#[repr(C)]
pub(crate) struct CreateStruct {
    pub params: *mut c_void,
    instance: isize,
    menu: isize,
    parent: Hwnd,
    cy: i32,
    cx: i32,
    y: i32,
    x: i32,
    style: i32,
    name: *const u16,
    class: *const u16,
    ex_style: u32,
}
#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    image_size: u32,
    x_pels: i32,
    y_pels: i32,
    colors_used: u32,
    colors_important: u32,
}
#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 3],
}
#[repr(C)]
struct IconInfo {
    icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    mask: isize,
    color: isize,
}
#[repr(C)]
struct Bitmap {
    kind: i32,
    width: i32,
    height: i32,
    width_bytes: i32,
    planes: u16,
    bits_pixel: u16,
    bits: *mut c_void,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassW(class: *const WindowClass) -> u16;
    fn UnregisterClassW(name: *const u16, instance: isize) -> i32;
    fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Hwnd,
        menu: isize,
        instance: isize,
        param: *mut c_void,
    ) -> Hwnd;
    pub(crate) fn DefWindowProcW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    pub(crate) fn SetWindowLongPtrW(hwnd: Hwnd, index: i32, value: isize) -> isize;
    pub(crate) fn GetWindowLongPtrW(hwnd: Hwnd, index: i32) -> isize;
    pub(crate) fn DestroyWindow(hwnd: Hwnd) -> i32;
    pub(crate) fn PostQuitMessage(code: i32);
    fn AddClipboardFormatListener(hwnd: Hwnd) -> i32;
    pub(crate) fn RemoveClipboardFormatListener(hwnd: Hwnd) -> i32;
    pub(crate) fn GetMessageW(message: *mut Message, hwnd: Hwnd, min: u32, max: u32) -> i32;
    pub(crate) fn TranslateMessage(message: *const Message) -> i32;
    pub(crate) fn DispatchMessageW(message: *const Message) -> isize;
    pub(crate) fn PostMessageW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> i32;
    pub(crate) fn SetTimer(hwnd: Hwnd, id: usize, elapsed: u32, callback: *const c_void) -> usize;
    pub(crate) fn KillTimer(hwnd: Hwnd, id: usize) -> i32;
    fn OpenClipboard(hwnd: Hwnd) -> i32;
    fn CloseClipboard() -> i32;
    fn GetClipboardData(format: u32) -> isize;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn RegisterClipboardFormatW(name: *const u16) -> u32;
    pub(crate) fn GetClipboardSequenceNumber() -> u32;
    pub(crate) fn GetForegroundWindow() -> Hwnd;
    fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
    fn GetIconInfo(icon: isize, info: *mut IconInfo) -> i32;
    fn DestroyIcon(icon: isize) -> i32;
    fn GetDC(hwnd: Hwnd) -> isize;
    fn ReleaseDC(hwnd: Hwnd, dc: isize) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> isize;
    fn GlobalSize(memory: isize) -> usize;
    pub(crate) fn GlobalLock(memory: isize) -> *mut c_void;
    pub(crate) fn GlobalUnlock(memory: isize) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn CloseHandle(handle: isize) -> i32;
    fn QueryFullProcessImageNameW(
        process: isize,
        flags: u32,
        name: *mut u16,
        size: *mut u32,
    ) -> i32;
}
#[link(name = "shell32")]
unsafe extern "system" {
    fn DragQueryFileW(drop: isize, index: u32, path: *mut u16, length: u32) -> u32;
    fn ExtractIconExW(
        path: *const u16,
        index: i32,
        large: *mut isize,
        small: *mut isize,
        count: u32,
    ) -> u32;
}
#[link(name = "gdi32")]
unsafe extern "system" {
    fn GetObjectW(object: isize, size: i32, result: *mut c_void) -> i32;
    fn GetDIBits(
        dc: isize,
        bitmap: isize,
        start: u32,
        lines: u32,
        bits: *mut c_void,
        info: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
    fn DeleteObject(object: isize) -> i32;
}
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *const c_void, flags: u32) -> i32;
    fn CoUninitialize();
}

pub(crate) fn os_error() -> anyhow::Error {
    std::io::Error::last_os_error().into()
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
pub(crate) fn format(name: &str) -> Result<u32> {
    let id = unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) };
    ensure!(id != 0, "{}: {}", tr("clipboard.error.native"), os_error());
    Ok(id)
}
fn available(format: u32) -> bool {
    unsafe { IsClipboardFormatAvailable(format) != 0 }
}
pub(crate) fn limit(mb: u32) -> usize {
    if mb == 0 {
        usize::MAX
    } else {
        (mb as usize).saturating_mul(1024 * 1024)
    }
}

pub(crate) struct Apartment;
impl Apartment {
    pub fn initialize() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(null(), 2) };
        ensure!(hr >= 0, "{}: HRESULT {hr:#x}", tr("clipboard.error.native"));
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

pub(crate) struct Window {
    pub hwnd: Hwnd,
    name: Vec<u16>,
    instance: isize,
}
impl Window {
    pub unsafe fn create(
        proc: unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize,
        state: *mut c_void,
    ) -> Result<Self> {
        let name = wide(&format!("Orbment.Clipboard.{}", uuid::Uuid::new_v4()));
        let instance = unsafe { GetModuleHandleW(null()) };
        let class = WindowClass {
            style: 0,
            proc: Some(proc),
            class_extra: 0,
            window_extra: 0,
            instance,
            icon: 0,
            cursor: 0,
            background: 0,
            menu: null(),
            name: name.as_ptr(),
        };
        ensure!(
            unsafe { RegisterClassW(&class) } != 0,
            "{}: {}",
            tr("clipboard.error.native"),
            os_error()
        );
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                name.as_ptr(),
                name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                -3,
                0,
                instance,
                state,
            )
        };
        if hwnd == 0 {
            unsafe {
                UnregisterClassW(name.as_ptr(), instance);
            }
            return Err(os_error());
        }
        let window = Self {
            hwnd,
            name,
            instance,
        };
        ensure!(
            unsafe { AddClipboardFormatListener(hwnd) } != 0,
            "{}: {}",
            tr("clipboard.error.native"),
            os_error()
        );
        Ok(window)
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            RemoveClipboardFormatListener(self.hwnd);
            DestroyWindow(self.hwnd);
            UnregisterClassW(self.name.as_ptr(), self.instance);
        }
    }
}

pub(crate) struct Clipboard;
impl Clipboard {
    pub(crate) fn open(hwnd: Hwnd) -> Result<Self> {
        for _ in 0..12 {
            if unsafe { OpenClipboard(hwnd) } != 0 {
                return Ok(Self);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(os_error()).context(tr("clipboard.error.busy"))
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}

fn bytes(format: u32, max: usize) -> Result<Vec<u8>> {
    let handle = unsafe { GetClipboardData(format) };
    ensure!(
        handle != 0,
        "{}: {}",
        tr("clipboard.error.native"),
        os_error()
    );
    let size = unsafe { GlobalSize(handle) };
    ensure!(size <= max, "{}", tr("clipboard.error.too_large"));
    if size == 0 {
        return Ok(Vec::new());
    }
    let ptr = unsafe { GlobalLock(handle) };
    ensure!(
        !ptr.is_null(),
        "{}: {}",
        tr("clipboard.error.native"),
        os_error()
    );
    let data = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size).to_vec() };
    unsafe {
        GlobalUnlock(handle);
    }
    Ok(data)
}

fn plain_text(max: usize) -> Result<Option<String>> {
    if !available(CF_UNICODETEXT) {
        return Ok(None);
    }
    let data = bytes(CF_UNICODETEXT, max)?;
    let utf16: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    Ok(Some(String::from_utf16_lossy(&utf16)))
}

fn private_clipboard() -> Result<bool> {
    for name in [
        INTERNAL,
        "ExcludeClipboardContentFromMonitorProcessing",
        "ClipboardViewerIgnore",
    ] {
        if available(format(name)?) {
            return Ok(true);
        }
    }
    let history = format("CanIncludeInClipboardHistory")?;
    if available(history) {
        let data = bytes(history, 4096)?;
        if data
            .get(..4)
            .is_none_or(|v| u32::from_le_bytes(v.try_into().expect("four bytes")) == 0)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) enum Capture {
    Item(Captured),
    Image {
        png: Vec<u8>,
        dimensions: (u32, u32),
        source: Option<SourceApp>,
    },
}

enum Snapshot {
    Text {
        content: String,
        sub_kind: String,
        raw: Option<Vec<u8>>,
    },
    Files(Vec<String>),
    Image(Vec<u8>, bool),
}

pub(crate) fn capture(
    hwnd: Hwnd,
    config: &ClipboardConfig,
    store: &ClipboardStore,
) -> Result<Option<Capture>> {
    let mut source = source_app();
    if source.as_ref().is_some_and(|app| {
        config.filters.excluded_app_ids.iter().any(|excluded| {
            excluded.eq_ignore_ascii_case(&app.executable)
                || excluded.eq_ignore_ascii_case(&app.name)
        })
    }) {
        return Ok(None);
    }
    if let Some(app) = &source
        && store.source_excluded(&app.executable, &config.filters.excluded_app_ids)?
    {
        return Ok(None);
    }
    let snapshot = {
        let _clipboard = Clipboard::open(hwnd)?;
        if private_clipboard()? {
            return Ok(None);
        }
        let mut snapshot = None;
        for kind in &config.capture.order {
            snapshot = match kind.as_str() {
                "files" if config.capture.files && available(CF_HDROP) => {
                    let handle = unsafe { GetClipboardData(CF_HDROP) };
                    ensure!(handle != 0, "{}", tr("clipboard.error.native"));
                    let count = unsafe { DragQueryFileW(handle, u32::MAX, null_mut(), 0) };
                    let mut files = Vec::new();
                    let mut size = 0usize;
                    for index in 0..count {
                        let length = unsafe { DragQueryFileW(handle, index, null_mut(), 0) };
                        size = size.saturating_add((length as usize).saturating_mul(2));
                        ensure!(
                            size <= limit(config.capture.max_text_mb),
                            "{}",
                            tr("clipboard.error.too_large")
                        );
                        let mut path = vec![0u16; length as usize + 1];
                        let written = unsafe {
                            DragQueryFileW(handle, index, path.as_mut_ptr(), path.len() as u32)
                        };
                        if written != 0 {
                            files.push(String::from_utf16_lossy(&path[..written as usize]));
                        }
                    }
                    if files.is_empty() {
                        None
                    } else {
                        Some(Snapshot::Files(files))
                    }
                }
                "image" if config.capture.image => {
                    let png = format("PNG")?;
                    if available(png) {
                        Some(Snapshot::Image(
                            bytes(png, limit(config.capture.max_image_mb))?,
                            false,
                        ))
                    } else if available(CF_DIBV5) {
                        Some(Snapshot::Image(
                            bytes(CF_DIBV5, limit(config.capture.max_image_mb))?,
                            true,
                        ))
                    } else if available(CF_DIB) {
                        Some(Snapshot::Image(
                            bytes(CF_DIB, limit(config.capture.max_image_mb))?,
                            true,
                        ))
                    } else {
                        None
                    }
                }
                "html" if config.capture.html && available(format("HTML Format")?) => {
                    let raw = bytes(format("HTML Format")?, limit(config.capture.max_text_mb))?;
                    let text = match plain_text(limit(config.capture.max_text_mb))? {
                        Some(text) => text,
                        None => content::html_plain(&raw)?,
                    };
                    Some(Snapshot::Text {
                        content: text,
                        sub_kind: "html".into(),
                        raw: Some(raw),
                    })
                }
                "rtf" if config.capture.rtf && available(format("Rich Text Format")?) => {
                    let raw = bytes(
                        format("Rich Text Format")?,
                        limit(config.capture.max_text_mb),
                    )?;
                    let text = match plain_text(limit(config.capture.max_text_mb))? {
                        Some(text) => text,
                        None => content::rtf_plain(&raw)?,
                    };
                    Some(Snapshot::Text {
                        content: text,
                        sub_kind: "rtf".into(),
                        raw: Some(raw),
                    })
                }
                "text" if config.capture.text => plain_text(limit(config.capture.max_text_mb))?
                    .map(|text| Snapshot::Text {
                        sub_kind: content::sub_kind(&text).into(),
                        content: text,
                        raw: None,
                    }),
                _ => None,
            };
            if snapshot.is_some() {
                break;
            }
        }
        snapshot
    };
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    if let Some(app) = &mut source {
        app.icon_file = store.source_icon(&app.executable)?;
        if app.icon_file.is_none() {
            match save_app_icon(app, store) {
                Ok(icon) => app.icon_file = icon,
                Err(error) => tracing::debug!(%error,"clipboard source icon unavailable"),
            }
        }
    }
    Ok(Some(match snapshot {
        Snapshot::Text {
            content,
            sub_kind,
            raw,
        } => {
            if content.is_empty() {
                return Ok(None);
            }
            let search_text = if sub_kind == "html" {
                content::html_plain(raw.as_deref().unwrap_or_default())?
            } else {
                content.clone()
            };
            Capture::Item(Captured {
                size: content.encode_utf16().count() as u64,
                kind: "text".into(),
                sub_kind: Some(sub_kind),
                search_text,
                content,
                raw_payload: raw,
                dimensions: None,
                source,
            })
        }
        Snapshot::Files(files) => {
            let size = files
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .fold(0u64, u64::saturating_add);
            Capture::Item(Captured {
                kind: "files".into(),
                sub_kind: None,
                content: serde_json::to_string(&files)?,
                raw_payload: None,
                search_text: files.join("\n"),
                size,
                dimensions: None,
                source,
            })
        }
        Snapshot::Image(data, dib) => {
            let data = if dib { dib_to_bmp(&data)? } else { data };
            let image = decode_image(&data, limit(config.capture.max_image_mb))?;
            let dimensions = (image.width(), image.height());
            let mut png = Cursor::new(Vec::new());
            image.write_to(&mut png, ImageFormat::Png)?;
            let png = png.into_inner();
            ensure!(
                png.len() <= limit(config.capture.max_image_mb),
                "{}",
                tr("clipboard.error.too_large")
            );
            Capture::Image {
                png,
                dimensions,
                source,
            }
        }
    }))
}

pub(crate) fn decode_image(data: &[u8], max: usize) -> Result<DynamicImage> {
    let reader = image::ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    let (width, height) = reader.into_dimensions()?;
    ensure!(
        u64::from(width)
            .saturating_mul(u64::from(height))
            .saturating_mul(4)
            <= max as u64,
        "{}",
        tr("clipboard.error.too_large")
    );
    let mut reader = image::ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = if max == usize::MAX {
        None
    } else {
        Some(max as u64)
    };
    reader.limits(limits);
    Ok(reader.decode()?)
}

fn dib_to_bmp(dib: &[u8]) -> Result<Vec<u8>> {
    ensure!(dib.len() >= 40, "{}", tr("clipboard.error.invalid_image"));
    let dword = |offset: usize| {
        u32::from_le_bytes(
            dib[offset..offset + 4]
                .try_into()
                .expect("bounded DIB field"),
        )
    };
    let header = dword(0) as usize;
    ensure!(
        header >= 40 && header <= dib.len(),
        "{}",
        tr("clipboard.error.invalid_image")
    );
    let bits = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = dword(16);
    let used = dword(32) as usize;
    let colors = if used != 0 {
        used
    } else if bits <= 8 {
        1usize << bits
    } else {
        0
    };
    let masks = if header == 40 && compression == 3 {
        12
    } else if header == 40 && compression == 6 {
        16
    } else {
        0
    };
    let offset = header
        .saturating_add(masks)
        .saturating_add(colors.saturating_mul(4));
    ensure!(
        offset <= dib.len() && dib.len() <= u32::MAX as usize - 14,
        "{}",
        tr("clipboard.error.invalid_image")
    );
    let mut bmp = Vec::with_capacity(dib.len() + 14);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((dib.len() + 14) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&((offset + 14) as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    Ok(bmp)
}

fn source_app() -> Option<SourceApp> {
    let hwnd = unsafe { GetForegroundWindow() };
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    let process = unsafe { OpenProcess(0x1000, 0, pid) };
    if process == 0 {
        return None;
    }
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    let success = unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) };
    unsafe {
        CloseHandle(process);
    }
    if success == 0 {
        return None;
    }
    let executable = String::from_utf16_lossy(&path[..length as usize]);
    let name = Path::new(&executable)
        .file_name()?
        .to_string_lossy()
        .into_owned();
    Some(SourceApp {
        executable,
        name,
        icon_file: None,
    })
}

fn save_app_icon(source: &SourceApp, store: &ClipboardStore) -> Result<Option<String>> {
    let mut icon = 0;
    if unsafe {
        ExtractIconExW(
            wide(&source.executable).as_ptr(),
            0,
            &mut icon,
            null_mut(),
            1,
        )
    } == 0
        || icon == 0
    {
        return Ok(None);
    }
    let result = icon_png(icon);
    unsafe {
        DestroyIcon(icon);
    }
    let Some(png) = result? else {
        return Ok(None);
    };
    Ok(Some(store.save_source_icon(source, &png)?))
}

fn icon_png(icon: isize) -> Result<Option<Vec<u8>>> {
    let mut info: IconInfo = unsafe { zeroed() };
    if unsafe { GetIconInfo(icon, &mut info) } == 0 {
        return Ok(None);
    }
    let result = (|| {
        if info.color == 0 {
            return Ok(None);
        }
        let mut bitmap: Bitmap = unsafe { zeroed() };
        if unsafe {
            GetObjectW(
                info.color,
                size_of::<Bitmap>() as i32,
                (&mut bitmap as *mut Bitmap).cast(),
            )
        } == 0
        {
            return Err(os_error());
        }
        ensure!(
            bitmap.width > 0 && bitmap.height > 0 && bitmap.width <= 1024 && bitmap.height <= 1024,
            "{}",
            tr("clipboard.error.invalid_image")
        );
        let mut pixels = vec![0u8; bitmap.width as usize * bitmap.height as usize * 4];
        let mut description: BitmapInfo = unsafe { zeroed() };
        description.header = BitmapInfoHeader {
            size: 40,
            width: bitmap.width,
            height: -bitmap.height,
            planes: 1,
            bit_count: 32,
            compression: 0,
            image_size: 0,
            x_pels: 0,
            y_pels: 0,
            colors_used: 0,
            colors_important: 0,
        };
        let dc = unsafe { GetDC(0) };
        if dc == 0 {
            return Err(os_error());
        }
        let result = unsafe {
            GetDIBits(
                dc,
                info.color,
                0,
                bitmap.height as u32,
                pixels.as_mut_ptr().cast(),
                &mut description,
                0,
            )
        };
        unsafe {
            ReleaseDC(0, dc);
        }
        ensure!(
            result != 0,
            "{}: {}",
            tr("clipboard.error.native"),
            os_error()
        );
        let has_alpha = pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
            if !has_alpha {
                pixel[3] = 255;
            }
        }
        let image = RgbaImage::from_raw(bitmap.width as u32, bitmap.height as u32, pixels)
            .context(tr("clipboard.error.invalid_image"))?;
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image).write_to(&mut png, ImageFormat::Png)?;
        Ok(Some(png.into_inner()))
    })();
    unsafe {
        if info.color != 0 {
            DeleteObject(info.color);
        }
        if info.mask != 0 {
            DeleteObject(info.mask);
        }
    }
    result
}
