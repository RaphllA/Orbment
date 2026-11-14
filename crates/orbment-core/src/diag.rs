use std::{
    mem::size_of,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use windows::Win32::{
    Foundation::{ERROR_NO_MORE_FILES, ERROR_SUCCESS, GetLastError, HANDLE, SetLastError},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        },
        Threading::{
            GET_GUI_RESOURCES_FLAGS, GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess,
            GetCurrentProcessId, GetGuiResources, GetProcessHandleCount,
        },
    },
};

use crate::single_instance::{OwnedHandle, is_elevated};

/// `modules` is the ModuleManager's authoritative lifecycle snapshot.
pub fn collect(modules: Value, hook_references: usize) -> Result<Value> {
    let process = unsafe { GetCurrentProcess() };
    let pid = unsafe { GetCurrentProcessId() };
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let mut handles = 0;
    unsafe {
        GetProcessMemoryInfo(
            process,
            (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
        .context("Collect current process memory counters")?;
        GetProcessHandleCount(process, &mut handles)
            .context("Collect current process handle count")?;
    }
    Ok(json!({
        "collected_at_unix_seconds": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        "process": {
            "pid": pid,
            "elevated": is_elevated()?,
            "working_set_bytes": memory.WorkingSetSize,
            "peak_working_set_bytes": memory.PeakWorkingSetSize,
            "private_bytes": memory.PrivateUsage,
            "page_fault_count": memory.PageFaultCount,
            "handle_count": handles,
            "gdi_objects": gui_resources(process, GR_GDIOBJECTS)?,
            "user_objects": gui_resources(process, GR_USEROBJECTS)?,
            "thread_count": thread_count(pid)?,
        },
        "hook_references": hook_references,
        "modules": modules,
    }))
}

fn gui_resources(process: HANDLE, kind: GET_GUI_RESOURCES_FLAGS) -> Result<u32> {
    unsafe {
        // Zero is a valid count before any UI has been created.
        SetLastError(ERROR_SUCCESS);
        let count = GetGuiResources(process, kind);
        let error = GetLastError();
        if count == 0 && error != ERROR_SUCCESS {
            return Err(windows::core::Error::from_hresult(error.to_hresult()))
                .context("Collect GUI resource count");
        }
        Ok(count)
    }
}

fn thread_count(pid: u32) -> Result<u32> {
    unsafe {
        let snapshot = OwnedHandle(
            CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
                .context("Snapshot process thread counts")?,
        );
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        Process32FirstW(snapshot.0, &mut entry).context("Read first process snapshot entry")?;
        loop {
            if entry.th32ProcessID == pid {
                return Ok(entry.cntThreads);
            }
            match Process32NextW(snapshot.0, &mut entry) {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
                Err(error) => return Err(error).context("Read process snapshot entry"),
            }
        }
    }
    bail!("Current process {pid} is absent from the process snapshot")
}
