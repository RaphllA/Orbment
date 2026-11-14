use crate::native::{Handle, NtQueryInformationProcess, ProcessBasicInformation, UnicodeString};
use anyhow::{Context, Result, ensure};
use orbment_core::i18n::tr;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, mem::size_of};
use windows::{
    Win32::{
        Foundation::{ERROR_NO_MORE_FILES, FILETIME, GetLastError, HANDLE, WAIT_TIMEOUT},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            ProcessStatus::{
                GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
            },
            Threading::{
                GetProcessTimes, IsProcessCritical, OpenProcess, PROCESS_ACCESS_RIGHTS,
                PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
                WaitForSingleObject,
            },
        },
    },
    core::{BOOL, PWSTR},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Windows FILETIME ticks, not Unix time. Required for every destructive operation.
    #[serde(with = "filetime")]
    pub creation_time: u64,
}

/// Decimal-string wire representation preserves FILETIME precision in JavaScript clients.
pub mod filetime {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(D::Error::custom(orbment_core::i18n::tr(
                "memory-process-times",
            )));
        }
        value
            .parse()
            .map_err(|_| D::Error::custom(orbment_core::i18n::tr("memory-process-times")))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub identity: Option<ProcessIdentity>,
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
    pub path: Option<String>,
    pub command_line: Option<String>,
    pub working_set: u64,
    pub private_bytes: u64,
    pub accessible: bool,
}

pub(crate) fn creation_time(handle: HANDLE) -> Result<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
        .context(tr("memory-process-times"))?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

pub(crate) fn open_verified(
    identity: ProcessIdentity,
    rights: PROCESS_ACCESS_RIGHTS,
) -> Result<Handle> {
    let handle = Handle(
        unsafe {
            OpenProcess(
                rights | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                identity.pid,
            )
        }
        .with_context(|| format!("{}: {}", tr("memory-process-inaccessible"), identity.pid))?,
    );
    ensure!(
        creation_time(handle.0)? == identity.creation_time
            && unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_TIMEOUT,
        "{}: {}",
        tr("memory-process-changed"),
        identity.pid
    );
    Ok(handle)
}

pub(crate) fn open_destructive(identity: ProcessIdentity, force: bool) -> Result<Handle> {
    ensure!(
        identity.pid > 4 && identity.pid != std::process::id(),
        "{}",
        tr("memory-process-protected")
    );
    let handle = open_verified(
        identity,
        if force {
            PROCESS_TERMINATE
        } else {
            PROCESS_ACCESS_RIGHTS(0)
        },
    )?;
    let mut critical = BOOL::default();
    unsafe { IsProcessCritical(handle.0, &mut critical) }
        .context(tr("memory-process-inaccessible"))?;
    ensure!(!critical.as_bool(), "{}", tr("memory-process-protected"));
    Ok(handle)
}

pub fn kill(identity: ProcessIdentity) -> Result<()> {
    let handle = open_destructive(identity, true)?;
    unsafe { TerminateProcess(handle.0, 1) }.context(tr("memory-process-kill"))
}

pub fn snapshot() -> Result<Vec<ProcessInfo>> {
    snapshot_with_details(true)
}

pub(crate) fn snapshot_identities() -> Result<Vec<ProcessInfo>> {
    snapshot_with_details(false)
}

fn snapshot_with_details(details: bool) -> Result<Vec<ProcessInfo>> {
    let snapshot = Handle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .context(tr("memory-process-enumerate"))?,
    );
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe { Process32FirstW(snapshot.0, &mut entry) }.context(tr("memory-process-enumerate"))?;
    let mut processes = Vec::new();
    loop {
        let end = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let mut process = ProcessInfo {
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..end]),
            ..Default::default()
        };
        if let Ok(raw) =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process.pid) }
        {
            let handle = Handle(raw);
            process.identity = creation_time(raw)
                .ok()
                .map(|creation_time| ProcessIdentity {
                    pid: process.pid,
                    creation_time,
                });
            if details {
                // Read ancestry from the opened generation, not a possibly stale ToolHelp row.
                let mut basic = ProcessBasicInformation::default();
                let queried = unsafe {
                    NtQueryInformationProcess(
                        raw,
                        0,
                        (&mut basic as *mut ProcessBasicInformation).cast(),
                        size_of::<ProcessBasicInformation>() as u32,
                        std::ptr::null_mut(),
                    )
                };
                process.parent_pid = if queried >= 0 {
                    basic.parent_pid as u32
                } else {
                    0
                };
                read_image_metadata(handle.0, &mut process);
                if process.path.is_none() {
                    process.identity = None;
                }
                let mut counters = PROCESS_MEMORY_COUNTERS_EX {
                    cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                    ..Default::default()
                };
                if unsafe {
                    GetProcessMemoryInfo(
                        raw,
                        (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX)
                            .cast::<PROCESS_MEMORY_COUNTERS>(),
                        size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                    )
                }
                .is_ok()
                {
                    process.working_set = counters.WorkingSetSize as u64;
                    process.private_bytes = counters.PrivateUsage as u64;
                    process.accessible = process.identity.is_some();
                }
            }
        }
        processes.push(process);
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            let error = unsafe { GetLastError() };
            ensure!(
                error == ERROR_NO_MORE_FILES,
                "{}: {}",
                tr("memory-process-enumerate"),
                error.0
            );
            break;
        }
    }
    Ok(processes)
}

pub(crate) fn enrich_port_process(process: &mut ProcessInfo) -> Result<()> {
    let identity = process
        .identity
        .context(tr("memory-process-inaccessible"))?;
    let handle = open_verified(identity, PROCESS_QUERY_LIMITED_INFORMATION)?;
    read_image_metadata(handle.0, process);
    Ok(())
}

fn read_image_metadata(handle: HANDLE, process: &mut ProcessInfo) {
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    }
    .is_ok()
    {
        let path = String::from_utf16_lossy(&path[..length as usize]);
        process.name = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned();
        process.path = Some(path);
    }
    process.command_line = command_line(handle);
}

fn command_line(handle: HANDLE) -> Option<String> {
    let mut needed = 0;
    unsafe {
        NtQueryInformationProcess(handle, 60, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed < size_of::<UnicodeString>() as u32 {
        return None;
    }
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    let status = unsafe {
        NtQueryInformationProcess(handle, 60, buffer.as_mut_ptr().cast(), needed, &mut needed)
    };
    if status < 0 {
        return None;
    }
    let text = unsafe { &*buffer.as_ptr().cast::<UnicodeString>() };
    let start = buffer.as_ptr() as usize;
    let end = start + buffer.len() * size_of::<usize>();
    let address = text.buffer as usize;
    if text.length == 0 {
        return Some(String::new());
    }
    if address < start || address.checked_add(text.length as usize)? > end || text.length % 2 != 0 {
        return None;
    }
    Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text.buffer, text.length as usize / 2)
    }))
}

/// A parent PID alone is not ancestry: creation time must establish the same live generation.
pub fn valid_parent(child: &ProcessInfo, parent: &ProcessInfo) -> bool {
    child.pid != parent.pid
        && child.parent_pid == parent.pid
        && matches!((child.identity, parent.identity), (Some(c), Some(p)) if p.creation_time <= c.creation_time)
}

pub fn by_pid(processes: &[ProcessInfo]) -> BTreeMap<u32, &ProcessInfo> {
    processes.iter().map(|p| (p.pid, p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_metadata_is_loaded_on_demand_and_rejects_a_changed_generation() -> Result<()> {
        let processes = snapshot_identities()?;
        let mut process = processes
            .into_iter()
            .find(|process| process.pid == std::process::id())
            .context("Current process is missing")?;
        assert!(process.path.is_none());
        assert!(process.command_line.is_none());
        let identity = process
            .identity
            .context("Current process identity is missing")?;
        enrich_port_process(&mut process)?;
        assert_eq!(process.identity, Some(identity));
        assert!(process.path.is_some());

        process.identity = Some(ProcessIdentity {
            creation_time: identity.creation_time.wrapping_add(1),
            ..identity
        });
        assert!(enrich_port_process(&mut process).is_err());
        Ok(())
    }
}
