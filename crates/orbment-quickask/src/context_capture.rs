use anyhow::{Context, Result, ensure};
use orbment_core::i18n::tr;
use windows::Win32::{
    Foundation::HWND,
    System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    },
    UI::{
        Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId,
        },
        WindowsAndMessaging::{GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW},
    },
};

#[derive(Clone, Debug)]
pub struct ContextImage {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct QuickContext {
    pub text: String,
    pub source_title: String,
    pub image: Option<ContextImage>,
}
impl QuickContext {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.text.len() <= 4 * 1024 * 1024,
            "{}",
            tr("quick_ask.context_too_large")
        );
        if let Some(image) = &self.image {
            ensure!(
                ["image/png", "image/jpeg", "image/webp", "image/gif"]
                    .contains(&image.mime_type.as_str())
                    && !image.bytes.is_empty()
                    && image.bytes.len() <= 20 * 1024 * 1024,
                "{}",
                tr("quick_ask.invalid_image")
            );
        }
        Ok(())
    }
}

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// Call on a dedicated worker before showing the popup. UI Automation reads the
/// selection directly: neither the clipboard nor either clipboard history changes.
/// Applications without TextPattern report an error instead of injecting Ctrl+C.
pub fn capture_selected_text(source_window: isize) -> Result<QuickContext> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let _apartment = ComApartment;
    let hwnd = HWND(source_window as *mut _);
    ensure!(
        !hwnd.is_invalid() && unsafe { GetForegroundWindow() } == hwnd,
        "{}",
        tr("quick_ask.source_changed")
    );
    let title_length = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let mut title = vec![0u16; title_length + 1];
    let copied = unsafe { GetWindowTextW(hwnd, &mut title) }.max(0) as usize;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
    let focused = unsafe { automation.GetFocusedElement()? };
    ensure!(
        !unsafe { focused.CurrentIsPassword()? }.as_bool(),
        "{}",
        tr("quick_ask.password_selection")
    );
    let pattern: IUIAutomationTextPattern =
        unsafe { focused.GetCurrentPatternAs(UIA_TextPatternId) }
            .context(tr("quick_ask.selection_unavailable"))?;
    let ranges = unsafe { pattern.GetSelection()? };
    let mut selected = String::new();
    for index in 0..unsafe { ranges.Length()? } {
        let range = unsafe { ranges.GetElement(index)? };
        let remaining = 4 * 1024 * 1024 - selected.len();
        let text = unsafe { range.GetText((remaining + 1) as i32)? }.to_string();
        ensure!(
            text.len() + usize::from(!selected.is_empty() && !text.is_empty()) <= remaining,
            "{}",
            tr("quick_ask.context_too_large")
        );
        if !text.is_empty() {
            if !selected.is_empty() {
                selected.push('\n');
            }
            selected.push_str(&text);
        }
    }
    ensure!(
        unsafe { GetForegroundWindow() } == hwnd,
        "{}",
        tr("quick_ask.source_changed")
    );
    ensure!(
        !selected.trim().is_empty(),
        "{}",
        tr("quick_ask.no_selection")
    );
    let context = QuickContext {
        text: selected,
        source_title: String::from_utf16_lossy(&title[..copied]),
        image: None,
    };
    context.validate()?;
    Ok(context)
}
