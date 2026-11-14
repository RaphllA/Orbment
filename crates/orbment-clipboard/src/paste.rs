use crate::{
    WriteContent,
    native::{
        CF_DIB, CF_HDROP, CF_UNICODETEXT, Clipboard, GetForegroundWindow, GlobalLock, GlobalUnlock,
        Hwnd, INTERNAL, decode_image, format, limit, os_error,
    },
};
use anyhow::{Result, ensure};
use image::ImageFormat;
use orbment_core::{config::ClipboardConfig, i18n::tr};
use std::{io::Cursor, mem::size_of};

#[link(name = "user32")]
unsafe extern "system" {
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, memory: isize) -> isize;
    fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    fn IsWindow(hwnd: Hwnd) -> i32;
    fn GetAsyncKeyState(key: i32) -> i16;
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, size: usize) -> isize;
    fn GlobalFree(memory: isize) -> isize;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyboardInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    data: u32,
    flags: u32,
    time: u32,
    extra: usize,
}
#[repr(C)]
union InputData {
    keyboard: KeyboardInput,
    mouse: MouseInput,
}
#[repr(C)]
struct Input {
    kind: u32,
    data: InputData,
}
fn put(format: u32, data: &[u8]) -> Result<()> {
    let memory = unsafe { GlobalAlloc(0x0002, data.len().max(1)) };
    ensure!(
        memory != 0,
        "{}: {}",
        tr("clipboard.error.native"),
        os_error()
    );
    let pointer = unsafe { GlobalLock(memory) };
    if pointer.is_null() {
        unsafe {
            GlobalFree(memory);
        }
        return Err(os_error());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), pointer.cast::<u8>(), data.len());
        GlobalUnlock(memory);
    }
    if unsafe { SetClipboardData(format, memory) } == 0 {
        unsafe {
            GlobalFree(memory);
        }
        return Err(os_error());
    }
    Ok(())
}

fn unicode_bytes(text: &str) -> Result<Vec<u8>> {
    ensure!(
        !text.contains('\0'),
        "{}",
        tr("clipboard.error.invalid_content")
    );
    Ok(text
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect())
}

pub(crate) fn write(hwnd: Hwnd, content: &WriteContent, config: &ClipboardConfig) -> Result<()> {
    // Encode and validate before EmptyClipboard so failed input never destroys the user's clipboard.
    let formats: Vec<(u32, Vec<u8>)> = match content {
        WriteContent::Text(text) => vec![(CF_UNICODETEXT, unicode_bytes(text)?)],
        WriteContent::Html { plain, raw } => vec![
            (CF_UNICODETEXT, unicode_bytes(plain)?),
            (format("HTML Format")?, raw.clone()),
        ],
        WriteContent::Rtf { plain, raw } => vec![
            (CF_UNICODETEXT, unicode_bytes(plain)?),
            (format("Rich Text Format")?, raw.clone()),
        ],
        WriteContent::Files(paths) => {
            ensure!(
                !paths.is_empty(),
                "{}",
                tr("clipboard.error.invalid_content")
            );
            let mut drop = vec![0u8; 20];
            drop[..4].copy_from_slice(&20u32.to_le_bytes());
            drop[16..20].copy_from_slice(&1u32.to_le_bytes());
            for path in paths {
                ensure!(
                    path.is_absolute(),
                    "{}",
                    tr("clipboard.error.invalid_content")
                );
                drop.extend(unicode_bytes(&path.to_string_lossy())?);
            }
            drop.extend_from_slice(&[0, 0]);
            vec![(CF_HDROP, drop)]
        }
        WriteContent::Image(path) => {
            let metadata = std::fs::metadata(path)?;
            ensure!(
                metadata.len() <= limit(config.capture.max_image_mb) as u64,
                "{}",
                tr("clipboard.error.too_large")
            );
            let image = decode_image(&std::fs::read(path)?, limit(config.capture.max_image_mb))?;
            let mut png = Cursor::new(Vec::new());
            image.write_to(&mut png, ImageFormat::Png)?;
            let width = image.width();
            let height = image.height();
            ensure!(
                width <= i32::MAX as u32 && height <= i32::MAX as u32,
                "{}",
                tr("clipboard.error.invalid_image")
            );
            let rgba = image.into_rgba8();
            let mut dib = vec![0u8; 40];
            dib[..4].copy_from_slice(&40u32.to_le_bytes());
            dib[4..8].copy_from_slice(&width.to_le_bytes());
            dib[8..12].copy_from_slice(&height.to_le_bytes());
            dib[12..14].copy_from_slice(&1u16.to_le_bytes());
            dib[14..16].copy_from_slice(&32u16.to_le_bytes());
            for y in (0..height).rev() {
                for x in 0..width {
                    let [r, g, b, a] = rgba.get_pixel(x, y).0;
                    dib.extend_from_slice(&[b, g, r, a]);
                }
            }
            vec![(format("PNG")?, png.into_inner()), (CF_DIB, dib)]
        }
    };
    if !matches!(content, WriteContent::Image(_)) {
        ensure!(
            formats
                .iter()
                .all(|(_, data)| data.len() <= limit(config.capture.max_text_mb)),
            "{}",
            tr("clipboard.error.too_large")
        );
    }
    let internal = format(INTERNAL)?;
    let _clipboard = Clipboard::open(hwnd)?;
    ensure!(
        unsafe { EmptyClipboard() } != 0,
        "{}: {}",
        tr("clipboard.error.native"),
        os_error()
    );
    put(internal, &1u32.to_le_bytes())?;
    for (id, data) in formats {
        put(id, &data)?;
    }
    Ok(())
}

pub(crate) fn paste(target: Option<isize>) -> Result<()> {
    if let Some(target) = target {
        ensure!(
            unsafe { IsWindow(target) } != 0,
            "{}",
            tr("clipboard.error.focus")
        );
        if unsafe { GetForegroundWindow() } != target {
            ensure!(
                unsafe { SetForegroundWindow(target) } != 0,
                "{}",
                tr("clipboard.error.focus")
            );
        }
        ensure!(
            unsafe { GetForegroundWindow() } == target,
            "{}",
            tr("clipboard.error.focus")
        );
    }
    let held_ctrl = unsafe { GetAsyncKeyState(0x11) } < 0;
    let modifiers: Vec<u16> = [0xa0, 0xa1, 0xa4, 0xa5, 0x5b, 0x5c]
        .into_iter()
        .filter(|key| unsafe { GetAsyncKeyState(*key as i32) } < 0)
        .collect();
    let sequence = paste_key_sequence(held_ctrl, &modifiers);
    let inputs: Vec<Input> = sequence
        .iter()
        .map(|(vk, up)| Input {
            kind: 1,
            data: InputData {
                keyboard: KeyboardInput {
                    vk: *vk,
                    scan: 0,
                    flags: if *up { 2 } else { 0 },
                    time: 0,
                    extra: 0,
                },
            },
        })
        .collect();
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<Input>() as i32,
        )
    };
    if sent != inputs.len() as u32 {
        // A partial batch must not strand synthetic Ctrl or V in the down state.
        let mut recovery = vec![Input {
            kind: 1,
            data: InputData {
                keyboard: KeyboardInput {
                    vk: 0x56,
                    scan: 0,
                    flags: 2,
                    time: 0,
                    extra: 0,
                },
            },
        }];
        if !held_ctrl {
            recovery.push(Input {
                kind: 1,
                data: InputData {
                    keyboard: KeyboardInput {
                        vk: 0x11,
                        scan: 0,
                        flags: 2,
                        time: 0,
                        extra: 0,
                    },
                },
            });
        }
        unsafe {
            SendInput(
                recovery.len() as u32,
                recovery.as_ptr(),
                size_of::<Input>() as i32,
            );
        }
        anyhow::bail!(tr("clipboard.error.input_blocked"));
    }
    Ok(())
}
/// VK values are kept platform-independent so the security-critical injection ordering is testable.
pub fn paste_key_sequence(held_ctrl: bool, held_modifiers: &[u16]) -> Vec<(u16, bool)> {
    let mut sequence = vec![(0x11, false)];
    sequence.extend(
        held_modifiers
            .iter()
            .copied()
            .filter(|key| {
                matches!(key, 0x10 | 0x12 | 0xa0..=0xa5 | 0x5b | 0x5c)
                    && !matches!(key, 0xa2 | 0xa3)
            })
            .map(|key| (key, true)),
    );
    sequence.extend([(0x56, false), (0x56, true)]);
    if !held_ctrl {
        sequence.push((0x11, true));
    }
    sequence
}
#[cfg(test)]
mod tests {
    use super::paste_key_sequence;
    #[test]
    fn paste_masks_menu_modifiers_and_preserves_held_control() {
        assert_eq!(
            paste_key_sequence(false, &[0xa4, 0xa0, 0x5b]),
            vec![
                (0x11, false),
                (0xa4, true),
                (0xa0, true),
                (0x5b, true),
                (0x56, false),
                (0x56, true),
                (0x11, true)
            ]
        );
        assert_eq!(
            paste_key_sequence(true, &[]),
            vec![(0x11, false), (0x56, false), (0x56, true)]
        );
    }
}
