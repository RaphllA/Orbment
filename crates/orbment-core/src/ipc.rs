use crate::i18n::tr;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{
            ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
            GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE,
            OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION,
            SECURITY_SQOS_PRESENT, WriteFile,
        },
        System::{
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
                PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
            },
            Threading::{
                CreateEventW, GetCurrentProcess, OpenProcessToken, SetEvent,
                WaitForMultipleObjects, WaitForSingleObject,
            },
        },
    },
    core::{PCWSTR, PWSTR, w},
};

pub const PIPE_NAME: &str = r"\\.\pipe\orbment-control-v1";
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub action: String,
    #[serde(default)]
    pub params: Value,
    #[serde(skip)]
    cancellation: Option<Arc<AtomicBool>>,
}

impl ActionRequest {
    pub fn new(action: impl Into<String>, params: Value) -> Self {
        Self {
            action: action.into(),
            params,
            cancellation: None,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActionResponse {
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl ActionResponse {
    fn from_result(result: Result<Value>) -> Self {
        match result {
            Ok(result) => Self {
                result: Some(result),
                error: None,
            },
            Err(error) => Self {
                result: None,
                error: Some(error.to_string()),
            },
        }
    }
}

#[derive(Clone)]
pub struct RequestSender {
    sender: SyncSender<PendingRequest>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

pub struct PendingRequest {
    pub request: ActionRequest,
    reply: Box<dyn FnOnce(Result<Value>) + Send>,
    cancelled: Arc<AtomicBool>,
}

impl PendingRequest {
    pub fn new(
        request: ActionRequest,
        cancelled: Arc<AtomicBool>,
        reply: impl FnOnce(Result<Value>) + Send + 'static,
    ) -> Self {
        Self {
            request,
            reply: Box::new(reply),
            cancelled,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn reply(self, result: Result<Value>) {
        (self.reply)(result);
    }
}

pub fn request_channel(
    wake: impl Fn() + Send + Sync + 'static,
) -> (RequestSender, Receiver<PendingRequest>) {
    let (sender, receiver) = mpsc::sync_channel(64);
    (
        RequestSender {
            sender,
            wake: Arc::new(wake),
        },
        receiver,
    )
}

impl RequestSender {
    pub fn call(&self, request: ActionRequest) -> Result<Value> {
        let (reply, receiver) = mpsc::sync_channel(1);
        let cancelled = request
            .cancellation
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        self.sender
            .try_send(PendingRequest::new(
                request,
                cancelled.clone(),
                move |result| {
                    let _ = reply.send(result);
                },
            ))
            .map_err(|_| anyhow::anyhow!("{}", tr("ipc.queue_unavailable")))?;
        (self.wake)();
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            ensure!(!cancelled.load(Ordering::Acquire), "{}", tr("ipc.stopped"));
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                cancelled.store(true, Ordering::Release);
                bail!("{}", tr("ipc.request_timeout"));
            }
            match receiver.recv_timeout(remaining.min(Duration::from_millis(20))) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(error) => {
                    cancelled.store(true, Ordering::Release);
                    return Err(error).with_context(|| tr("ipc.queue_unavailable"));
                }
            }
        }
    }
}

fn handle(value: &OwnedHandle) -> HANDLE {
    HANDLE(value.as_raw_handle())
}

fn owned(value: HANDLE) -> OwnedHandle {
    // All callers pass a successfully created non-inheritable kernel handle.
    unsafe { OwnedHandle::from_raw_handle(value.0) }
}

fn event() -> Result<OwnedHandle> {
    Ok(owned(unsafe { CreateEventW(None, true, false, None)? }))
}

struct LocalAllocation(HLOCAL);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(self.0));
        }
    }
}

/// Uses TokenUser rather than TokenOwner: elevated tokens may be owned by Administrators.
fn current_user_sid() -> Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let token = owned(token);
        let mut bytes = 0;
        match GetTokenInformation(handle(&token), TokenUser, None, 0, &mut bytes) {
            Err(error) if error.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => {}
            Err(error) => return Err(error.into()),
            Ok(()) => bail!("{}", tr("ipc.security_failed")),
        }
        // TokenUser embeds pointers and therefore needs pointer-aligned storage.
        let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        GetTokenInformation(
            handle(&token),
            TokenUser,
            Some(storage.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )?;
        let user = &*storage.as_ptr().cast::<TOKEN_USER>();
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
        let _allocation = LocalAllocation(HLOCAL(sid.0.cast()));
        Ok(sid.to_string()?)
    }
}

fn create_server_pipe() -> Result<OwnedHandle> {
    let sid = current_user_sid().with_context(|| tr("ipc.security_failed"))?;
    let sddl: Vec<u16> = format!("D:P(A;;GRGW;;;{sid})S:(ML;;NW;;;ME)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .with_context(|| tr("ipc.security_failed"))?;
        let _allocation = LocalAllocation(HLOCAL(descriptor.0));
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let pipe = CreateNamedPipeW(
            w!(r"\\.\pipe\orbment-control-v1"),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            5000,
            Some(&security),
        );
        if pipe.is_invalid() {
            return Err(windows::core::Error::from_win32())
                .with_context(|| tr("ipc.listen_failed"));
        }
        Ok(owned(pipe))
    }
}

/// One serialized connection at a time; an unresponsive peer cannot block shutdown.
pub struct PipeServer {
    stop: Arc<OwnedHandle>,
    worker: Option<JoinHandle<()>>,
    dispatch_worker: Option<JoinHandle<()>>,
}

impl PipeServer {
    /// Handlers must return promptly or honor `ActionRequest::is_cancelled`.
    /// `RequestSender::call` provides cancellation-aware main-thread dispatch.
    pub fn start(
        handler: impl Fn(ActionRequest) -> Result<Value> + Send + 'static,
    ) -> Result<Self> {
        let pipe = create_server_pipe()?;
        let stop = Arc::new(event()?);
        let worker_stop = stop.clone();
        let (dispatch, requests) = mpsc::channel::<(ActionRequest, SyncSender<Result<Value>>)>();
        // Keep user callbacks off the pipe thread: UI teardown may itself be waiting for it.
        let dispatch_worker = thread::Builder::new()
            .name("orbment-ipc-dispatch".into())
            .spawn(move || {
                for (request, reply) in requests {
                    let _ = reply.send(handler(request));
                }
            })
            .with_context(|| tr("ipc.listen_failed"))?;
        let worker = thread::Builder::new()
            .name("orbment-ipc".into())
            .spawn(move || {
                while !stopped(&worker_stop) {
                    let connected = connect(&pipe, &worker_stop);
                    if stopped(&worker_stop) {
                        break;
                    }
                    if let Err(error) = connected {
                        tracing::warn!(%error, "IPC connection failed");
                        break;
                    }
                    let exchange = (|| -> Result<()> {
                        let deadline = Instant::now() + REQUEST_TIMEOUT;
                        let cancellation = Arc::new(AtomicBool::new(false));
                        let _cancel_on_exit = CancelOnDrop(cancellation.clone());
                        let request = read_json::<ActionRequest>(&pipe, &worker_stop, deadline);
                        let result = match request {
                            Ok(mut request) => {
                                request.cancellation = Some(cancellation);
                                let (reply, receiver) = mpsc::sync_channel(1);
                                dispatch
                                    .send((request, reply))
                                    .with_context(|| tr("ipc.queue_unavailable"))?;
                                loop {
                                    if stopped(&worker_stop) {
                                        bail!("{}", tr("ipc.stopped"));
                                    }
                                    if Instant::now() >= deadline {
                                        bail!("{}", tr("ipc.request_timeout"));
                                    }
                                    match receiver.recv_timeout(Duration::from_millis(20)) {
                                        Ok(result) => break result,
                                        Err(RecvTimeoutError::Timeout) => continue,
                                        Err(error) => {
                                            return Err(error)
                                                .with_context(|| tr("ipc.queue_unavailable"));
                                        }
                                    }
                                }
                            }
                            Err(error) => Err(error),
                        };
                        write_json(
                            &pipe,
                            &worker_stop,
                            &ActionResponse::from_result(result),
                            deadline,
                        )?;
                        // DisconnectNamedPipe discards unread output; the reader acknowledges consumption.
                        let mut acknowledgement = [0u8];
                        read_exact(&pipe, &worker_stop, &mut acknowledgement, deadline)?;
                        Ok(())
                    })();
                    if let Err(error) = exchange
                        && !stopped(&worker_stop)
                    {
                        tracing::debug!(%error, "IPC request failed");
                    }
                    unsafe {
                        let _ = DisconnectNamedPipe(handle(&pipe));
                    }
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                let _ = dispatch_worker.join();
                return Err(error).with_context(|| tr("ipc.listen_failed"));
            }
        };
        Ok(Self {
            stop,
            worker: Some(worker),
            dispatch_worker: Some(dispatch_worker),
        })
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        unsafe {
            let _ = SetEvent(handle(&self.stop));
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.dispatch_worker.take() {
            let _ = worker.join();
        }
    }
}

fn stopped(stop: &OwnedHandle) -> bool {
    unsafe { WaitForSingleObject(handle(stop), 0) == WAIT_OBJECT_0 }
}

fn connect(pipe: &OwnedHandle, stop: &OwnedHandle) -> Result<()> {
    let ready = event()?;
    let mut operation = OVERLAPPED {
        hEvent: handle(&ready),
        ..Default::default()
    };
    unsafe {
        match ConnectNamedPipe(handle(pipe), Some(&mut operation)) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(()),
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                finish_io(pipe, stop, &mut operation, u32::MAX).map(|_| ())
            }
            Err(error) => Err(error).with_context(|| tr("ipc.transport_failed")),
        }
    }
}

fn finish_io(
    pipe: &OwnedHandle,
    stop: &OwnedHandle,
    operation: &mut OVERLAPPED,
    timeout_ms: u32,
) -> Result<u32> {
    unsafe {
        let wait = WaitForMultipleObjects(&[handle(stop), operation.hEvent], false, timeout_ms);
        let mut transferred = 0;
        if wait.0 == WAIT_OBJECT_0.0 + 1 {
            GetOverlappedResult(handle(pipe), operation, &mut transferred, false)
                .with_context(|| tr("ipc.transport_failed"))?;
            return Ok(transferred);
        }
        // Drain cancellation before stack buffers or OVERLAPPED storage go out of scope.
        let _ = CancelIoEx(handle(pipe), Some(operation));
        let _ = GetOverlappedResult(handle(pipe), operation, &mut transferred, true);
        if wait == WAIT_OBJECT_0 {
            bail!("{}", tr("ipc.stopped"));
        }
        if wait == WAIT_TIMEOUT {
            bail!("{}", tr("ipc.request_timeout"));
        }
        Err(windows::core::Error::from_win32()).with_context(|| tr("ipc.transport_failed"))
    }
}

fn transfer(
    pipe: &OwnedHandle,
    stop: &OwnedHandle,
    read: Option<&mut [u8]>,
    write: Option<&[u8]>,
    deadline: Instant,
) -> Result<usize> {
    let timeout = deadline.saturating_duration_since(Instant::now());
    ensure!(!timeout.is_zero(), "{}", tr("ipc.request_timeout"));
    let ready = event()?;
    let mut operation = OVERLAPPED {
        hEvent: handle(&ready),
        ..Default::default()
    };
    let mut transferred = 0;
    let result = unsafe {
        if let Some(buffer) = read {
            ReadFile(
                handle(pipe),
                Some(buffer),
                Some(&mut transferred),
                Some(&mut operation),
            )
        } else {
            WriteFile(
                handle(pipe),
                write,
                Some(&mut transferred),
                Some(&mut operation),
            )
        }
    };
    match result {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
            transferred = finish_io(
                pipe,
                stop,
                &mut operation,
                timeout.as_millis().min(u32::MAX as u128 - 1) as u32,
            )?;
        }
        Err(error) => return Err(error).with_context(|| tr("ipc.transport_failed")),
    }
    ensure!(transferred != 0, "{}", tr("ipc.disconnected"));
    Ok(transferred as usize)
}

fn read_exact(
    pipe: &OwnedHandle,
    stop: &OwnedHandle,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> Result<()> {
    while !bytes.is_empty() {
        let read = transfer(pipe, stop, Some(bytes), None, deadline)?;
        bytes = &mut bytes[read..];
    }
    Ok(())
}

fn write_all(
    pipe: &OwnedHandle,
    stop: &OwnedHandle,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<()> {
    while !bytes.is_empty() {
        let written = transfer(pipe, stop, None, Some(bytes), deadline)?;
        bytes = &bytes[written..];
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(
    pipe: &OwnedHandle,
    stop: &OwnedHandle,
    deadline: Instant,
) -> Result<T> {
    let mut header = [0u8; 4];
    read_exact(pipe, stop, &mut header, deadline)?;
    let length = u32::from_le_bytes(header) as usize;
    ensure!(
        (1..=MAX_FRAME_BYTES).contains(&length),
        "{}",
        tr("ipc.invalid_frame")
    );
    let mut bytes = vec![0; length];
    read_exact(pipe, stop, &mut bytes, deadline)?;
    serde_json::from_slice(&bytes).with_context(|| tr("ipc.invalid_frame"))
}

fn write_json(
    pipe: &OwnedHandle,
    stop: &OwnedHandle,
    value: &impl Serialize,
    deadline: Instant,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_FRAME_BYTES,
        "{}",
        tr("ipc.invalid_frame")
    );
    write_all(pipe, stop, &(bytes.len() as u32).to_le_bytes(), deadline)?;
    write_all(pipe, stop, &bytes, deadline)
}

/// Stateless client: never starts Orbment, requests elevation, or initializes UI/COM.
pub fn request(action: &str, params: Value) -> Result<Value> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let pipe = loop {
        match unsafe {
            CreateFileW(
                w!(r"\\.\pipe\orbment-control-v1"),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        } {
            Ok(pipe) => break owned(pipe),
            Err(error) if error.code() == ERROR_PIPE_BUSY.to_hresult() => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(!remaining.is_zero(), "{}", tr("ipc.request_timeout"));
                unsafe {
                    WaitNamedPipeW(
                        w!(r"\\.\pipe\orbment-control-v1"),
                        remaining.as_millis() as u32,
                    )
                }
                .ok()
                .with_context(|| tr("ipc.unavailable"))?;
            }
            Err(error) => return Err(error).with_context(|| tr("ipc.unavailable")),
        }
    };
    let stop = event()?;
    write_json(&pipe, &stop, &ActionRequest::new(action, params), deadline)?;
    let response: ActionResponse = read_json(&pipe, &stop, deadline)?;
    write_all(&pipe, &stop, &[0], deadline)?;
    if let Some(error) = response.error {
        bail!("{error}");
    }
    // Option<Value> represents JSON null as None on deserialization.
    Ok(response.result.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_bus::ActionBus;

    #[test]
    fn queue_replies_from_the_action_bus_owner_thread() -> Result<()> {
        let owner = thread::current().id();
        let mut bus = ActionBus::default();
        bus.register("app.thread", move |params| {
            assert_eq!(thread::current().id(), owner);
            Ok(params)
        })?;
        let (sender, receiver) = request_channel(|| {});
        let worker = thread::spawn(move || {
            sender.call(ActionRequest::new(
                "app.thread",
                serde_json::json!({"value":7}),
            ))
        });
        let pending = receiver.recv()?;
        let result = bus.call(&pending.request.action, pending.request.params.clone());
        pending.reply(result);
        assert_eq!(worker.join().unwrap()?["value"], 7);
        Ok(())
    }

    #[test]
    fn stopping_a_request_marks_pending_and_unblocks_the_waiter() -> Result<()> {
        let (sender, receiver) = request_channel(|| {});
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut request = ActionRequest::new("app.cancelled", Value::Null);
        request.cancellation = Some(cancellation.clone());
        let worker = thread::spawn(move || sender.call(request));
        let pending = receiver.recv()?;
        drop(CancelOnDrop(cancellation));
        assert!(pending.is_cancelled());
        drop(pending);
        assert!(worker.join().unwrap().is_err());
        Ok(())
    }
}
