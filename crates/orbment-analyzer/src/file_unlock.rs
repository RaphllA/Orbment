use crate::native::{Handle, NtQuerySystemInformation, UnicodeString};
use crate::process::{self, ProcessIdentity};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::c_void,
    fs::{File, OpenOptions},
    mem::size_of,
    os::windows::{
        ffi::OsStringExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows::Win32::Foundation::HANDLE;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileLock {
    pub process_id: u32,
    #[serde(with = "process::filetime")]
    pub creation_time: u64,
    pub process_name: String,
    pub path: String,
    pub handle: Option<u64>,
}

const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_DUP_HANDLE: u32 = 0x0040;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const DUPLICATE_CLOSE_SOURCE: u32 = 1;
const DUPLICATE_SAME_ACCESS: u32 = 2;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_SHARE_ALL: u32 = 7;
const ERROR_MORE_DATA: u32 = 234;
const RM_BATCH_SIZE: usize = 64;
const QUERY_TIMEOUT: Duration = Duration::from_secs(25);
const RESOLVER_TIMEOUT: Duration = Duration::from_millis(15);
const RESOLVER_CANCEL_GRACE: Duration = Duration::from_millis(5);
static QUERY_RUNNING: AtomicBool = AtomicBool::new(false);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RmUniqueProcess {
    process_id: u32,
    start_time: FileTime,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RmProcessInfo {
    process: RmUniqueProcess,
    app_name: [u16; 256],
    service_name: [u16; 64],
    app_type: u32,
    app_status: u32,
    session_id: u32,
    restartable: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SystemHandle {
    object: usize,
    process_id: usize,
    value: usize,
    granted_access: u32,
    creator_backtrace_index: u16,
    object_type_index: u16,
    attributes: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Default)]
struct FileInformation {
    attributes: u32,
    creation_time: FileTime,
    last_access_time: FileTime,
    last_write_time: FileTime,
    volume_serial: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryObject(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

#[link(name = "rstrtmgr")]
unsafe extern "system" {
    fn RmStartSession(session: *mut u32, flags: u32, key: *mut u16) -> u32;
    fn RmEndSession(session: u32) -> u32;
    fn RmRegisterResources(
        session: u32,
        file_count: u32,
        files: *const *const u16,
        process_count: u32,
        processes: *const RmUniqueProcess,
        service_count: u32,
        services: *const *const u16,
    ) -> u32;
    fn RmGetList(
        session: u32,
        needed: *mut u32,
        count: *mut u32,
        processes: *mut RmProcessInfo,
        reboot_reasons: *mut u32,
    ) -> u32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> HANDLE;
    fn GetCurrentProcess() -> HANDLE;
    fn GetCurrentProcessId() -> u32;
    fn DuplicateHandle(
        source_process: HANDLE,
        source: HANDLE,
        target_process: HANDLE,
        target: *mut HANDLE,
        access: u32,
        inherit: i32,
        options: u32,
    ) -> i32;
    fn TerminateProcess(process: HANDLE, exit_code: u32) -> i32;
    fn IsProcessCritical(process: HANDLE, critical: *mut i32) -> i32;
    fn QueryFullProcessImageNameW(
        process: HANDLE,
        flags: u32,
        name: *mut u16,
        length: *mut u32,
    ) -> i32;
    fn GetFileType(file: HANDLE) -> u32;
    fn GetFileInformationByHandle(file: HANDLE, information: *mut FileInformation) -> i32;
    fn GetFinalPathNameByHandleW(file: HANDLE, path: *mut u16, length: u32, flags: u32) -> u32;
    fn CompareStringOrdinal(
        left: *const u16,
        left_length: i32,
        right: *const u16,
        right_length: i32,
        ignore_case: i32,
    ) -> i32;
    fn CancelSynchronousIo(thread: HANDLE) -> i32;
}

struct RestartSession(u32);
impl Drop for RestartSession {
    fn drop(&mut self) {
        unsafe { RmEndSession(self.0) };
    }
}

struct QueryPermit;
impl Drop for QueryPermit {
    fn drop(&mut self) {
        QUERY_RUNNING.store(false, Ordering::Release);
    }
}

/// Call from a background job. Foreign file-object queries run on a reusable
/// resolver thread that is cancelled and detached if synchronous I/O stalls.
pub fn query(path: &Path) -> Result<Vec<FileLock>, String> {
    QUERY_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| {
            "A file-lock query is still running or awaiting I/O cancellation".to_owned()
        })?;
    let permit = QueryPermit;
    let path = path.to_owned();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("file-lock-query".to_owned())
        .spawn(move || {
            let _permit = permit;
            let result = query_inner(&path, &worker_cancelled);
            let _ = sender.send(result);
        })
        .map_err(|error| format!("Start file-lock query: {error}"))?;
    match receiver.recv_timeout(QUERY_TIMEOUT) {
        Ok(result) => {
            let _ = worker.join();
            result
        }
        Err(error) => {
            cancelled.store(true, Ordering::Release);
            unsafe { CancelSynchronousIo(HANDLE(worker.as_raw_handle())) };
            // Never terminate a Rust thread: its handles/session must unwind normally.
            Err(format!(
                "File-lock query did not complete: {error}; no complete result is available"
            ))
        }
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("File-lock query was cancelled".to_owned())
    } else {
        Ok(())
    }
}

struct ResolverTarget {
    path: Vec<u16>,
    id: Option<(u32, u32, u32)>,
    directory: bool,
}

impl ResolverTarget {
    fn resolve(&self, handle: HANDLE) -> Option<Vec<u16>> {
        // File objects also include pipes; never request their object names.
        if unsafe { GetFileType(handle) } != 1 {
            return None;
        }
        let native = object_name(handle)?;
        if path_matches(&self.path, &native, self.directory) {
            return Some(native);
        }
        // Only a single-file query needs a file-ID fallback for hard-link aliases.
        if !self.directory && self.id.is_some() && file_id(handle) == self.id {
            return Some(self.path.clone());
        }
        None
    }
}

struct ResolverJob {
    handle: OwnedHandle,
    response: mpsc::SyncSender<Option<Vec<u16>>>,
}

enum Resolution {
    Path(Vec<u16>),
    Missing,
    TimedOut,
}

struct ResolverWorker {
    jobs: mpsc::Sender<ResolverJob>,
    thread: std::thread::JoinHandle<()>,
}

impl ResolverWorker {
    fn start(target: Arc<ResolverTarget>) -> Result<Self, String> {
        let (jobs, receiver) = mpsc::channel::<ResolverJob>();
        let thread = std::thread::Builder::new()
            .name("file-lock-resolver".to_owned())
            .spawn(move || {
                while let Ok(ResolverJob { handle, response }) = receiver.recv() {
                    let path = target.resolve(HANDLE(handle.as_raw_handle()));
                    // Release the duplicate before acknowledging the job. Even handle
                    // cleanup can wait for a driver; it must stay off the query thread.
                    drop(handle);
                    if response.send(path).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("Start file-lock resolver thread: {error}"))?;
        Ok(Self { jobs, thread })
    }

    fn cancel(&self) {
        unsafe { CancelSynchronousIo(HANDLE(self.thread.as_raw_handle())) };
    }
}

impl Drop for ResolverWorker {
    fn drop(&mut self) {
        self.cancel();
        // Dropping the sender disconnects the queue; dropping JoinHandle detaches.
        // A stalled worker retains its duplicate and target until it can unwind.
    }
}

struct FileResolver {
    target: Arc<ResolverTarget>,
    worker: Option<ResolverWorker>,
}

impl FileResolver {
    fn new(path: Vec<u16>, id: Option<(u32, u32, u32)>, directory: bool) -> Self {
        Self {
            target: Arc::new(ResolverTarget {
                path,
                id,
                directory,
            }),
            worker: None,
        }
    }

    fn resolve(&mut self, handle: OwnedHandle) -> Result<Resolution, String> {
        if self.worker.is_none() {
            self.worker = Some(ResolverWorker::start(Arc::clone(&self.target))?);
        }
        let worker = self.worker.as_ref().unwrap();
        // Each job owns its reply channel, so a late reply cannot reach another job.
        let (response, receiver) = mpsc::sync_channel(1);
        worker
            .jobs
            .send(ResolverJob { handle, response })
            .map_err(|_| {
                "File-lock resolver thread stopped before accepting a handle".to_owned()
            })?;
        let response = match receiver.recv_timeout(RESOLVER_TIMEOUT) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                worker.cancel();
                receiver.recv_timeout(RESOLVER_CANCEL_GRACE)
            }
            response => response,
        };
        match response {
            Ok(Some(path)) => Ok(Resolution::Path(path)),
            Ok(None) => Ok(Resolution::Missing),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Never join or terminate a blocked thread. The next handle gets a
                // fresh worker, including when it belongs to the same process.
                self.worker.take();
                Ok(Resolution::TimedOut)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.worker.take();
                Err("File-lock resolver thread stopped before returning a result".to_owned())
            }
        }
    }
}

fn object_name(handle: HANDLE) -> Option<Vec<u16>> {
    let mut length = 1024usize;
    loop {
        if length > 128 * 1024 {
            return None;
        }
        let mut buffer = vec![0usize; length.div_ceil(size_of::<usize>())];
        let mut needed = 0;
        let status = unsafe {
            NtQueryObject(
                handle,
                1, // ObjectNameInformation; avoid normalized file-information I/O.
                buffer.as_mut_ptr().cast(),
                (buffer.len() * size_of::<usize>()) as u32,
                &mut needed,
            )
        };
        if matches!(status as u32, 0xc000_0004 | 0xc000_0023 | 0x8000_0005) {
            length = (length * 2).max(needed as usize);
            continue;
        }
        if status < 0 {
            return None;
        }
        let name = unsafe { &*buffer.as_ptr().cast::<UnicodeString>() };
        let start = buffer.as_ptr() as usize;
        let end = start + buffer.len() * size_of::<usize>();
        let address = name.buffer as usize;
        if name.length == 0
            || name.length % 2 != 0
            || address < start
            || address.checked_add(name.length as usize)? > end
        {
            return None;
        }
        return Some(unsafe {
            std::slice::from_raw_parts(name.buffer, name.length as usize / 2).to_vec()
        });
    }
}

fn query_inner(path: &Path, cancelled: &AtomicBool) -> Result<Vec<FileLock>, String> {
    let query_started = Instant::now();
    let target = open_path(path, 0, FILE_SHARE_ALL)
        .map_err(|error| format!("Open lock-query target {}: {error}", path.display()))?;
    let target_handle = HANDLE(target.as_raw_handle());
    let metadata = target
        .metadata()
        .map_err(|error| format!("Read lock-query target {}: {error}", path.display()))?;
    let directory = metadata.is_dir();
    if !directory && !metadata.is_file() {
        return Err("File-lock queries require a regular file or directory".to_owned());
    }
    let target_native = final_path(target_handle, 2)?;
    let target_dos = final_path(target_handle, 0)?;
    let target_path = PathBuf::from(std::ffi::OsString::from_wide(&target_dos));
    let target_id = file_id(target_handle);
    let mut locks = Vec::new();
    check_cancelled(cancelled)?;
    let handles = system_handles()?;
    let own_pid = unsafe { GetCurrentProcessId() };
    let file_type = handles
        .iter()
        .find(|entry| {
            entry.process_id == own_pid as usize && entry.value == target_handle.0 as usize
        })
        .map(|entry| entry.object_type_index)
        .ok_or_else(|| "The system handle snapshot did not include the query target".to_owned())?;
    let mut handles: Vec<_> = handles
        .into_iter()
        .filter(|entry| {
            entry.object_type_index == file_type
                && entry.process_id != 0
                // Never recursively inspect duplicates retained by our resolver threads.
                && entry.process_id != own_pid as usize
        })
        .collect();
    drop(target);
    // Do not register our temporary target handle as an occupying process.
    // RM also sees images/mappings after the original file handle has closed.
    if !directory {
        append_restart_locks(std::slice::from_ref(&target_path), &mut locks, cancelled)?;
    }
    if directory {
        append_directory_locks(&target_path, &mut locks, cancelled)?;
    }
    let rm_pids: BTreeSet<_> = locks.iter().map(|lock| lock.process_id as usize).collect();
    handles.sort_unstable_by_key(|entry| (!rm_pids.contains(&entry.process_id), entry.process_id));
    let scan_deadline = query_started + Duration::from_secs(18);
    let mut resolver = FileResolver::new(target_native.clone(), target_id, directory);
    let mut current_pid = None;
    let mut process = None;
    let mut creation_time = 0;
    let mut timeouts = 0;
    let mut skipped = 0;
    let mut inaccessible = 0;
    let mut name = String::new();
    let mut remaining = handles.len();
    for entry in handles {
        remaining -= 1;
        if Instant::now() >= scan_deadline {
            skipped += remaining + 1;
            break;
        }
        check_cancelled(cancelled)?;
        let Ok(pid) = u32::try_from(entry.process_id) else {
            continue;
        };
        if current_pid != Some(pid) {
            current_pid = Some(pid);
            process = open_process(pid, PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION)
                .ok()
                .and_then(|handle| {
                    creation_time = process::creation_time(handle.0).ok()?;
                    Some(handle)
                });
            name = process_name(pid);
        }
        let Some(process) = process.as_ref() else {
            inaccessible += 1;
            continue;
        };
        let Ok(duplicate) = duplicate_handle(process.0, entry.value, DUPLICATE_SAME_ACCESS) else {
            continue;
        };
        let native = match resolver.resolve(duplicate)? {
            Resolution::Path(path) => path,
            Resolution::Missing => continue,
            Resolution::TimedOut => {
                timeouts += 1;
                continue;
            }
        };
        let display = if directory {
            let mut display = trim_separator(&target_dos).to_vec();
            display.extend_from_slice(&native[trim_separator(&target_native).len()..]);
            display
        } else {
            target_dos.clone()
        };
        let display = display_path(&display);
        locks.retain(|lock| {
            !(lock.process_id == pid
                && lock.creation_time == creation_time
                && lock.handle.is_none()
                && lock.path == display)
        });
        locks.push(FileLock {
            process_id: pid,
            creation_time,
            process_name: name.clone(),
            path: display,
            handle: Some(entry.value as u64),
        });
    }
    drop(resolver);
    if timeouts != 0 || skipped != 0 || inaccessible != 0 {
        tracing::warn!(
            timeouts,
            skipped_handles = skipped,
            inaccessible_handles = inaccessible,
            "File-lock scan returned known matches with partial system-handle visibility"
        );
    }
    locks.sort_by(|left, right| {
        (left.process_id, left.creation_time, &left.path, left.handle).cmp(&(
            right.process_id,
            right.creation_time,
            &right.path,
            right.handle,
        ))
    });
    locks.dedup_by(|left, right| {
        left.process_id == right.process_id
            && left.creation_time == right.creation_time
            && left.path == right.path
            && left.handle == right.handle
    });
    Ok(locks)
}

fn open_path(path: &Path, access: u32, sharing: u32) -> std::io::Result<File> {
    OpenOptions::new()
        .access_mode(access)
        .share_mode(sharing)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

fn file_id(handle: HANDLE) -> Option<(u32, u32, u32)> {
    let mut information = FileInformation::default();
    if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0
        || (information.index_high == 0 && information.index_low == 0)
    {
        return None;
    }
    Some((
        information.volume_serial,
        information.index_high,
        information.index_low,
    ))
}

fn final_path(handle: HANDLE, volume_flags: u32) -> Result<Vec<u16>, String> {
    let mut last_error = String::new();
    // Normalized names resolve aliases; OPENED also works when SMB denies traversing
    // a parent directory, which makes the normalized-name query fail.
    for flags in [volume_flags, volume_flags | 8] {
        let mut buffer = vec![0u16; 512];
        loop {
            let length = unsafe {
                GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, flags)
            } as usize;
            if length == 0 {
                last_error = format!(
                    "Resolve file handle path: {}",
                    std::io::Error::last_os_error()
                );
                break;
            }
            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(buffer);
            }
            if length > 32768 {
                return Err("Resolved file handle path exceeds the Windows path limit".to_owned());
            }
            buffer.resize(length + 1, 0);
        }
    }
    Err(last_error)
}

fn path_matches(target: &[u16], candidate: &[u16], directory: bool) -> bool {
    let target = trim_separator(target);
    let candidate = trim_separator(candidate);
    if candidate.len() < target.len()
        || (!directory && candidate.len() != target.len())
        || (directory && candidate.len() > target.len() && candidate[target.len()] != b'\\' as u16)
    {
        return false;
    }
    unsafe {
        CompareStringOrdinal(
            target.as_ptr(),
            target.len() as i32,
            candidate.as_ptr(),
            target.len() as i32,
            1,
        ) == 2
    }
}

fn trim_separator(path: &[u16]) -> &[u16] {
    let end = path
        .iter()
        .rposition(|value| *value != b'\\' as u16)
        .map_or(0, |index| index + 1);
    &path[..end]
}

fn display_path(path: &[u16]) -> String {
    let path = String::from_utf16_lossy(path);
    if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        path.strip_prefix("\\\\?\\").unwrap_or(&path).to_owned()
    }
}

fn system_handles() -> Result<Vec<SystemHandle>, String> {
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xc000_0004u32 as i32;
    const STATUS_BUFFER_TOO_SMALL: i32 = 0xc000_0023u32 as i32;
    let mut bytes: usize = 1024 * 1024;
    loop {
        if bytes > 256 * 1024 * 1024 {
            return Err("System handle snapshot exceeds 256 MiB".to_owned());
        }
        // Pointer-sized allocation keeps both the native header and entries aligned.
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let mut returned = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                64, // SystemExtendedHandleInformation
                storage.as_mut_ptr().cast::<c_void>(),
                (storage.len() * size_of::<usize>()) as u32,
                &mut returned,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH || status == STATUS_BUFFER_TOO_SMALL {
            bytes = (bytes * 2).max(returned as usize);
            continue;
        }
        if status < 0 {
            return Err(format!(
                "Query system handles: NTSTATUS {:#010x}",
                status as u32
            ));
        }
        let header = 2 * size_of::<usize>();
        let available = returned as usize;
        if available < header
            || available > storage.len() * size_of::<usize>()
            || storage[0] > (available - header) / size_of::<SystemHandle>()
        {
            return Err("Invalid SystemExtendedHandleInformation response".to_owned());
        }
        let entries = unsafe {
            std::slice::from_raw_parts(storage.as_ptr().add(2).cast::<SystemHandle>(), storage[0])
        };
        return Ok(entries.to_vec());
    }
}

fn append_directory_locks(
    root: &Path,
    locks: &mut Vec<FileLock>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    let mut pending = vec![root.to_owned()];
    let mut batch = Vec::with_capacity(RM_BATCH_SIZE);
    while let Some(directory) = pending.pop() {
        check_cancelled(cancelled)?;
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error)
                if directory != root
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                    ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "Enumerate lock-query directory {}: {error}",
                    directory.display()
                ));
            }
        };
        for entry in entries {
            check_cancelled(cancelled)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!("Enumerate {}: {error}", directory.display()));
                }
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("Inspect {}: {error}", entry.path().display())),
            };
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                continue; // Do not leave the queried tree or recurse through junction cycles.
            }
            let path = entry.path();
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                // Request DELETE as well as read access to catch mapped executable images.
                // This is only an open probe: it never writes, deletes, or sets delete-on-close.
                match open_path(&path, 0x8001_0000, 0) {
                    Ok(file) => drop(file),
                    Err(error) if matches!(error.raw_os_error(), Some(5 | 32 | 33)) => {
                        batch.push(path);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(format!("Probe lock on {}: {error}", path.display())),
                }
                if batch.len() == RM_BATCH_SIZE {
                    append_restart_locks(&batch, locks, cancelled)?;
                    batch.clear();
                }
            }
        }
    }
    if !batch.is_empty() {
        append_restart_locks(&batch, locks, cancelled)?;
    }
    Ok(())
}

fn restart_processes(paths: &[PathBuf]) -> Result<Vec<RmProcessInfo>, String> {
    use std::os::windows::ffi::OsStrExt;
    let mut session = 0;
    let mut key = [0u16; 33];
    win32_status(
        unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) },
        "RmStartSession",
    )?;
    let session = RestartSession(session);
    let names: Vec<Vec<u16>> = paths
        .iter()
        .map(|path| path.as_os_str().encode_wide().chain(Some(0)).collect())
        .collect();
    let pointers: Vec<_> = names.iter().map(|name| name.as_ptr()).collect();
    win32_status(
        unsafe {
            RmRegisterResources(
                session.0,
                pointers.len() as u32,
                pointers.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            )
        },
        "RmRegisterResources",
    )?;
    let mut processes = Vec::<RmProcessInfo>::new();
    for _ in 0..8 {
        let mut needed = 0;
        let mut count = processes.len() as u32;
        let mut reboot_reasons = 0;
        let status = unsafe {
            RmGetList(
                session.0,
                &mut needed,
                &mut count,
                if processes.is_empty() {
                    std::ptr::null_mut()
                } else {
                    processes.as_mut_ptr()
                },
                &mut reboot_reasons,
            )
        };
        if status == ERROR_MORE_DATA {
            if needed == 0 {
                continue;
            }
            // All fields of RM_PROCESS_INFO admit the zero bit pattern.
            processes.resize(needed as usize, unsafe { std::mem::zeroed() });
            continue;
        }
        win32_status(status, "RmGetList")?;
        if count as usize > processes.len() {
            return Err("RmGetList returned an invalid process count".to_owned());
        }
        processes.truncate(count as usize);
        return Ok(processes);
    }
    Err("Restart Manager's process list kept changing; retry the query".to_owned())
}

fn append_restart_locks(
    paths: &[PathBuf],
    locks: &mut Vec<FileLock>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    check_cancelled(cancelled)?;
    let processes = restart_processes(paths)?;
    if processes.is_empty() {
        return Ok(());
    }
    if paths.len() > 1 {
        // RM returns a batch-wide union, not a file-to-process mapping. Split only
        // positive batches to avoid attributing every process to every child file.
        let (left, right) = paths.split_at(paths.len() / 2);
        append_restart_locks(left, locks, cancelled)?;
        return append_restart_locks(right, locks, cancelled);
    }
    use std::os::windows::ffi::OsStrExt;
    let display = display_path(&paths[0].as_os_str().encode_wide().collect::<Vec<_>>());
    for process in processes {
        let creation_time = (u64::from(process.process.start_time.high) << 32)
            | u64::from(process.process.start_time.low);
        if locks.iter().any(|lock| {
            lock.process_id == process.process.process_id
                && lock.creation_time == creation_time
                && lock.path == display
        }) {
            continue;
        }
        let end = process
            .app_name
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(process.app_name.len());
        let name = String::from_utf16_lossy(&process.app_name[..end]);
        locks.push(FileLock {
            process_id: process.process.process_id,
            creation_time,
            process_name: if name.is_empty() {
                process_name(process.process.process_id)
            } else {
                name
            },
            path: display.clone(),
            handle: None,
        });
    }
    Ok(())
}

fn win32_status(status: u32, operation: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "{operation}: {}",
            std::io::Error::from_raw_os_error(status as i32)
        ))
    }
}

fn open_process(pid: u32, rights: u32) -> Result<Handle, String> {
    let handle = unsafe { OpenProcess(rights, 0, pid) };
    if handle.0.is_null() {
        Err(format!(
            "Open process {pid}: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(Handle(handle))
    }
}

fn process_name(pid: u32) -> String {
    if let Ok(process) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        if unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut length) }
            != 0
        {
            let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize]));
            if let Some(name) = path.file_name() {
                return name.to_string_lossy().into_owned();
            }
        }
    }
    format!("PID {pid}")
}

fn duplicate_handle(process: HANDLE, value: usize, options: u32) -> Result<OwnedHandle, String> {
    let mut duplicate = HANDLE::default();
    if unsafe {
        DuplicateHandle(
            process,
            HANDLE(value as *mut c_void),
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            options,
        )
    } == 0
    {
        Err(format!(
            "DuplicateHandle: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(duplicate.0) })
    }
}

/// The caller must enforce permission and obtain the destructive-action confirmation.
/// The process generation and duplicated file handle are checked before acting.
pub fn unlock(identity: ProcessIdentity, path: &Path, handle: Option<u64>) -> Result<(), String> {
    let process_id = identity.pid;
    if identity.creation_time == 0
        || process_id <= 4
        || process_id == unsafe { GetCurrentProcessId() }
    {
        return Err("Refusing to unlock a system process or Orbment itself".to_owned());
    }
    let rights = if handle.is_some() {
        PROCESS_DUP_HANDLE
    } else {
        PROCESS_TERMINATE
    };
    let process = process::open_verified(
        identity,
        windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS(rights),
    )
    .map_err(|error| format!("{error:#}"))?;
    let mut critical = 0;
    if unsafe { IsProcessCritical(process.0, &mut critical) } == 0 {
        return Err(format!(
            "Check process {process_id} safety: {}",
            std::io::Error::last_os_error()
        ));
    }
    if critical != 0 {
        return Err(format!("Refusing to unlock critical process {process_id}"));
    }
    if let Some(value) = handle {
        let value = usize::try_from(value)
            .map_err(|_| "Handle does not fit this architecture".to_owned())?;
        if value == 0 || value >= usize::MAX - 15 || value & 3 != 0 {
            return Err("Invalid file handle value".to_owned());
        }
        let check = duplicate_handle(process.0, value, DUPLICATE_SAME_ACCESS)?;
        verify_file_handle(path, check)?;
        // Windows has no compare-and-close operation for a foreign handle. Keep
        // validation adjacent to close, though concurrent source reuse can still race.
        let closed = duplicate_handle(
            process.0,
            value,
            DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
        )
        .map_err(|error| format!("{error}; Windows may already have closed the source handle"))?;
        drop(closed);
    } else if unsafe { TerminateProcess(process.0, 1) } == 0 {
        return Err(format!(
            "Terminate process {process_id}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn verify_file_handle(path: &Path, handle: OwnedHandle) -> Result<(), String> {
    let target = open_path(path, 0, FILE_SHARE_ALL)
        .map_err(|error| format!("Open expected lock target {}: {error}", path.display()))?;
    let target_handle = HANDLE(target.as_raw_handle());
    let mut resolver =
        FileResolver::new(final_path(target_handle, 2)?, file_id(target_handle), false);
    match resolver.resolve(handle)? {
        Resolution::Path(_) => Ok(()),
        Resolution::Missing => {
            Err("The supplied handle no longer refers to the expected file".to_owned())
        }
        Resolution::TimedOut => Err("Timed out verifying the expected file handle".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlock_rejects_a_stale_process_generation_without_terminating_it() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "set /p ORBMENT_UNLOCK_TEST="])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let identity = ProcessIdentity {
            pid: child.id(),
            creation_time: process::creation_time(HANDLE(child.as_raw_handle()))
                .unwrap()
                .wrapping_add(1),
        };
        let result = unlock(identity, Path::new("unused"), None);
        let still_running = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        child.wait().unwrap();
        assert!(result.is_err());
        assert!(still_running);
    }

    #[test]
    fn handle_verification_rejects_other_files_and_accepts_hard_links() {
        let root = std::env::temp_dir().join(format!(
            "orbment-unlock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let expected = root.join("expected");
        let other = root.join("other");
        let alias = root.join("alias");
        std::fs::write(&expected, b"expected").unwrap();
        std::fs::write(&other, b"other").unwrap();
        std::fs::hard_link(&expected, &alias).unwrap();

        let target = open_path(&expected, 0, FILE_SHARE_ALL).unwrap();
        let target_handle = HANDLE(target.as_raw_handle());
        let resolver = ResolverTarget {
            path: final_path(target_handle, 2).unwrap(),
            id: file_id(target_handle),
            directory: false,
        };
        let other = open_path(&other, 0, FILE_SHARE_ALL).unwrap();
        assert!(resolver.resolve(HANDLE(other.as_raw_handle())).is_none());
        let alias = open_path(&alias, 0, FILE_SHARE_ALL).unwrap();
        assert!(resolver.resolve(HANDLE(alias.as_raw_handle())).is_some());
        let directory = open_path(&root, 0, FILE_SHARE_ALL).unwrap();
        let resolver = ResolverTarget {
            path: final_path(HANDLE(directory.as_raw_handle()), 2).unwrap(),
            id: file_id(HANDLE(directory.as_raw_handle())),
            directory: false,
        };
        assert!(resolver.resolve(target_handle).is_none());

        drop((target, other, alias, directory));
        std::fs::remove_dir_all(root).unwrap();
    }
}
