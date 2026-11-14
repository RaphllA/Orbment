use anyhow::Result;
use orbment_core::i18n::tr;
use windows::{
    Win32::{Foundation::HWND, UI::Controls::*},
    core::PCWSTR,
};

/// Invoke on the UI thread only when the router returns `ChooseEngine`.
/// Persistence must succeed before the caller retries the pending OCR operation.
pub fn choose_ocr_engine(
    owner: HWND,
    persist: impl FnOnce(&'static str) -> Result<()>,
) -> Result<Option<&'static str>> {
    let title = crate::wide(&tr("ocr.engine_prompt_title"));
    let instruction = crate::wide(&tr("ocr.engine_prompt_instruction"));
    let content = crate::wide(&tr("ocr.engine_prompt_content"));
    let system = crate::wide(&tr("ocr.engine_windows_choice"));
    let offline = crate::wide(&tr("ocr.engine_offline_choice"));
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: 101,
            pszButtonText: PCWSTR(system.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: 102,
            pszButtonText: PCWSTR(offline.as_ptr()),
        },
    ];
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner,
        dwFlags: TDF_USE_COMMAND_LINKS | TDF_ALLOW_DIALOG_CANCELLATION | TDF_SIZE_TO_CONTENT,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszMainInstruction: PCWSTR(instruction.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: 101,
        ..Default::default()
    };
    let mut selected = 0;
    unsafe {
        TaskDialogIndirect(&config, Some(&mut selected), None, None)?;
    }
    let tier = match selected {
        101 => "WindowsNative",
        102 => "OfflineOnnx",
        _ => return Ok(None),
    };
    persist(tier)?;
    Ok(Some(tier))
}
