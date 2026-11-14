use crate::capture_overlay::wide;
use anyhow::Result;
use windows::{
    Win32::{Foundation::*, System::Registry::*},
    core::PCWSTR,
};

const TYPES: &[&str] = &["image", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"];
pub fn cleanup() -> Result<()> {
    for kind in TYPES {
        let path = wide(&format!(
            "Software\\Classes\\SystemFileAssociations\\{kind}\\shell\\Orbment.Pin"
        ));
        let result = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
        if result != ERROR_FILE_NOT_FOUND && result != ERROR_PATH_NOT_FOUND {
            result.ok()?;
        }
    }
    Ok(())
}
