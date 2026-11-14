use std::{
    cell::RefCell,
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, bail};
use orbment_core::i18n::tr;
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE, HWND, LPARAM, WPARAM},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, CoCreateInstance, CoSetProxyBlanket, EOAC_NONE,
                RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
            },
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Threading::{
                GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
            Variant::{VARIANT, VariantClear, VariantToUInt32},
            Wmi::{
                IWbemClassObject, IWbemLocator, IWbemObjectSink, IWbemObjectSink_Impl,
                IWbemServices, WBEM_E_CALL_CANCELLED, WBEM_FLAG_SEND_STATUS, WbemLocator,
            },
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::{
                DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GetForegroundWindow, GetMessageW,
                GetWindowThreadProcessId, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
                WINEVENT_OUTOFCONTEXT, WM_QUIT,
            },
        },
    },
    core::{BSTR, HRESULT, PCWSTR, PWSTR, Ref, implement, w},
};

use crate::Command;

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(crate) fn foreground_pid() -> Result<u32> {
    let hwnd = unsafe { GetForegroundWindow() };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        bail!(tr("audio.no_foreground_process"));
    }
    Ok(pid)
}

pub(crate) fn process_path(pid: u32) -> Result<String> {
    let process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? });
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )?
    };
    Ok(String::from_utf16_lossy(&buffer[..length as usize]))
}

pub(crate) fn running_processes() -> Result<Vec<(u32, String)>> {
    let snapshot = OwnedHandle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? });
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    let mut result = Vec::new();
    loop {
        match next {
            Ok(()) => {
                // Protected/system processes are expected to deny image-path access.
                if let Ok(path) = process_path(entry.th32ProcessID) {
                    result.push((entry.th32ProcessID, path));
                }
                next = unsafe { Process32NextW(snapshot.0, &mut entry) };
            }
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(result)
}

pub(crate) fn same_path(left: &str, right: &str) -> bool {
    left.replace('/', "\\").to_lowercase() == right.replace('/', "\\").to_lowercase()
}

pub(crate) fn same_name(path: &str, name: &str) -> bool {
    path.rsplit(['\\', '/'])
        .next()
        .is_some_and(|base| base.to_lowercase() == name.to_lowercase())
}

thread_local! {
    static FOREGROUND_SENDER: RefCell<Option<Sender<Command>>> = const { RefCell::new(None) };
}

unsafe extern "system" fn foreground_event(
    _: HWINEVENTHOOK,
    _: u32,
    hwnd: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != 0 {
        FOREGROUND_SENDER.with(|sender| {
            if let Some(sender) = sender.borrow().as_ref() {
                let _ = sender.send(Command::ForegroundChanged(pid));
            }
        });
    }
}

pub(crate) struct ForegroundWatcher {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl ForegroundWatcher {
    pub(crate) fn new(sender: Sender<Command>) -> Result<Self> {
        let (ready, received) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("orbment-audio-foreground".into())
            .spawn(move || {
                FOREGROUND_SENDER.with(|slot| *slot.borrow_mut() = Some(sender));
                let mut message = MSG::default();
                let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
                let hook = unsafe {
                    SetWinEventHook(
                        EVENT_SYSTEM_FOREGROUND,
                        EVENT_SYSTEM_FOREGROUND,
                        None,
                        Some(foreground_event),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT,
                    )
                };
                if hook.0.is_null() {
                    let _ = ready.send(Err(windows::core::Error::from_win32().to_string()));
                    return;
                }
                if ready.send(Ok(unsafe { GetCurrentThreadId() })).is_ok() {
                    loop {
                        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
                        if result.0 <= 0 {
                            break;
                        }
                        unsafe { DispatchMessageW(&message) };
                    }
                }
                let _ = unsafe { UnhookWinEvent(hook) };
                FOREGROUND_SENDER.with(|slot| *slot.borrow_mut() = None);
            })
            .with_context(|| tr("audio.foreground_watch_failed"))?;
        match received.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                bail!("{}: {result:?}", tr("audio.foreground_watch_failed"))
            }
        }
    }
}

impl Drop for ForegroundWatcher {
    fn drop(&mut self) {
        if let Err(error) =
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
        {
            tracing::warn!(%error, "Audio foreground watcher already stopped");
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[implement(IWbemObjectSink)]
struct ProcessSink {
    sender: Sender<Command>,
}

#[allow(non_snake_case)]
impl IWbemObjectSink_Impl for ProcessSink_Impl {
    fn Indicate(
        &self,
        count: i32,
        objects: *const Option<IWbemClassObject>,
    ) -> windows::core::Result<()> {
        // WMI owns this array for the callback duration; retain only primitive process IDs.
        if count <= 0 || objects.is_null() {
            return Ok(());
        }
        for object in unsafe { std::slice::from_raw_parts(objects, count as usize) }
            .iter()
            .flatten()
        {
            let mut value = VARIANT::default();
            let read = unsafe { object.Get(w!("ProcessID"), 0, &mut value, None, None) };
            let pid = read.and_then(|()| unsafe { VariantToUInt32(&value) });
            let _ = unsafe { VariantClear(&mut value) };
            if let Ok(pid) = pid {
                let _ = self.sender.send(Command::ProcessStarted(pid));
            }
        }
        Ok(())
    }

    fn SetStatus(
        &self,
        _: i32,
        result: HRESULT,
        _: &BSTR,
        _: Ref<'_, IWbemClassObject>,
    ) -> windows::core::Result<()> {
        if result.is_err() && result != HRESULT(WBEM_E_CALL_CANCELLED.0) {
            let _ = self.sender.send(Command::WatcherFailed(format!(
                "{}: {result}",
                tr("audio.process_watch_failed")
            )));
        }
        Ok(())
    }
}

pub(crate) struct ProcessWatcher {
    services: IWbemServices,
    sink: IWbemObjectSink,
}

impl ProcessWatcher {
    pub(crate) fn new(sender: Sender<Command>) -> Result<Self> {
        unsafe {
            let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
            let empty = BSTR::new();
            let services = locator.ConnectServer(
                &BSTR::from(r"ROOT\CIMV2"),
                &empty,
                &empty,
                &empty,
                0,
                &empty,
                None,
            )?;
            CoSetProxyBlanket(
                &services,
                10,
                0,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )?;
            let sink: IWbemObjectSink = ProcessSink { sender }.into();
            // Win32_ProcessStartTrace is the event provider, not a __InstanceCreationEvent poll.
            services.ExecNotificationQueryAsync(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT * FROM Win32_ProcessStartTrace"),
                WBEM_FLAG_SEND_STATUS,
                None,
                &sink,
            )?;
            Ok(Self { services, sink })
        }
    }
}

impl Drop for ProcessWatcher {
    fn drop(&mut self) {
        if let Err(error) = unsafe { self.services.CancelAsyncCall(&self.sink) } {
            tracing::warn!(%error, "CancelAsyncCall(audio process watcher) failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_paths_are_exact_case_insensitive_windows_paths() {
        assert!(same_path(r"C:\Apps\PLAYER.EXE", "c:/apps/player.exe"));
        assert!(!same_path(r"C:\Apps\player.exe", r"D:\Apps\player.exe"));
        assert!(same_name(r"C:\Apps\player.exe", "PLAYER.EXE"));
        assert!(!same_name(r"C:\Apps\player.exe", "layer.exe"));
    }
}
