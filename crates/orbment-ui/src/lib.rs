mod analyzer_panel;
mod audio_osd;
mod d2d_context;
mod event_loop;
mod file_tools;
mod hook_thread;
mod hotkeys;
pub mod ime;
mod main_host;
mod mesh_renderer;
mod ocr_prompt;
mod settings;
mod tray;
pub mod theme;
pub mod treemap;
mod ui_popup;
pub mod utilities;
mod utility_overlay;
mod windows;
pub use ocr_prompt::choose_ocr_engine;

pub use audio_osd::AudioOsd;
pub use d2d_context::{D2dContext, TextRun};
pub use event_loop::{EventLoop, PIN_IMAGE_COPYDATA_TAG, UiEvent, send_pin_image, wake};
pub use file_tools::{FileToolsAction, FileToolsState, SharedDiskReport, SharedFileToolsState};
pub use hook_thread::{
    HookSubscription, HookThread, KeyboardHookEvent, MouseButton, MouseEventKind, MouseHookEvent,
    MouseHookHandler, MouseModifiers,
};
pub use hotkeys::Hotkeys;
pub use main_host::{
    CanvasPainter, MainHostWindow, PopupPanel, SearchPopupWindow, SettingsEvent, SharedFonts,
    TransientPanelWindow, TransientPopupWindow, render_ui_previews,
};
pub use tray::{MenuItem, Tray};
pub use ui_popup::{ClipboardGroup, ClipboardPopupEvent, ClipboardRow, PopupQuery};
pub use utilities::UtilitiesModule;
pub use windows::{OverlayFloatingWindow, TaskbarChildWindow};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn register_class(
    name: ::windows::core::PCWSTR,
    procedure: ::windows::Win32::UI::WindowsAndMessaging::WNDPROC,
) -> anyhow::Result<()> {
    use ::windows::Win32::{
        Foundation::ERROR_CLASS_ALREADY_EXISTS, System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    };
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: procedure,
            hInstance: GetModuleHandleW(None)?.into(),
            lpszClassName: name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            let error = ::windows::core::Error::from_win32();
            if error.code() != ::windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
                return Err(error.into());
            }
        }
    }
    Ok(())
}
