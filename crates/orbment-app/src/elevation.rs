use std::{ffi::OsString, mem::size_of, os::windows::ffi::OsStrExt, path::Path};

use anyhow::{Context, Result, bail, ensure};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_INVALID_PARAMETER, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        UI::{
            Shell::{
                SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
            },
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
    core::{PCWSTR, w},
};

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Request elevation without exiting the caller; only a successful launch permits shutdown.
pub fn restart(data_dir: &Path) -> Result<()> {
    let executable = std::env::current_exe().context("Locate Orbment for elevated restart")?;
    let executable: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let directory = std::env::current_dir().context("Read elevated restart working directory")?;
    let directory: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
    let parameters = command_line(&[
        OsString::from("--restart-from"),
        OsString::from(std::process::id().to_string()),
        OsString::from("--data-dir"),
        data_dir.as_os_str().to_owned(),
    ])?;
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
    let launched = unsafe { ShellExecuteExW(&mut execute) };
    let process = ProcessHandle(execute.hProcess);
    launched.context("Launch elevated Orbment restart")?;
    ensure!(
        !process.0.is_invalid(),
        "Elevated Orbment restart returned no process handle"
    );
    Ok(())
}

/// Wait before acquiring the singleton so the previous instance can finish shutdown.
pub fn wait_for_parent(pid: u32) -> Result<()> {
    ensure!(
        pid != 0 && pid != std::process::id(),
        "Invalid elevated restart parent PID: {pid}"
    );
    let process = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(handle) => ProcessHandle(handle),
        // The old instance can finish exiting before the elevated child opens its handle.
        Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("Open elevated restart parent PID {pid}"));
        }
    };
    match unsafe { WaitForSingleObject(process.0, 30_000) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => bail!("Elevated restart parent PID {pid} did not exit within 30 seconds"),
        WAIT_FAILED => Err(windows::core::Error::from_win32())
            .with_context(|| format!("Wait for elevated restart parent PID {pid}")),
        status => bail!("Unexpected wait status {status:?} for elevated restart parent PID {pid}"),
    }
}

// CRT quoting preserves UTF-16, embedded quotes and backslashes before the closing quote.
fn command_line(arguments: &[OsString]) -> Result<Vec<u16>> {
    let mut output = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index != 0 {
            output.push(b' ' as u16);
        }
        output.push(b'"' as u16);
        let mut slashes = 0;
        for unit in argument.encode_wide() {
            ensure!(
                unit != 0,
                "Elevated restart argument contains a NUL character"
            );
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
    output.push(0);
    Ok(output)
}
