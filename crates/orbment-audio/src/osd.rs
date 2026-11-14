use std::rc::Rc;

use anyhow::Result;
use orbment_core::i18n::tr;
use orbment_ui::{D2dContext, OverlayFloatingWindow};

pub fn show(
    context: Rc<D2dContext>,
    device_name: &str,
    duration_ms: u32,
) -> Result<OverlayFloatingWindow> {
    let text = format!("{} {}", tr("audio.switched"), device_name);
    OverlayFloatingWindow::new(context, &text, duration_ms)
}

pub fn play_notification_sound() -> Result<()> {
    use windows::Win32::{System::Diagnostics::Debug::MessageBeep, UI::WindowsAndMessaging::MB_OK};
    unsafe { MessageBeep(MB_OK)? };
    Ok(())
}
