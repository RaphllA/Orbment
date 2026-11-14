use crate::event_loop::TRAY_MESSAGE;
use anyhow::{Result, ensure};
use orbment_core::config::MemoryTrayIconConfig;
use std::{cell::Cell, marker::PhantomData, mem::size_of, rc::Rc};
#[cfg(test)]
use tests::shell_notify_icon as Shell_NotifyIconW;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::PCWSTR,
};

pub(crate) fn application_icon(width: i32, height: i32) -> Result<HICON> {
    unsafe {
        let icon = GetModuleHandleW(None).and_then(|instance| {
            LoadImageW(
                Some(instance.into()),
                PCWSTR(std::ptr::without_provenance(1)),
                IMAGE_ICON,
                width,
                height,
                LR_DEFAULTCOLOR | LR_SHARED,
            )
            .map(|icon| HICON(icon.0))
        });
        // Test executables and other hosts may not embed the application resources.
        Ok(icon.or_else(|_| LoadIconW(None, IDI_APPLICATION))?)
    }
}

pub struct MenuItem {
    pub label: String,
    pub action: String,
    pub checked: bool,
}

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    id: u32,
    tooltip: String,
    owned_icon: bool,
    registered: Cell<bool>,
    _main_thread: PhantomData<Rc<()>>,
}

impl Tray {
    pub fn new(hwnd: HWND) -> Result<Self> {
        Self::new_with_id(hwnd, 1)
    }

    pub fn new_with_id(hwnd: HWND, id: u32) -> Result<Self> {
        let tray = Self {
            hwnd,
            icon: unsafe {
                application_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))?
            },
            id,
            tooltip: orbment_core::i18n::tr("ui.tray_tooltip"),
            owned_icon: false,
            registered: Cell::new(false),
            _main_thread: PhantomData,
        };
        if let Err(error) = tray.readd() {
            tracing::warn!("tray registration deferred until Explorer is ready: {error:#}");
        }
        Ok(tray)
    }

    pub fn set_tooltip(&mut self, text: &str) -> Result<()> {
        self.tooltip = text.into();
        let mut data = self.data();
        data.uFlags = NIF_TIP;
        copy_text(&mut data.szTip, &self.tooltip);
        self.modify_or_readd(&data);
        Ok(())
    }

    pub fn set_icon_path(&mut self, path: &str) -> Result<()> {
        let (icon, owned) = if path.is_empty() {
            (
                unsafe {
                    application_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))?
                },
                false,
            )
        } else {
            let (file, index) = path
                .rsplit_once(',')
                .and_then(|(file, index)| {
                    index.trim().parse::<i32>().ok().map(|index| (file, index))
                })
                .unwrap_or((path, 0));
            let file = crate::wide(file.trim().trim_start_matches('@').trim_matches('"'));
            let mut expanded = vec![0_u16; 32768];
            let length = unsafe {
                windows::Win32::System::Environment::ExpandEnvironmentStringsW(
                    PCWSTR(file.as_ptr()),
                    Some(&mut expanded),
                )
            };
            ensure!(
                length > 0 && length as usize <= expanded.len(),
                "Unable to expand icon resource path"
            );
            let mut icon = HICON::default();
            let count = unsafe {
                ExtractIconExW(PCWSTR(expanded.as_ptr()), index, None, Some(&mut icon), 1)
            };
            ensure!(
                count > 0 && !icon.is_invalid(),
                "Unable to extract audio device icon"
            );
            (icon, true)
        };
        self.replace_icon(icon, owned)
    }

    pub fn set_memory_icon(&mut self, percent: u32, config: &MemoryTrayIconConfig) -> Result<()> {
        let icon = render_memory_icon(percent.min(100), config)?;
        self.replace_icon(icon, true)
    }

    fn replace_icon(&mut self, icon: HICON, owned: bool) -> Result<()> {
        if self.owned_icon {
            unsafe {
                let _ = DestroyIcon(self.icon);
            }
        }
        self.icon = icon;
        self.owned_icon = owned;
        let mut data = self.data();
        data.uFlags = NIF_ICON;
        data.hIcon = self.icon;
        self.modify_or_readd(&data);
        Ok(())
    }

    fn modify_or_readd(&self, data: &NOTIFYICONDATAW) {
        if self.registered.get() && unsafe { Shell_NotifyIconW(NIM_MODIFY, data).as_bool() } {
            return;
        }
        if let Err(error) = self.readd() {
            tracing::warn!("tray update deferred until Explorer is ready: {error:#}");
        }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: self.id,
            ..Default::default()
        }
    }

    pub fn readd(&self) -> Result<()> {
        self.registered.set(false);
        let mut data = self.data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = TRAY_MESSAGE;
        data.hIcon = self.icon;
        copy_text(&mut data.szTip, &self.tooltip);
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &data).ok()?;
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            if let Err(error) = Shell_NotifyIconW(NIM_SETVERSION, &data).ok() {
                // Remove the partial registration so the next NIM_ADD can retry.
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                return Err(error.into());
            }
        }
        self.registered.set(true);
        Ok(())
    }

    pub fn notify(&self, title: &str, text: &str) -> Result<()> {
        self.notify_with_sound(title, text, true)
    }

    pub fn notify_with_sound(&self, title: &str, text: &str, sound: bool) -> Result<()> {
        let mut data = self.data();
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_INFO;
        if !sound {
            data.dwInfoFlags |= NIIF_NOSOUND;
        }
        copy_text(&mut data.szInfoTitle, title);
        copy_text(&mut data.szInfo, text);
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &data).ok()?;
        }
        Ok(())
    }

    pub fn show_menu(&self, items: &[MenuItem]) -> Result<Option<String>> {
        ensure!(items.len() < u16::MAX as usize, "Too many tray menu items");
        let menu = Menu(unsafe { CreatePopupMenu()? });
        for (index, item) in items.iter().enumerate() {
            if item.label == "-" || item.action.is_empty() {
                unsafe {
                    AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())?;
                }
                continue;
            }
            let text = crate::wide(&item.label);
            let flags = MF_STRING
                | if item.checked {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
            unsafe {
                AppendMenuW(menu.0, flags, index + 1, PCWSTR(text.as_ptr()))?;
            }
        }
        let mut point = POINT::default();
        unsafe {
            GetCursorPos(&mut point)?;
            let _ = SetForegroundWindow(self.hwnd);
            let command = TrackPopupMenu(
                menu.0,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                point.x,
                point.y,
                None,
                self.hwnd,
                None,
            )
            .0 as usize;
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            Ok(command
                .checked_sub(1)
                .and_then(|index| items.get(index))
                .map(|item| item.action.clone()))
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            if self.owned_icon {
                let _ = DestroyIcon(self.icon);
            }
        }
    }
}

struct IconBitmap {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u32,
}

impl IconBitmap {
    fn new(width: i32, height: i32) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            ensure!(!dc.is_invalid(), "Unable to create memory icon DC");
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
                anyhow::bail!("Unable to select memory icon bitmap");
            }
            std::ptr::write_bytes(pixels.cast::<u8>(), 0, (width * height * 4) as usize);
            Ok(Self {
                dc,
                bitmap,
                previous,
                pixels: pixels.cast(),
            })
        }
    }
}

impl Drop for IconBitmap {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn render_memory_icon(percent: u32, config: &MemoryTrayIconConfig) -> Result<HICON> {
    unsafe {
        let width = GetSystemMetrics(SM_CXSMICON).max(16);
        let height = GetSystemMetrics(SM_CYSMICON).max(16);
        let scale = if config.antialias { 4 } else { 1 };
        let text = IconBitmap::new(width * scale, height * scale)?;
        let output = IconBitmap::new(width, height)?;
        let font_name = crate::wide(&config.font);
        let mut digits: Vec<u16> = percent.to_string().encode_utf16().collect();
        let font = CreateFontW(
            -(height - 2) * scale,
            if digits.len() == 3 {
                (width - 2) * scale / 3
            } else {
                0
            },
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            NONANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(font_name.as_ptr()),
        );
        ensure!(!font.is_invalid(), "Unable to create memory icon font");
        let previous_font = SelectObject(text.dc, HGDIOBJ(font.0));
        if previous_font.is_invalid() || previous_font.0 as isize == -1 {
            let _ = DeleteObject(HGDIOBJ(font.0));
            anyhow::bail!("Unable to select memory icon font");
        }
        SetTextColor(text.dc, COLORREF(0x00ffffff));
        SetBkMode(text.dc, TRANSPARENT);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width * scale,
            bottom: height * scale,
        };
        let drawn = DrawTextW(
            text.dc,
            &mut digits,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        // GDI batches writes; flush before reading DIB pixels for alpha compositing.
        let flushed = GdiFlush();
        SelectObject(text.dc, previous_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        ensure!(
            drawn != 0 && flushed.as_bool(),
            "Unable to draw memory icon text"
        );

        let background = if percent >= config.danger_level_pct as u32 {
            config.danger_color
        } else if percent >= config.warn_level_pct as u32 {
            config.warn_color
        } else {
            config.bg_color
        };
        let foreground = if config.transparent_bg && percent >= config.warn_level_pct as u32 {
            background
        } else {
            config.text_color
        };
        let bg = premultiplied_argb(if config.transparent_bg { 0 } else { background });
        let fg = premultiplied_argb(foreground);
        let text_pixels =
            std::slice::from_raw_parts(text.pixels, (width * height * scale * scale) as usize);
        let pixels = std::slice::from_raw_parts_mut(output.pixels, (width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let mut sum = [0_u32; 4];
                for sy in 0..scale {
                    for sx in 0..scale {
                        let px = x as f32 + (sx as f32 + 0.5) / scale as f32;
                        let py = y as f32 + (sy as f32 + 0.5) / scale as f32;
                        let radius = if config.round_corners {
                            width.min(height) as f32 / 4.0
                        } else {
                            0.0
                        };
                        if !inside_icon(px, py, width as f32, height as f32, radius, 0.0) {
                            continue;
                        }
                        let border = config.show_border
                            && !inside_icon(px, py, width as f32, height as f32, radius, 1.0);
                        let coverage = if border {
                            255
                        } else {
                            text_pixels
                                [((y * scale + sy) * width * scale + x * scale + sx) as usize]
                                & 255
                        };
                        let alpha = fg[3] * coverage / 255;
                        for channel in 0..4 {
                            sum[channel] +=
                                fg[channel] * coverage / 255 + bg[channel] * (255 - alpha) / 255;
                        }
                    }
                }
                let count = (scale * scale) as u32;
                pixels[(y * width + x) as usize] = (sum[0] / count)
                    | ((sum[1] / count) << 8)
                    | ((sum[2] / count) << 16)
                    | ((sum[3] / count) << 24);
            }
        }
        let stride = ((width + 15) / 16) * 2;
        let mut mask_bytes = vec![0_u8; (stride * height) as usize];
        for y in 0..height {
            for x in 0..width {
                if pixels[(y * width + x) as usize] >> 24 == 0 {
                    mask_bytes[(y * stride + x / 8) as usize] |= 0x80 >> (x % 8);
                }
            }
        }
        let mask = CreateBitmap(width, height, 1, 1, Some(mask_bytes.as_ptr().cast()));
        ensure!(!mask.is_invalid(), "Unable to create memory icon mask");
        SelectObject(output.dc, output.previous);
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: output.bitmap,
            ..Default::default()
        });
        let _ = DeleteObject(HGDIOBJ(mask.0));
        Ok(icon?)
    }
}

fn premultiplied_argb(color: u32) -> [u32; 4] {
    let alpha = color >> 24;
    [
        (color & 255) * alpha / 255,
        ((color >> 8) & 255) * alpha / 255,
        ((color >> 16) & 255) * alpha / 255,
        alpha,
    ]
}

fn inside_icon(x: f32, y: f32, width: f32, height: f32, radius: f32, inset: f32) -> bool {
    if x < inset || y < inset || x >= width - inset || y >= height - inset {
        return false;
    }
    let radius = (radius - inset).max(0.0);
    let center_x = x.clamp(inset + radius, width - inset - radius);
    let center_y = y.clamp(inset + radius, height - inset - radius);
    (x - center_x).powi(2) + (y - center_y).powi(2) <= radius * radius
}

struct Menu(HMENU);
impl Drop for Menu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

fn copy_text<const N: usize>(destination: &mut [u16; N], text: &str) {
    let mut position = 0;
    for character in text.chars() {
        let mut units = [0; 2];
        let units = character.encode_utf16(&mut units);
        if position + units.len() >= N {
            break;
        }
        destination[position..position + units.len()].copy_from_slice(units);
        position += units.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};
    use windows::core::BOOL;

    struct ShellMock {
        replies: VecDeque<(NOTIFY_ICON_MESSAGE, bool)>,
        additions: Vec<(HICON, String)>,
    }

    thread_local! {
        static SHELL: RefCell<Option<ShellMock>> = const { RefCell::new(None) };
    }

    struct ResetShell;

    impl Drop for ResetShell {
        fn drop(&mut self) {
            SHELL.set(None);
        }
    }

    fn with_shell(replies: &[(NOTIFY_ICON_MESSAGE, bool)], test: impl FnOnce()) {
        SHELL.set(Some(ShellMock {
            replies: replies.iter().copied().collect(),
            additions: Vec::new(),
        }));
        let _reset = ResetShell;
        test();
        SHELL.with_borrow(|shell| assert!(shell.as_ref().unwrap().replies.is_empty()));
    }

    pub(super) unsafe fn shell_notify_icon(
        command: NOTIFY_ICON_MESSAGE,
        data: *const NOTIFYICONDATAW,
    ) -> BOOL {
        if std::thread::panicking() {
            return BOOL(1);
        }
        SHELL.with_borrow_mut(|shell| {
            let Some(shell) = shell else {
                return unsafe { windows::Win32::UI::Shell::Shell_NotifyIconW(command, data) };
            };
            let data = unsafe { &*data };
            if command == NIM_ADD {
                let length = data.szTip.iter().position(|&unit| unit == 0).unwrap();
                shell.additions.push((
                    data.hIcon,
                    String::from_utf16(&data.szTip[..length]).unwrap(),
                ));
            }
            if command == NIM_DELETE && shell.replies.is_empty() {
                return BOOL(1);
            }
            let (expected, success) = shell.replies.pop_front().expect("unexpected shell call");
            assert_eq!(command, expected);
            BOOL::from(success)
        })
    }

    #[test]
    fn empty_icon_path_restores_shared_application_icon() {
        with_shell(
            &[
                (NIM_ADD, true),
                (NIM_SETVERSION, true),
                (NIM_MODIFY, true),
                (NIM_MODIFY, true),
            ],
            || {
                let mut tray = Tray::new(HWND::default()).unwrap();
                let application_icon = tray.icon;
                let owned_icon = unsafe { CopyIcon(application_icon).unwrap() };
                tray.replace_icon(owned_icon, true).unwrap();
                tray.set_icon_path("").unwrap();
                assert_eq!(tray.icon, application_icon);
                assert!(!tray.owned_icon);
                assert!(unsafe { CopyIcon(owned_icon) }.is_err());

                drop(tray);
                let copy = unsafe { CopyIcon(application_icon).unwrap() };
                unsafe { DestroyIcon(copy).unwrap() };
            },
        );
    }

    #[test]
    fn unavailable_shell_preserves_latest_tooltip_and_owned_icon_for_readd() {
        with_shell(
            &[
                (NIM_ADD, false),
                (NIM_ADD, false),
                (NIM_ADD, false),
                (NIM_ADD, false),
                (NIM_ADD, true),
                (NIM_SETVERSION, true),
                (NIM_MODIFY, true),
            ],
            || {
                let mut tray = Tray::new_with_id(HWND::default(), 3).unwrap();
                assert!(!tray.registered.get());
                tray.set_tooltip("Memory: 42%").unwrap();
                let old_icon = unsafe { CopyIcon(tray.icon).unwrap() };
                tray.replace_icon(old_icon, true).unwrap();
                let icon = unsafe { CopyIcon(old_icon).unwrap() };
                tray.replace_icon(icon, true).unwrap();
                assert!(unsafe { CopyIcon(old_icon) }.is_err());
                assert_eq!(tray.icon, icon);
                assert!(tray.owned_icon);
                assert!(!tray.registered.get());
                assert_eq!(tray.tooltip, "Memory: 42%");

                tray.readd().unwrap();
                assert!(tray.registered.get());
                SHELL.with_borrow(|shell| {
                    assert_eq!(
                        shell.as_ref().unwrap().additions.last(),
                        Some(&(icon, "Memory: 42%".into()))
                    );
                });
                tray.set_tooltip("Memory: 43%").unwrap();
                assert!(tray.registered.get());
            },
        );
    }

    #[test]
    fn failed_modifications_readd_and_retry_with_latest_state() {
        with_shell(
            &[
                (NIM_ADD, true),
                (NIM_SETVERSION, true),
                (NIM_MODIFY, false),
                (NIM_ADD, true),
                (NIM_SETVERSION, true),
                (NIM_MODIFY, false),
                (NIM_ADD, false),
                (NIM_ADD, true),
                (NIM_SETVERSION, true),
            ],
            || {
                let mut tray = Tray::new(HWND::default()).unwrap();
                assert!(tray.registered.get());
                tray.set_tooltip("First update").unwrap();
                assert!(tray.registered.get());
                let icon = unsafe { CopyIcon(tray.icon).unwrap() };
                tray.replace_icon(icon, true).unwrap();
                assert!(!tray.registered.get());
                assert_eq!(tray.icon, icon);
                tray.set_tooltip("Latest update").unwrap();
                assert!(tray.registered.get());
                SHELL.with_borrow(|shell| {
                    let shell = shell.as_ref().unwrap();
                    assert_eq!(shell.additions[1].1, "First update");
                    assert_eq!(
                        shell.additions.last(),
                        Some(&(icon, "Latest update".into()))
                    );
                });
            },
        );
    }

    #[test]
    fn failed_version_setup_removes_partial_registration_before_retrying() {
        with_shell(
            &[
                (NIM_ADD, true),
                (NIM_SETVERSION, false),
                (NIM_DELETE, true),
                (NIM_ADD, true),
                (NIM_SETVERSION, true),
            ],
            || {
                let mut tray = Tray::new(HWND::default()).unwrap();
                assert!(!tray.registered.get());
                tray.set_tooltip("Explorer ready").unwrap();
                assert!(tray.registered.get());
            },
        );
    }
}
