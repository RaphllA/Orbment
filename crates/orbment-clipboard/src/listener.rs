use crate::{ClipboardStore, native, types::*};
use anyhow::{Context, Result, ensure};
use orbment_core::{
    config::{ClipboardConfig, ConfigStore},
    i18n::tr,
    ocr::{OcrEngine, WindowsNativeOcr},
};
use std::{
    collections::BTreeSet,
    ffi::c_void,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type OcrRequested = Arc<dyn Fn(i64) + Send + Sync>;
#[derive(Default)]
struct OcrRequests {
    callback: Option<OcrRequested>,
    pending: BTreeSet<i64>,
}

impl OcrRequests {
    fn subscribe(shared: &Mutex<Self>, callback: OcrRequested) {
        let pending = {
            let mut requests = shared.lock().expect("clipboard OCR request lock poisoned");
            requests.callback = Some(callback.clone());
            requests.pending.clone()
        };
        for seq in pending {
            callback(seq);
        }
    }

    fn enqueue(
        shared: &Mutex<Self>,
        sender: &Sender<OcrCommand>,
        has_ocr: bool,
        seq: i64,
    ) -> Result<()> {
        let callback = {
            let mut requests = shared
                .lock()
                .map_err(|_| anyhow::anyhow!(tr("clipboard.error.store_poisoned")))?;
            if !requests.pending.insert(seq) {
                return Ok(());
            }
            if has_ocr {
                if sender.send(OcrCommand::Recognize(seq)).is_err() {
                    requests.pending.remove(&seq);
                    anyhow::bail!("{}", tr("clipboard.error.stopped"));
                }
                None
            } else {
                requests.callback.clone()
            }
        };
        if let Some(callback) = callback {
            callback(seq);
        }
        Ok(())
    }
}

struct CommandReply {
    sender: Option<Sender<Result<()>>>,
    wake: Option<Box<dyn FnOnce() + Send>>,
}

impl CommandReply {
    fn finish(mut self, result: Result<()>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

impl Drop for CommandReply {
    fn drop(&mut self) {
        // Closing the result channel must precede waking its UI consumer, including on shutdown.
        self.sender.take();
        if let Some(wake) = self.wake.take() {
            wake();
        }
    }
}

enum Command {
    Write {
        content: WriteContent,
        target: Option<Option<isize>>,
        seq: Option<i64>,
        cancelled: Arc<AtomicBool>,
        reply: CommandReply,
    },
}
enum OcrCommand {
    Recognize(i64),
    Stop,
}

pub struct ClipboardService {
    store: ClipboardStore,
    sender: Sender<Command>,
    hwnd: isize,
    listener: Option<JoinHandle<()>>,
    ocr_sender: Sender<OcrCommand>,
    ocr_worker: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
    has_ocr: bool,
    ocr_requests: Arc<Mutex<OcrRequests>>,
}

impl ClipboardService {
    pub fn start(data_dir: &Path, config: ClipboardConfig) -> Result<Self> {
        let global = ConfigStore::load(data_dir)?;
        let engine: Option<Arc<dyn OcrEngine>> = match global.config.app.ocr_engine_tier.as_str() {
            "WindowsNative" => Some(Arc::new(WindowsNativeOcr::new(None))),
            _ => None,
        };
        Self::start_with_ocr(data_dir, config, engine)
    }

    /// The app resolves the global OCR tier; None defers new image recognition to an explicit request.
    pub fn start_with_ocr(
        data_dir: &Path,
        config: ClipboardConfig,
        engine: Option<Arc<dyn OcrEngine>>,
    ) -> Result<Self> {
        let store = ClipboardStore::open(data_dir, &config)?;
        let (sender, receiver) = mpsc::channel();
        let (ocr_sender, ocr_receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(OcrRequests::default()));
        let mut service = Self {
            store: store.clone(),
            sender,
            hwnd: 0,
            listener: None,
            ocr_sender: ocr_sender.clone(),
            ocr_worker: None,
            stopping: stopping.clone(),
            has_ocr: engine.is_some(),
            ocr_requests: requests.clone(),
        };
        if let Some(engine) = engine {
            let worker_store = store.clone();
            let worker_stopping = stopping.clone();
            let worker_requests = requests.clone();
            service.ocr_worker = Some(thread::Builder::new().name("clipboard-ocr".into()).spawn(
                move || {
                    while let Ok(command) = ocr_receiver.recv() {
                        if worker_stopping.load(Ordering::Acquire) {
                            break;
                        }
                        let OcrCommand::Recognize(seq) = command else {
                            break;
                        };
                        let result = (|| -> Result<()> {
                            let Some(item) = worker_store.get(seq)? else {
                                return Ok(());
                            };
                            let Some(path) = worker_store.media_path(&item)? else {
                                return Ok(());
                            };
                            if worker_stopping.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            let result = engine.recognize(&path)?;
                            if !worker_stopping.load(Ordering::Acquire) {
                                worker_store.set_ocr_text(seq, &item.content, &result.text)?;
                            }
                            Ok(())
                        })();
                        if let Err(error) = result {
                            tracing::warn!(%error,seq,"clipboard OCR failed");
                        }
                        if !worker_stopping.load(Ordering::Acquire)
                            && let Ok(mut requests) = worker_requests.lock()
                        {
                            requests.pending.remove(&seq);
                        }
                    }
                },
            )?);
        }
        let state = ListenerState {
            store,
            config,
            receiver,
            stopping: stopping.clone(),
            ocr_sender,
            has_ocr: service.has_ocr,
            requests,
            last_sequence: None,
            last_cleanup: Instant::now(),
            retry_count: 0,
        };
        service.listener = Some(thread::Builder::new().name("clipboard-sta".into()).spawn(
            move || {
                let apartment = match native::Apartment::initialize() {
                    Ok(apartment) => apartment,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let mut state = Box::new(state);
                let window = match unsafe {
                    native::Window::create(
                        window_proc,
                        (&mut *state as *mut ListenerState).cast::<c_void>(),
                    )
                } {
                    Ok(window) => window,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                if state.config.history.cleanup_interval_hours > 0
                    && unsafe { native::SetTimer(window.hwnd, 1, 60_000, std::ptr::null()) } == 0
                {
                    let _ = ready_tx.send(Err(std::io::Error::last_os_error().into()));
                    return;
                }
                let _ = ready_tx.send(Ok(window.hwnd));
                if !stopping.load(Ordering::Acquire)
                    && (state.config.history.retention_value > 0
                        || state.config.history.max_count > 0)
                    && let Err(error) = state.store.cleanup()
                {
                    tracing::warn!(%error,"clipboard initial cleanup failed");
                }
                unsafe {
                    native::PostMessageW(window.hwnd, native::WM_CLIPBOARDUPDATE, 0, 0);
                }
                let mut message = native::Message::default();
                loop {
                    let status = unsafe { native::GetMessageW(&mut message, 0, 0, 0) };
                    if status <= 0 || stopping.load(Ordering::Acquire) {
                        break;
                    }
                    unsafe {
                        native::TranslateMessage(&message);
                        native::DispatchMessageW(&message);
                    }
                }
                unsafe {
                    native::KillTimer(window.hwnd, 1);
                    native::KillTimer(window.hwnd, 2);
                }
                drop(window);
                drop(state);
                drop(apartment);
            },
        )?);
        service.hwnd = ready_rx.recv().context(tr("clipboard.error.stopped"))??;
        Ok(service)
    }

    pub fn store(&self) -> ClipboardStore {
        self.store.clone()
    }
    pub fn set_on_change(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        self.store.set_on_change(callback);
    }

    pub fn set_on_ocr_requested(&self, callback: OcrRequested) {
        if !self.has_ocr {
            OcrRequests::subscribe(&self.ocr_requests, callback);
        }
    }

    pub fn recognize_item(&self, seq: i64) -> Result<()> {
        ensure!(self.has_ocr, "{}", tr("clipboard.error.ocr_unavailable"));
        let item = self
            .store
            .get(seq)?
            .context(tr("clipboard.error.item_missing"))?;
        ensure!(
            item.kind == "image",
            "{}",
            tr("clipboard.error.invalid_image")
        );
        OcrRequests::enqueue(&self.ocr_requests, &self.ocr_sender, self.has_ocr, seq)
    }

    pub fn write(
        &self,
        content: &WriteContent,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Receiver<Result<()>>> {
        self.command(content, None, None, wake)
    }

    pub fn paste(
        &self,
        content: &WriteContent,
        target: Option<isize>,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Receiver<Result<()>>> {
        self.command(content, Some(target), None, wake)
    }

    pub fn write_item(
        &self,
        seq: i64,
        options: WriteOptions,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Receiver<Result<()>>> {
        self.command(
            &self.store.write_content(seq, options)?,
            None,
            Some(seq),
            wake,
        )
    }

    pub fn paste_item(
        &self,
        seq: i64,
        options: WriteOptions,
        target: Option<isize>,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Receiver<Result<()>>> {
        self.command(
            &self.store.write_content(seq, options)?,
            Some(target),
            Some(seq),
            wake,
        )
    }

    fn command(
        &self,
        content: &WriteContent,
        target: Option<Option<isize>>,
        seq: Option<i64>,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Receiver<Result<()>>> {
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "{}",
            tr("clipboard.error.stopped")
        );
        let (reply, result) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.sender
            .send(Command::Write {
                content: content.clone(),
                target,
                seq,
                cancelled: cancelled.clone(),
                reply: CommandReply {
                    sender: Some(reply),
                    wake: Some(Box::new(wake)),
                },
            })
            .map_err(|_| anyhow::anyhow!(tr("clipboard.error.stopped")))?;
        if unsafe { native::PostMessageW(self.hwnd, native::WM_COMMAND, 0, 0) } == 0 {
            cancelled.store(true, Ordering::Release);
            anyhow::bail!("{}", tr("clipboard.error.stopped"));
        }
        Ok(result)
    }

    pub fn stop(mut self) -> Vec<i64> {
        self.shutdown();
        let mut requests = self
            .ocr_requests
            .lock()
            .expect("clipboard OCR request lock poisoned");
        std::mem::take(&mut requests.pending).into_iter().collect()
    }

    fn shutdown(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.ocr_sender.send(OcrCommand::Stop);
        // Recognition may block in the engine; the stopping checks discard its late result.
        drop(self.ocr_worker.take());
        if self.hwnd != 0 {
            unsafe {
                native::PostMessageW(self.hwnd, native::WM_CLOSE, 0, 0);
            }
            self.hwnd = 0;
        }
        if let Some(thread) = self.listener.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ClipboardService {
    fn drop(&mut self) {
        self.shutdown();
        if let Err(error) = self.store.checkpoint() {
            tracing::warn!(%error,"clipboard shutdown checkpoint failed");
        }
    }
}

struct ListenerState {
    store: ClipboardStore,
    config: ClipboardConfig,
    receiver: Receiver<Command>,
    stopping: Arc<AtomicBool>,
    ocr_sender: Sender<OcrCommand>,
    has_ocr: bool,
    requests: Arc<Mutex<OcrRequests>>,
    last_sequence: Option<u32>,
    last_cleanup: Instant,
    retry_count: u8,
}
impl ListenerState {
    fn capture(&mut self, hwnd: isize) -> Result<()> {
        let sequence = unsafe { native::GetClipboardSequenceNumber() };
        if self.last_sequence == Some(sequence) {
            return Ok(());
        }
        let captured = native::capture(hwnd, &self.config, &self.store)?;
        match captured {
            Some(native::Capture::Item(item)) => {
                self.store.save_capture(item)?;
            }
            Some(native::Capture::Image {
                png,
                dimensions,
                source,
            }) => {
                if let Some(seq) = self.store.save_png_capture(&png, dimensions, source)?
                    && self.config.capture.image_ocr
                {
                    OcrRequests::enqueue(&self.requests, &self.ocr_sender, self.has_ocr, seq)?;
                }
            }
            None => {}
        }
        self.last_sequence = Some(sequence);
        self.retry_count = 0;
        if self.config.history.cleanup_interval_hours == 0
            && (self.config.history.max_count > 0 || self.config.history.retention_value > 0)
        {
            self.store.cleanup()?;
        }
        Ok(())
    }

    fn commands(&mut self, hwnd: isize) {
        while let Ok(command) = self.receiver.try_recv() {
            if self.stopping.load(Ordering::Acquire) {
                // Dropping queued replies disconnects and wakes their UI consumers.
                continue;
            }
            match command {
                Command::Write {
                    content,
                    target,
                    seq,
                    cancelled,
                    reply,
                } => {
                    if cancelled.load(Ordering::Acquire) {
                        continue;
                    }
                    let result = crate::paste::write(hwnd, &content, &self.config)
                        .and_then(|()| {
                            if let Some(target) = target {
                                crate::paste::paste(target)
                            } else {
                                Ok(())
                            }
                        })
                        .and_then(|()| match seq {
                            Some(seq) => self.store.record_use(seq),
                            None => Ok(()),
                        });
                    reply.finish(result);
                }
            }
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: isize,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if message == 0x0081 {
        let create = unsafe { &*(lparam as *const native::CreateStruct) };
        unsafe {
            native::SetWindowLongPtrW(hwnd, -21, create.params as isize);
        }
    }
    let pointer = unsafe { native::GetWindowLongPtrW(hwnd, -21) } as *mut ListenerState;
    if !pointer.is_null() {
        let state = unsafe { &mut *pointer };
        match message {
            native::WM_CLIPBOARDUPDATE => {
                if let Err(error) = state.capture(hwnd) {
                    tracing::warn!(%error,"clipboard capture failed");
                    if state.retry_count < 3 {
                        state.retry_count += 1;
                        unsafe {
                            native::SetTimer(hwnd, 2, 100, std::ptr::null());
                        }
                    }
                }
                return 0;
            }
            native::WM_COMMAND => {
                state.commands(hwnd);
                return 0;
            }
            native::WM_TIMER => {
                if wparam == 2 {
                    unsafe {
                        native::KillTimer(hwnd, 2);
                        native::PostMessageW(hwnd, native::WM_CLIPBOARDUPDATE, 0, 0);
                    }
                } else if state.last_cleanup.elapsed()
                    >= Duration::from_secs(
                        u64::from(state.config.history.cleanup_interval_hours) * 3600,
                    )
                {
                    if let Err(error) = state.store.cleanup() {
                        tracing::warn!(%error,"clipboard cleanup failed");
                    }
                    state.last_cleanup = Instant::now();
                }
                return 0;
            }
            native::WM_CLOSE => {
                unsafe {
                    native::PostQuitMessage(0);
                }
                return 0;
            }
            _ => {}
        }
    }
    unsafe { native::DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_ocr_preserves_the_entire_batch_for_engine_rebuild() {
        let requests = Mutex::new(OcrRequests::default());
        let (worker, _) = mpsc::channel();
        for seq in [10, 11] {
            OcrRequests::enqueue(&requests, &worker, false, seq).unwrap();
        }
        let (prompt, prompts) = mpsc::channel();
        OcrRequests::subscribe(&requests, Arc::new(move |seq| prompt.send(seq).unwrap()));
        OcrRequests::enqueue(&requests, &worker, false, 12).unwrap();
        OcrRequests::enqueue(&requests, &worker, false, 11).unwrap();
        assert_eq!(prompts.try_iter().collect::<Vec<_>>(), [10, 11, 12]);

        let pending = std::mem::take(&mut requests.lock().unwrap().pending);
        let rebuilt = Mutex::new(OcrRequests::default());
        let (worker, commands) = mpsc::channel();
        for seq in pending {
            OcrRequests::enqueue(&rebuilt, &worker, true, seq).unwrap();
        }
        let recognized: Vec<_> = commands
            .try_iter()
            .map(|command| match command {
                OcrCommand::Recognize(seq) => seq,
                OcrCommand::Stop => panic!("unexpected stop"),
            })
            .collect();
        assert_eq!(recognized, [10, 11, 12]);
        assert_eq!(rebuilt.lock().unwrap().pending.len(), 3);
    }

    #[test]
    fn stop_detaches_blocked_ocr_and_preserves_pending_requests() {
        let data_dir =
            std::env::temp_dir().join(format!("orbment-clipboard-stop-{}", uuid::Uuid::new_v4()));
        let store = ClipboardStore::open(&data_dir, &ClipboardConfig::default()).unwrap();
        let (sender, _receiver) = mpsc::channel();
        let (ocr_sender, ocr_receiver) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(OcrRequests::default()));
        requests.lock().unwrap().pending.extend([10, 11]);
        let (release, blocked) = mpsc::channel();
        let (finished, completion) = mpsc::channel();
        let worker_stopping = stopping.clone();
        let worker = thread::spawn(move || {
            blocked.recv().unwrap();
            assert!(worker_stopping.load(Ordering::Acquire));
            assert!(matches!(ocr_receiver.recv().unwrap(), OcrCommand::Stop));
            finished.send(()).unwrap();
        });
        let service = ClipboardService {
            store,
            sender,
            hwnd: 0,
            listener: None,
            ocr_sender,
            ocr_worker: Some(worker),
            stopping: stopping.clone(),
            has_ocr: true,
            ocr_requests: requests,
        };
        let (stopped, stop_result) = mpsc::channel();
        let shutdown = thread::spawn(move || stopped.send(service.stop()).unwrap());
        let pending = stop_result.recv_timeout(Duration::from_secs(5));
        let was_stopping = stopping.load(Ordering::Acquire);
        let worker_was_blocked = matches!(completion.try_recv(), Err(mpsc::TryRecvError::Empty));
        // Release even if shutdown regresses to joining, so the test cannot strand the worker.
        release.send(()).unwrap();
        shutdown.join().unwrap();
        completion.recv_timeout(Duration::from_secs(5)).unwrap();
        std::fs::remove_dir_all(data_dir).unwrap();
        assert_eq!(pending.unwrap(), [10, 11]);
        assert!(was_stopping);
        assert!(worker_was_blocked);
    }

    #[test]
    fn stopping_during_command_drain_discards_remaining_commands_and_replies() {
        let data_dir =
            std::env::temp_dir().join(format!("orbment-clipboard-queue-{}", uuid::Uuid::new_v4()));
        let store = ClipboardStore::open(&data_dir, &ClipboardConfig::default()).unwrap();
        let (sender, receiver) = mpsc::channel();
        let (ocr_sender, _) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let mut state = ListenerState {
            store,
            config: ClipboardConfig::default(),
            receiver,
            stopping: stopping.clone(),
            ocr_sender,
            has_ocr: false,
            requests: Arc::new(Mutex::new(OcrRequests::default())),
            last_sequence: None,
            last_cleanup: Instant::now(),
            retry_count: 0,
        };
        let (observed, observations) = mpsc::channel();
        for index in 0..4 {
            let (reply, result) = mpsc::channel::<Result<()>>();
            let stopping = stopping.clone();
            let observed = observed.clone();
            sender
                .send(Command::Write {
                    content: WriteContent::Text("must not reach the clipboard".into()),
                    target: None,
                    seq: None,
                    cancelled: Arc::new(AtomicBool::new(index == 0)),
                    reply: CommandReply {
                        sender: Some(reply),
                        wake: Some(Box::new(move || {
                            stopping.store(true, Ordering::Release);
                            observed
                                .send(matches!(
                                    result.try_recv(),
                                    Err(mpsc::TryRecvError::Disconnected)
                                ))
                                .unwrap();
                        })),
                    },
                })
                .unwrap();
        }
        state.commands(0);
        assert_eq!(observations.try_iter().collect::<Vec<_>>(), [true; 4]);
        assert!(state.receiver.try_recv().is_err());
        drop(state);
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn completion_publishes_the_result_before_waking_the_ui() {
        let (sender, receiver) = mpsc::channel::<Result<()>>();
        let (observed, observation) = mpsc::channel();
        CommandReply {
            sender: Some(sender),
            wake: Some(Box::new(move || {
                observed
                    .send(receiver.try_recv().unwrap().is_err())
                    .unwrap();
            })),
        }
        .finish(Err(anyhow::anyhow!("write failed")));
        assert!(observation.try_recv().unwrap());
    }

    #[test]
    fn abandoned_commands_disconnect_before_waking_the_ui() {
        let (sender, receiver) = mpsc::channel::<Result<()>>();
        let (observed, observation) = mpsc::channel();
        drop(CommandReply {
            sender: Some(sender),
            wake: Some(Box::new(move || {
                observed
                    .send(matches!(
                        receiver.try_recv(),
                        Err(mpsc::TryRecvError::Disconnected)
                    ))
                    .unwrap();
            })),
        });
        assert!(observation.try_recv().unwrap());
    }
}
