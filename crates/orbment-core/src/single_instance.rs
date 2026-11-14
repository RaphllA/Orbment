use std::{
    ffi::{OsStr, OsString},
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND,
            GetLastError, HANDLE, LPARAM, WPARAM,
        },
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{
            CreateMutexW, GetCurrentProcess, OpenMutexW, OpenProcessToken,
            SYNCHRONIZATION_SYNCHRONIZE,
        },
        UI::{
            Shell::{
                SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
            },
            WindowsAndMessaging::{
                FindWindowW, PostMessageW, RegisterWindowMessageW, SW_SHOWNORMAL,
            },
        },
    },
    core::{PCWSTR, w},
};

pub struct InstanceGuard {
    _mutex: OwnedHandle,
}

pub(super) struct OwnedHandle(pub HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Acquire the daemon singleton using the caller's current token.
pub fn start() -> Result<Option<InstanceGuard>> {
    unsafe {
        match OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            w!("Local\\Orbment_SingleInstance"),
        ) {
            Ok(handle) => {
                let _existing = OwnedHandle(handle);
                activate_existing();
                return Ok(None);
            }
            Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                activate_existing();
                return Ok(None);
            }
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {}
            Err(error) => return Err(error).context("Open Orbment instance mutex"),
        }
    }

    unsafe {
        match CreateMutexW(None, false, w!("Local\\Orbment_SingleInstance")) {
            Ok(handle) => {
                let already_exists = GetLastError() == ERROR_ALREADY_EXISTS;
                let mutex = OwnedHandle(handle);
                if already_exists {
                    activate_existing();
                    return Ok(None);
                }
                Ok(Some(InstanceGuard { _mutex: mutex }))
            }
            // Another integrity level can win the race between the probe and creation.
            Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                activate_existing();
                Ok(None)
            }
            Err(error) => Err(error).context("Create Orbment instance mutex"),
        }
    }
}

fn activate_existing() {
    unsafe {
        let message = RegisterWindowMessageW(w!("Orbment.Activate"));
        if message == 0 {
            tracing::warn!(error = ?windows::core::Error::from_win32(), "Register activation message");
            return;
        }
        // The first instance may still be starting and not yet have a host window.
        if let Ok(window) = FindWindowW(w!("Orbment.Host"), PCWSTR::null())
            && let Err(error) = PostMessageW(Some(window), message, WPARAM(0), LPARAM(0))
        {
            tracing::warn!(%error, "Post activation message");
        }
    }
}

pub fn is_elevated() -> Result<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .context("Open current process token")?;
        let token = OwnedHandle(token);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0;
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .context("Read current token elevation")?;
        Ok(elevation.TokenIsElevated != 0)
    }
}

pub(super) fn wide(value: &OsStr) -> Vec<u16> {
    let mut units: Vec<u16> = value.encode_wide().collect();
    units.push(0);
    units
}

/// Quote each argument using the Windows CRT backslash/quote rules, without a shell.
pub fn command_line(arguments: &[OsString]) -> Vec<u16> {
    let mut output = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index != 0 {
            output.push(b' ' as u16);
        }
        output.push(b'"' as u16);
        let mut slashes = 0;
        for unit in argument.encode_wide() {
            if unit == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            let count = if unit == b'"' as u16 {
                slashes * 2 + 1
            } else {
                slashes
            };
            output.extend(std::iter::repeat_n(b'\\' as u16, count));
            output.push(unit);
            slashes = 0;
        }
        output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        output.push(b'"' as u16);
    }
    output
}

pub(super) fn run_elevated(executable: &Path, arguments: &[OsString]) -> Result<OwnedHandle> {
    let executable = wide(executable.as_os_str());
    let mut parameters = command_line(arguments);
    parameters.push(0);
    let directory = wide(std::env::current_dir()?.as_os_str());
    let mut execute = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(executable.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: PCWSTR(directory.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut execute)?;
    }
    ensure!(
        !execute.hProcess.is_invalid(),
        "Elevated launch returned no process handle"
    );
    Ok(OwnedHandle(execute.hProcess))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_preserves_empty_unicode_quotes_and_trailing_backslashes() {
        let arguments = [
            OsString::new(),
            OsString::from(r"C:\用户 data\"),
            OsString::from("a\\\"b"),
        ];
        assert_eq!(
            String::from_utf16(&command_line(&arguments)).unwrap(),
            "\"\" \"C:\\用户 data\\\\\" \"a\\\\\\\"b\""
        );
    }
}
