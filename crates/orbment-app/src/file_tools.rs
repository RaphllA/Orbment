use anyhow::{Context, Result, anyhow, bail, ensure};
use orbment_analyzer::{
    disk, file_search, file_unlock, mft_engine::MftEngine, process::ProcessIdentity,
};
use orbment_core::{
    config::{DiskAnalyzerConfig, FileSearchConfig},
    db,
    i18n::tr,
    ipc::PendingRequest,
    lifecycle::{ActivateTrigger, CoreContext, OrbmentModule, ServiceState, UiSurfaceState},
};
use orbment_ui::{FileToolsAction, FileToolsState, SharedFileToolsState, wake};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HWND,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::{
            Shell::{
                COPYENGINE_E_USER_CANCELLED, FILEOPERATION_FLAGS, FOF_ALLOWUNDO,
                FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOFX_EARLYFAILURE,
                FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IFileOperationProgressSink,
                IFileOperationProgressSink_Impl, IShellItem, SEE_MASK_NOASYNC,
                SHCreateItemFromParsingName, SHELLEXECUTEINFOW, ShellExecuteExW,
                TSF_DELETE_RECYCLE_IF_POSSIBLE,
            },
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
    core::{BOOL, HRESULT, PCWSTR, Ref, implement, w},
};

#[derive(Default)]
struct Status {
    search_enabled: bool,
    disk_enabled: bool,
    search_open: bool,
    disk_open: bool,
    consumers: usize,
    effective_mode: String,
    loaded: bool,
    warnings: Vec<String>,
    error: Option<String>,
}

enum Work {
    Configure(FileSearchConfig, DiskAnalyzerConfig),
    Ui(FileToolsAction),
    Request(PendingRequest),
    Shutdown,
}

#[derive(Default)]
struct QueueState {
    accepting: bool,
    queued: usize,
    idle: bool,
}

pub fn is_request(action: &str) -> bool {
    matches!(
        action,
        "file_search.query"
            | "disk_analyzer.top_consumers"
            | "disk_analyzer.file_lock_query"
            | "disk_analyzer.file_unlock"
    )
}

pub struct FileTools {
    sender: Option<Sender<Work>>,
    thread: Option<JoinHandle<Result<()>>>,
    retiring: Vec<JoinHandle<Result<()>>>,
    queue: Arc<Mutex<QueueState>>,
    pub state: SharedFileToolsState,
    status: Arc<Mutex<Status>>,
    pending: Arc<AtomicUsize>,
    data_dir: PathBuf,
    search: FileSearchConfig,
    disk: DiskAnalyzerConfig,
    hwnd: usize,
}

impl FileTools {
    pub fn new(
        data_dir: PathBuf,
        search: FileSearchConfig,
        disk: DiskAnalyzerConfig,
        hwnd: HWND,
    ) -> Result<Self> {
        let status = Arc::new(Mutex::new(Status {
            search_enabled: search.enabled,
            disk_enabled: disk.enabled,
            effective_mode: if search.enabled && search.backend == "BuiltinMft" {
                search.residency_mode.clone()
            } else {
                "OnDemandOnly".into()
            },
            ..Status::default()
        }));
        let mut tools = Self {
            sender: None,
            thread: None,
            state: Arc::new(Mutex::new(FileToolsState::default())),
            retiring: Vec::new(),
            queue: Arc::new(Mutex::new(QueueState::default())),
            status,
            pending: Arc::new(AtomicUsize::new(0)),
            data_dir,
            search,
            disk,
            hwnd: hwnd.0 as usize,
        };
        if tools.resident() {
            tools.start()?;
        }
        Ok(tools)
    }

    fn resident(&self) -> bool {
        self.search.enabled
            && self.search.backend == "BuiltinMft"
            && self.search.residency_mode == "Resident"
    }

    fn start(&mut self) -> Result<()> {
        if self.sender.is_some() {
            return Ok(());
        }
        let (sender, receiver) = mpsc::channel();
        self.queue = Arc::new(Mutex::new(QueueState {
            accepting: true,
            ..QueueState::default()
        }));
        let data_dir = self.data_dir.clone();
        let mut worker = Worker {
            engine: MftEngine::new(data_dir.join("mft.snapshot"), self.search.clone()),
            search: self.search.clone(),
            disk: self.disk.clone(),
            state: self.state.clone(),
            status: self.status.clone(),
            pending: self.pending.clone(),
            queue: self.queue.clone(),
            hwnd: self.hwnd,
            search_open: false,
            disk_open: false,
            search_ref: false,
            disk_ref: false,
            analysis: None,
            lock_path: None,
            analysis_revision: None,
            analysis_config_dirty: false,
        };
        self.thread = Some(
            std::thread::Builder::new()
                .name("orbment-file-tools".into())
                .spawn(move || {
                    let result =
                        match unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok() {
                            Ok(()) => {
                                let _apartment = ComApartment;
                                worker.run(&receiver, &data_dir)
                            }
                            Err(error) => Err(error.into()),
                        };
                    if let Err(error) = &result {
                        let accepting = {
                            let mut queue = worker.queue.lock().unwrap();
                            let accepting = queue.accepting;
                            queue.accepting = false;
                            accepting
                        };
                        for work in receiver.try_iter() {
                            match work {
                                Work::Request(request) => request.reply(Err(anyhow!("{error:#}"))),
                                Work::Ui(_) => {
                                    worker.pending.fetch_sub(1, Ordering::AcqRel);
                                }
                                Work::Configure(_, _) | Work::Shutdown => {}
                            }
                        }
                        worker.queue.lock().unwrap().queued = 0;
                        if accepting {
                            worker.publish_status(Some(format!("{error:#}")));
                            worker.finish(Err(anyhow!("{error:#}")));
                        }
                    }
                    worker.wake();
                    result
                })?,
        );
        self.sender = Some(sender);
        Ok(())
    }

    pub fn modules(&self) -> [FileToolsModule; 2] {
        ["file_search", "disk_analyzer"].map(|id| FileToolsModule {
            id,
            status: self.status.clone(),
        })
    }

    pub fn configure(&mut self, search: FileSearchConfig, disk: DiskAnalyzerConfig) -> Result<()> {
        self.search = search;
        self.disk = disk;
        {
            let mut status = self.status.lock().unwrap();
            status.search_enabled = self.search.enabled;
            status.disk_enabled = self.disk.enabled;
        }
        if self.resident() {
            self.start()?;
        }
        if self.sender.is_some() {
            self.send(Work::Configure(self.search.clone(), self.disk.clone()))
                .map_err(|_| anyhow!(tr("file_tools.error.worker_stopped")))?;
        }
        Ok(())
    }

    pub fn action(&mut self, action: FileToolsAction) -> Result<()> {
        if self.sender.is_none()
            && matches!(
                action,
                FileToolsAction::DiskClosed | FileToolsAction::SearchClosed
            )
        {
            return Ok(());
        }
        self.enqueue(Work::Ui(action))
    }

    fn send(&self, work: Work) -> std::result::Result<(), Box<Work>> {
        let mut queue = self.queue.lock().unwrap();
        if !queue.accepting {
            return Err(Box::new(work));
        }
        self.sender
            .as_ref()
            .unwrap()
            .send(work)
            .map_err(|error| Box::new(error.0))?;
        queue.queued += 1;
        queue.idle = false;
        Ok(())
    }

    fn enqueue(&mut self, work: Work) -> Result<()> {
        self.start()?;
        self.pending.fetch_add(1, Ordering::AcqRel);
        {
            let mut state = self.state.lock().unwrap();
            state.busy = true;
            if matches!(&work, Work::Ui(FileToolsAction::Search { .. } | FileToolsAction::SearchOpened)) {
                state.search_query = None;
            }
            if !matches!(
                &work,
                Work::Ui(
                    FileToolsAction::SearchOpened
                        | FileToolsAction::SearchClosed
                        | FileToolsAction::DiskOpened
                        | FileToolsAction::DiskClosed
                )
            ) {
                state.error = None;
            }
            state.revision = state.revision.wrapping_add(1);
        }
        if self.send(work).is_err() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            let mut state = self.state.lock().unwrap();
            state.busy = self.pending.load(Ordering::Acquire) != 0;
            state.error = Some(tr("file_tools.error.worker_stopped"));
            state.revision = state.revision.wrapping_add(1);
            bail!(tr("file_tools.error.worker_stopped"));
        }
        Ok(())
    }

    pub fn request(&mut self, request: PendingRequest) {
        if let Err(error) = self.start() {
            request.reply(Err(error));
            return;
        }
        if let Err(error) = self.send(Work::Request(request))
            && let Work::Request(request) = *error
        {
            request.reply(Err(anyhow!(tr("file_tools.error.worker_stopped"))));
        }
    }

    pub fn diagnostics(&self) -> Value {
        let status = self.status.lock().unwrap();
        json!({"consumers": status.consumers, "effective_mode": status.effective_mode,
            "worker_started": self.thread.is_some(),
            "loaded": status.loaded, "warnings": status.warnings, "error": status.error})
    }

    pub fn maintenance(&mut self) -> Result<()> {
        let stop = {
            let mut queue = self.queue.lock().unwrap();
            if self.sender.is_some() && queue.queued == 0 && queue.idle {
                // Only the sender owner retires an idle generation, after all kinds of work.
                queue.accepting = false;
                true
            } else {
                false
            }
        };
        if stop {
            if let Some(sender) = self.sender.take() {
                let _ = sender.send(Work::Shutdown);
            }
            if let Some(thread) = self.thread.take() {
                self.retiring.push(thread);
            }
        } else if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            self.sender.take();
            self.retiring.push(self.thread.take().unwrap());
        }
        let mut result = Ok(());
        let mut index = 0;
        while index < self.retiring.len() {
            if self.retiring[index].is_finished() {
                let joined = self
                    .retiring
                    .swap_remove(index)
                    .join()
                    .map_err(|_| anyhow!(tr("file_tools.error.worker_stopped")))
                    .and_then(|result| result);
                if let Err(error) = joined {
                    result = Err(error);
                }
            } else {
                index += 1;
            }
        }
        result
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if let Some(sender) = self.sender.take() {
            self.queue.lock().unwrap().accepting = false;
            let _ = sender.send(Work::Shutdown);
        }
        if let Some(thread) = self.thread.take() {
            self.retiring.push(thread);
        }
        let mut result = Ok(());
        for thread in self.retiring.drain(..) {
            if let Err(error) = thread
                .join()
                .map_err(|_| anyhow!(tr("file_tools.error.worker_stopped")))
                .and_then(|result| result)
            {
                result = Err(error);
            }
        }
        result
    }
}

impl Drop for FileTools {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::warn!("file tools shutdown: {error:#}");
        }
    }
}

pub struct FileToolsModule {
    id: &'static str,
    status: Arc<Mutex<Status>>,
}

impl OrbmentModule for FileToolsModule {
    fn id(&self) -> &'static str {
        self.id
    }
    fn service_state(&self) -> ServiceState {
        let status = self.status.lock().unwrap();
        let enabled = if self.id == "file_search" {
            status.search_enabled
        } else {
            status.disk_enabled
        };
        if !enabled {
            ServiceState::Disabled
        } else if status.error.is_some() {
            ServiceState::Unavailable {
                reason: "file_tools.error.unavailable",
            }
        } else {
            ServiceState::Running
        }
    }
    fn ui_state(&self) -> UiSurfaceState {
        let status = self.status.lock().unwrap();
        if if self.id == "file_search" {
            status.search_open
        } else {
            status.disk_open
        } {
            UiSurfaceState::Visible
        } else {
            UiSurfaceState::Unloaded
        }
    }
    fn start_service(&mut self, _: &CoreContext) -> Result<()> {
        Ok(())
    }
    fn open_ui(&mut self, _: ActivateTrigger, _: &CoreContext) -> Result<()> {
        Ok(())
    }
    fn close_ui(&mut self, _: &CoreContext) -> Result<()> {
        Ok(())
    }
    fn reclaim_idle_ui_and_cache(&mut self, _: &CoreContext) -> Result<()> {
        Ok(())
    }
    fn stop_service(&mut self, _: &CoreContext) -> Result<()> {
        // The shared worker is stopped once by App after all module UIs close.
        Ok(())
    }
}

struct Worker {
    engine: MftEngine,
    search: FileSearchConfig,
    disk: DiskAnalyzerConfig,
    state: SharedFileToolsState,
    status: Arc<Mutex<Status>>,
    pending: Arc<AtomicUsize>,
    queue: Arc<Mutex<QueueState>>,
    hwnd: usize,
    search_open: bool,
    disk_open: bool,
    search_ref: bool,
    disk_ref: bool,
    analysis: Option<(PathBuf, String)>,
    analysis_revision: Option<u64>,
    analysis_config_dirty: bool,
    lock_path: Option<PathBuf>,
}

impl Worker {
    fn wake(&self) {
        wake(HWND(self.hwnd as *mut std::ffi::c_void));
    }

    fn publish_status(&self, error: Option<String>) {
        let mut status = self.status.lock().unwrap();
        status.search_enabled = self.search.enabled;
        status.disk_enabled = self.disk.enabled;
        status.search_open = self.search_open;
        status.disk_open = self.disk_open;
        status.consumers = self.engine.consumers();
        status.effective_mode = self.engine.effective_mode().into();
        status.loaded = self.engine.tree().is_some();
        status.warnings = self.engine.warnings().to_vec();
        status.error = error;
    }

    fn finish(&self, result: Result<()>) {
        // UI commands own one pending count; periodic refresh and IPC do not.
        let mut state = self.state.lock().unwrap();
        let busy = self.pending.load(Ordering::Acquire) != 0;
        if let Err(error) = result {
            let message = format!("{}: {error:#}", tr("file_tools.error.operation_failed"));
            if state.busy == busy && state.error.as_ref() == Some(&message) {
                return;
            }
            state.error = Some(message);
        }
        state.busy = busy;
        state.revision = state.revision.wrapping_add(1);
        drop(state);
        self.wake();
    }

    fn run(&mut self, receiver: &Receiver<Work>, data_dir: &Path) -> Result<()> {
        let database = db::open(data_dir)?;
        let mut last_tick = Instant::now() - Duration::from_secs(1);
        let mut last_refresh = Instant::now();
        loop {
            if self
                .engine
                .next_tick_in()
                .is_some_and(|delay| delay.is_zero())
                && last_tick.elapsed() >= Duration::from_secs(1)
            {
                let result = self.engine.tick().map_err(anyhow::Error::msg);
                self.publish_status(result.as_ref().err().map(|error| format!("{error:#}")));
                if let Err(error) = result {
                    self.finish(Err(error));
                }
                last_tick = Instant::now();
                if last_refresh.elapsed() >= Duration::from_secs(2)
                    && let Some(result) = self.refresh_analysis()
                {
                    self.finish(result);
                    last_refresh = Instant::now();
                }
            }
            let timeout = self
                .engine
                .next_tick_in()
                .map(|delay| delay.max(Duration::from_secs(1).saturating_sub(last_tick.elapsed())));
            {
                let mut queue = self.queue.lock().unwrap();
                queue.idle = timeout.is_none() && !self.search_open && !self.disk_open;
                if queue.idle {
                    self.wake();
                }
            }
            let received = match timeout {
                Some(timeout) => receiver.recv_timeout(timeout),
                None => receiver
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            };
            let work = match received {
                Ok(work) => work,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            match work {
                Work::Shutdown => break,
                Work::Configure(search, disk) => {
                    // Presentation-only settings are consumed by the UI without rebuilding the report.
                    self.analysis_config_dirty |= self.disk.enabled != disk.enabled
                        || self.disk.show_allocated_size != disk.show_allocated_size
                        || self.disk.show_free_space != disk.show_free_space
                        || self.disk.max_top_files != disk.max_top_files;
                    self.search = search;
                    self.disk = disk;
                    self.search_open &= self.search.enabled;
                    self.disk_open &= self.disk.enabled;
                    self.engine.configure(self.search.clone());
                    let result = self.sync_refs();
                    self.publish_status(result.as_ref().err().map(|error| format!("{error:#}")));
                    self.finish(result);
                }
                Work::Request(request) => {
                    if !request.is_cancelled() {
                        let result = self.execute(
                            &database,
                            &request.request.action,
                            request.request.params.clone(),
                        );
                        request.reply(result);
                    }
                    self.publish_status(None);
                }
                Work::Ui(action) => {
                    let result = self.ui(&database, action);
                    self.pending.fetch_sub(1, Ordering::AcqRel);
                    self.publish_status(None);
                    self.finish(result);
                }
            }
            {
                let mut queue = self.queue.lock().unwrap();
                queue.queued -= 1;
                queue.idle =
                    self.engine.next_tick_in().is_none() && !self.search_open && !self.disk_open;
                if queue.idle {
                    self.wake();
                }
            }
        }
        if self.search_ref {
            self.engine.release().map_err(anyhow::Error::msg)?;
        }
        if self.disk_ref {
            self.engine.release().map_err(anyhow::Error::msg)?;
        }
        self.engine.shutdown().map_err(anyhow::Error::msg)?;
        db::checkpoint(&database)?;
        Ok(())
    }

    fn refresh_analysis(&mut self) -> Option<Result<()>> {
        if !self.disk_open || !self.disk.enabled || !self.analysis_config_dirty {
            return None;
        }
        let (target, filter) = self.analysis.clone()?;
        Some(self.analyze(&target, &filter, false))
    }

    fn sync_refs(&mut self) -> Result<()> {
        let search = self.search_open && self.search.enabled && self.search.backend == "BuiltinMft";
        let disk = self.disk_open && self.disk.enabled;
        for (wanted, held) in [(search, &mut self.search_ref), (disk, &mut self.disk_ref)] {
            if wanted && !*held {
                self.engine.acquire().map_err(anyhow::Error::msg)?;
                *held = true;
            } else if !wanted && *held {
                self.engine.release().map_err(anyhow::Error::msg)?;
                *held = false;
            }
        }
        if search {
            self.engine.ensure_search().map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    fn with_tree<T>(
        &mut self,
        target: Option<&Path>,
        refresh: bool,
        operation: impl FnOnce(&orbment_analyzer::mft_engine::MftTree) -> Result<T>,
    ) -> Result<T> {
        self.engine.acquire().map_err(anyhow::Error::msg)?;
        let result = (|| {
            if let Some(target) = target {
                if refresh {
                    self.engine.refresh_target(target)
                } else {
                    self.engine.ensure_target(target)
                }
                .map_err(anyhow::Error::msg)?;
            } else {
                self.engine.ensure_search().map_err(anyhow::Error::msg)?;
            }
            operation(
                self.engine
                    .tree()
                    .context(tr("file_tools.error.unavailable"))?,
            )
        })();
        let release = self.engine.release().map_err(anyhow::Error::msg);
        match result {
            Ok(value) => {
                release?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }

    fn search(
        &mut self,
        database: &rusqlite::Connection,
        query: &str,
        limit: usize,
    ) -> Result<Vec<file_search::SearchResult>> {
        ensure!(self.search.enabled, "{}", tr("file_tools.error.disabled"));
        if self.search.backend == "EverythingBridge" {
            return file_search::query_everything(&self.search, query, limit)
                .map_err(anyhow::Error::msg);
        }
        let mut statement =
            database.prepare("SELECT full_path, run_count FROM file_run_history")?;
        let history = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        let config = self.search.clone();
        self.with_tree(None, false, |tree| {
            file_search::query_builtin(tree, &config, query, limit, &history)
                .map_err(anyhow::Error::msg)
        })
    }

    fn analyze(&mut self, target: &Path, filter: &str, refresh: bool) -> Result<()> {
        ensure!(self.disk.enabled, "{}", tr("file_tools.error.disabled"));
        let config = self.disk.clone();
        let report = self.with_tree(Some(target), refresh, |tree| {
            disk::analyze(tree, target, &config, filter).map_err(anyhow::Error::msg)
        })?;
        self.analysis_revision = Some(self.engine.revision());
        self.analysis_config_dirty = false;
        let mut state = self.state.lock().unwrap();
        state.disk = Some(report.into());
        state.disk_revision = state.disk_revision.wrapping_add(1);
        Ok(())
    }

    fn execute(
        &mut self,
        database: &rusqlite::Connection,
        action: &str,
        params: Value,
    ) -> Result<Value> {
        match action {
            "file_search.query" => {
                let args: SearchParams = crate::parameters(params)?;
                Ok(serde_json::to_value(self.search(
                    database,
                    &args.query,
                    args.limit.unwrap_or(100),
                )?)?)
            }
            "disk_analyzer.top_consumers" => {
                ensure!(self.disk.enabled, "{}", tr("file_tools.error.disabled"));
                let args: TopParams = crate::parameters(params)?;
                let target = checked_path(&args.path)?;
                let limit = args.limit.unwrap_or(self.disk.max_top_files as usize);
                let allocated = args.allocated.unwrap_or(self.disk.show_allocated_size);
                let entries = self.with_tree(Some(&target), false, |tree| {
                    disk::top_consumers(tree, &target, limit, allocated).map_err(anyhow::Error::msg)
                })?;
                Ok(serde_json::to_value(entries)?)
            }
            "disk_analyzer.file_lock_query" => {
                ensure!(self.disk.enabled, "{}", tr("file_tools.error.disabled"));
                let args: PathParams = crate::parameters(params)?;
                Ok(serde_json::to_value(
                    file_unlock::query(&checked_path(&args.path)?).map_err(anyhow::Error::msg)?,
                )?)
            }
            "disk_analyzer.file_unlock" => {
                ensure!(self.disk.enabled, "{}", tr("file_tools.error.disabled"));
                let args: UnlockParams = crate::parameters(params)?;
                file_unlock::unlock(
                    ProcessIdentity {
                        pid: args.process_id,
                        creation_time: args.creation_time,
                    },
                    &checked_path(&args.path)?,
                    args.handle,
                )
                .map_err(anyhow::Error::msg)?;
                Ok(json!({"process_id": args.process_id, "handle": args.handle, "unlocked": true}))
            }
            _ => bail!("{action}: {}", tr("file_tools.error.unavailable")),
        }
    }

    fn ui(&mut self, database: &rusqlite::Connection, action: FileToolsAction) -> Result<()> {
        match action {
            FileToolsAction::SearchOpened => {
                self.search_open = true;
                self.sync_refs()?;
                self.state.lock().unwrap().search_query = None;
                let results = self.search(database, "", 100)?;
                let mut state = self.state.lock().unwrap();
                state.search_results = results;
                state.search_query = Some(String::new());
                state.error = None;
            }
            FileToolsAction::SearchClosed => {
                self.search_open = false;
                self.sync_refs()?;
            }
            FileToolsAction::DiskOpened => {
                self.disk_open = true;
                self.sync_refs()?;
                if self.disk.enabled
                    && self.analysis_revision.is_none()
                    && let Some((target, filter)) = self.analysis.clone()
                {
                    self.analyze(&target, &filter, false)?;
                }
            }
            FileToolsAction::DiskClosed => {
                self.disk_open = false;
                {
                    let mut state = self.state.lock().unwrap();
                    state.disk = None;
                    state.duplicates = Vec::new();
                    state.disk_revision = state.disk_revision.wrapping_add(1);
                }
                self.analysis_revision = None;
                self.sync_refs()?;
            }
            FileToolsAction::Search { query } => {
                self.state.lock().unwrap().search_query = None;
                let results = self.search(database, &query, 100)?;
                let mut state = self.state.lock().unwrap();
                state.search_results = results;
                state.search_query = Some(query);
                state.error = None;
            }
            FileToolsAction::Analyze { target, filter } => {
                let target = checked_path(&target)?;
                self.analyze(&target, &filter, true)?;
                self.analysis = Some((target, filter));
                self.state.lock().unwrap().duplicates.clear();
            }
            FileToolsAction::Duplicates => {
                ensure!(self.disk.enabled, "{}", tr("file_tools.error.disabled"));
                let target = self
                    .analysis
                    .as_ref()
                    .context(tr("file_tools.error.no_analysis"))?
                    .0
                    .clone();
                let entries = self.with_tree(Some(&target), false, |tree| {
                    disk::files_in_subtree(tree, &target).map_err(anyhow::Error::msg)
                })?;
                let groups = disk::find_duplicates(&entries).map_err(anyhow::Error::msg)?;
                self.state.lock().unwrap().duplicates = groups;
            }
            FileToolsAction::QueryLocks { path } => {
                ensure!(self.disk.enabled, "{}", tr("file_tools.error.disabled"));
                let path = checked_path(&path)?;
                let locks = file_unlock::query(&path).map_err(anyhow::Error::msg)?;
                self.lock_path = Some(path);
                self.state.lock().unwrap().locks = locks;
            }
            FileToolsAction::TerminateLock { lock } => {
                self.unlock(&lock, None)?;
            }
            FileToolsAction::CloseHandle { lock } => {
                let handle = lock.handle.context(tr("file_tools.error.unavailable"))?;
                self.unlock(&lock, Some(handle))?;
            }
            FileToolsAction::Open { path } => {
                let path = checked_path(&path)?;
                shell_open(&path)?;
                database.execute("INSERT INTO file_run_history(full_path,run_count,last_run_date) VALUES(?1,1,strftime('%Y-%m-%dT%H:%M:%fZ','now')) ON CONFLICT(full_path) DO UPDATE SET run_count=run_count+1,last_run_date=excluded.last_run_date", [path.to_string_lossy().as_ref()])?;
            }
            FileToolsAction::Explorer { path } => {
                let path = checked_path(&path)?;
                std::process::Command::new("explorer.exe")
                    .arg(format!("/select,{}", path.display()))
                    .spawn()?;
            }
            FileToolsAction::Terminal { path } => {
                let path = checked_path(&path)?;
                let directory = if path.is_dir() {
                    path.as_path()
                } else {
                    path.parent().context(tr("file_tools.error.invalid_path"))?
                };
                std::process::Command::new("wt.exe")
                    .arg("-d")
                    .arg(directory)
                    .spawn()?;
            }
            FileToolsAction::Recycle { path } => {
                recycle(&checked_path(&path)?)?;
            }
            FileToolsAction::Ports { .. } => bail!(tr("file_tools.error.unavailable")),
        }
        Ok(())
    }

    fn unlock(&mut self, lock: &file_unlock::FileLock, handle: Option<u64>) -> Result<()> {
        ensure!(self.disk.enabled, "{}", tr("file_tools.error.disabled"));
        file_unlock::unlock(
            ProcessIdentity {
                pid: lock.process_id,
                creation_time: lock.creation_time,
            },
            &checked_path(&lock.path)?,
            handle,
        )
        .map_err(anyhow::Error::msg)?;
        if let Some(path) = &self.lock_path {
            let locks = file_unlock::query(path).map_err(anyhow::Error::msg)?;
            self.state.lock().unwrap().locks = locks;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    query: String,
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TopParams {
    path: String,
    limit: Option<usize>,
    allocated: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathParams {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnlockParams {
    process_id: u32,
    #[serde(with = "orbment_analyzer::process::filetime")]
    creation_time: u64,
    path: String,
    handle: Option<u64>,
}

fn checked_path(path: &str) -> Result<PathBuf> {
    ensure!(
        !path.is_empty() && !path.contains('\0'),
        "{}",
        tr("file_tools.error.invalid_path")
    );
    std::path::absolute(path).context(tr("file_tools.error.invalid_path"))
}

fn shell_open(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut execute = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("open"),
        lpFile: PCWSTR(path.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut execute) }.context(tr("file_tools.error.operation_failed"))
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn recycle_flags() -> FILEOPERATION_FLAGS {
    // NOERRORUI alone ignores errors and continues; EARLYFAILURE stops the operation.
    FOF_ALLOWUNDO
        | FOFX_RECYCLEONDELETE
        | FOF_NOCONFIRMATION
        | FOF_NOERRORUI
        | FOF_SILENT
        | FOFX_EARLYFAILURE
}

#[derive(Default)]
struct RecycleProgress {
    refused: AtomicBool,
    failure: AtomicI32,
}

impl RecycleProgress {
    fn record_result(&self, result: HRESULT) -> windows::core::Result<()> {
        if result.is_err() {
            let _ = self
                .failure
                .compare_exchange(0, result.0, Ordering::AcqRel, Ordering::Acquire);
        }
        result.ok()
    }

    fn complete(
        &self,
        performed: windows::core::Result<()>,
        aborted: windows::core::Result<BOOL>,
    ) -> Result<()> {
        ensure!(
            !self.refused.load(Ordering::Acquire),
            "{}",
            tr("file_tools.error.recycle_unavailable")
        );
        performed.context(tr("file_tools.error.operation_failed"))?;
        HRESULT(self.failure.load(Ordering::Acquire))
            .ok()
            .context(tr("file_tools.error.operation_failed"))?;
        let aborted = aborted.context(tr("file_tools.error.operation_failed"))?;
        ensure!(
            !aborted.as_bool(),
            "{}",
            tr("file_tools.error.operation_failed")
        );
        Ok(())
    }
}

#[implement(IFileOperationProgressSink)]
struct RecycleSink {
    progress: Arc<RecycleProgress>,
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for RecycleSink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn FinishOperations(&self, result: HRESULT) -> windows::core::Result<()> {
        self.progress.record_result(result)
    }

    fn PreDeleteItem(&self, flags: u32, _: Ref<'_, IShellItem>) -> windows::core::Result<()> {
        // This flag alone is not a guarantee: RECYCLEONDELETE and EARLYFAILURE are
        // also required. Never approve a callback requesting permanent deletion.
        if self.progress.refused.load(Ordering::Acquire)
            || flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0
        {
            self.progress.refused.store(true, Ordering::Release);
            return Err(windows::core::Error::from_hresult(
                COPYENGINE_E_USER_CANCELLED,
            ));
        }
        Ok(())
    }

    fn PostDeleteItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        result: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        self.progress.record_result(result)
    }

    fn PreRenameItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PostRenameItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PreMoveItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PostMoveItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PreCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PostCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PreNewItem(&self, _: u32, _: Ref<'_, IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn PostNewItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

fn recycle(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // The worker owns the STA; these interfaces are released before its COM guard.
    let operation: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) }
            .context(tr("file_tools.error.operation_failed"))?;
    unsafe { operation.SetOperationFlags(recycle_flags()) }
        .context(tr("file_tools.error.operation_failed"))?;
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None) }
        .context(tr("file_tools.error.operation_failed"))?;
    let progress = Arc::new(RecycleProgress::default());
    let sink: IFileOperationProgressSink = RecycleSink {
        progress: progress.clone(),
    }
    .into();
    unsafe { operation.DeleteItem(&item, &sink) }
        .context(tr("file_tools.error.operation_failed"))?;
    let performed = unsafe { operation.PerformOperations() };
    // S_OK can still mean an operation was aborted; query even when Perform failed.
    let aborted = unsafe { operation.GetAnyOperationsAborted() };
    progress.complete(performed, aborted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbment_core::ipc::ActionRequest;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn journal_revision_changes_do_not_rebuild_the_open_analysis() {
        let search = FileSearchConfig {
            enabled: false,
            ..Default::default()
        };
        let state = Arc::new(Mutex::new(FileToolsState {
            disk_revision: 42,
            ..Default::default()
        }));
        let mut worker = Worker {
            engine: MftEngine::new(PathBuf::new(), search.clone()),
            search,
            disk: DiskAnalyzerConfig {
                enabled: true,
                monitor_fs_changes: true,
                ..Default::default()
            },
            state: state.clone(),
            status: Default::default(),
            pending: Default::default(),
            queue: Default::default(),
            hwnd: 0,
            search_open: false,
            disk_open: true,
            search_ref: false,
            disk_ref: false,
            analysis: Some((PathBuf::from(r"C:\missing-analysis-target"), String::new())),
            analysis_revision: Some(u64::MAX),
            analysis_config_dirty: false,
            lock_path: None,
        };
        assert_ne!(worker.analysis_revision, Some(worker.engine.revision()));
        for _ in 0..3 {
            assert!(worker.refresh_analysis().is_none());
        }
        assert_eq!(state.lock().unwrap().disk_revision, 42);
    }

    #[test]
    fn recycle_policy_requires_recycling_and_stops_on_error() {
        use windows::Win32::UI::Shell::FOF_WANTNUKEWARNING;

        let flags = recycle_flags().0;
        for required in [
            FOF_ALLOWUNDO,
            FOFX_RECYCLEONDELETE,
            FOF_NOCONFIRMATION,
            FOF_NOERRORUI,
            FOF_SILENT,
            FOFX_EARLYFAILURE,
        ] {
            assert_eq!(flags & required.0, required.0);
        }
        assert_eq!(flags & FOF_WANTNUKEWARNING.0, 0);
    }

    #[test]
    fn recycle_sink_accepts_recycle_requests_with_other_transfer_flags() {
        let recycle = TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32;
        for flags in [recycle, u32::MAX] {
            let progress = Arc::new(RecycleProgress::default());
            let sink: IFileOperationProgressSink = RecycleSink {
                progress: progress.clone(),
            }
            .into();
            // Invoke only our COM callback; no Shell operation or filesystem access.
            unsafe { sink.PreDeleteItem(flags, None) }.unwrap();
            assert!(!progress.refused.load(Ordering::Acquire));
        }
    }

    #[test]
    fn recycle_sink_refuses_permanent_delete_and_latches_cancellation() {
        let recycle = TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32;
        for flags in [0, !recycle] {
            let progress = Arc::new(RecycleProgress::default());
            let sink: IFileOperationProgressSink = RecycleSink {
                progress: progress.clone(),
            }
            .into();
            let error = unsafe { sink.PreDeleteItem(flags, None) }.unwrap_err();
            assert_eq!(error.code(), COPYENGINE_E_USER_CANCELLED);
            let repeated = unsafe { sink.PreDeleteItem(recycle, None) }.unwrap_err();
            assert_eq!(repeated.code(), COPYENGINE_E_USER_CANCELLED);
            for performed in [Ok(()), Err(error)] {
                let error = progress.complete(performed, Ok(BOOL(1))).unwrap_err();
                assert_eq!(
                    error.to_string(),
                    tr("file_tools.error.recycle_unavailable")
                );
            }
        }
    }

    #[test]
    fn recycle_completion_requires_successful_execution_and_abort_query() {
        use windows::Win32::Foundation::E_FAIL;

        let progress = RecycleProgress::default();
        assert!(progress.complete(Ok(()), Ok(BOOL(0))).is_ok());
        assert!(progress.complete(Ok(()), Ok(BOOL(1))).is_err());
        assert!(progress.complete(Err(E_FAIL.into()), Ok(BOOL(0))).is_err());
        assert!(progress.complete(Ok(()), Err(E_FAIL.into())).is_err());
    }

    #[test]
    fn recycle_completion_preserves_item_failure_even_if_shell_reports_success() {
        use windows::Win32::Foundation::E_ACCESSDENIED;

        let progress = Arc::new(RecycleProgress::default());
        let sink: IFileOperationProgressSink = RecycleSink {
            progress: progress.clone(),
        }
        .into();
        let result = unsafe { sink.PostDeleteItem(0, None, E_ACCESSDENIED, None) };
        assert_eq!(result.unwrap_err().code(), E_ACCESSDENIED);
        unsafe { sink.PostDeleteItem(0, None, HRESULT(0), None) }.unwrap();
        let error = progress.complete(Ok(()), Ok(BOOL(0))).unwrap_err();
        assert_eq!(
            error.downcast_ref::<windows::core::Error>().unwrap().code(),
            E_ACCESSDENIED
        );
    }

    #[test]
    fn unlock_requires_exact_process_identity_and_target() {
        let params = json!({
            "process_id": 42,
            "creation_time": "134000000000000001",
            "path": "C:\\locked.txt",
            "handle": 128,
        });
        let parsed: UnlockParams = serde_json::from_value(params.clone()).unwrap();
        assert_eq!(parsed.creation_time, 134_000_000_000_000_001);
        for missing in ["creation_time", "path"] {
            let mut incomplete = params.clone();
            incomplete.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<UnlockParams>(incomplete).is_err());
        }
        let mut rounded = params;
        rounded["creation_time"] = json!(134_000_000_000_000_001_u64);
        assert!(serde_json::from_value::<UnlockParams>(rounded).is_err());
    }

    #[test]
    fn idle_retirement_preserves_queued_configuration_and_request_reply() {
        let search = FileSearchConfig {
            enabled: false,
            ..Default::default()
        };
        let disk = DiskAnalyzerConfig {
            enabled: false,
            ..Default::default()
        };
        let mut tools = FileTools::new(
            PathBuf::new(),
            search.clone(),
            disk.clone(),
            HWND::default(),
        )
        .unwrap();
        let (sender, receiver) = mpsc::channel();
        tools.sender = Some(sender);
        tools.queue.lock().unwrap().accepting = true;
        let (reply, result) = mpsc::channel();
        let request = PendingRequest::new(
            ActionRequest::new("file_search.query", json!({"query": "needle"})),
            Arc::new(AtomicBool::new(false)),
            move |value| {
                let _ = reply.send(value);
            },
        );
        assert!(tools.send(Work::Configure(search, disk)).is_ok());
        assert!(tools.send(Work::Request(request)).is_ok());
        // The UI has no pending commands, but the shared queue is not empty.
        tools.queue.lock().unwrap().idle = true;
        tools.maintenance().unwrap();
        assert!(tools.sender.is_some());
        assert!(matches!(receiver.recv().unwrap(), Work::Configure(_, _)));
        let Work::Request(request) = receiver.recv().unwrap() else {
            panic!("queued request was replaced by shutdown");
        };
        request.reply(Ok(json!(["needle"])));
        assert_eq!(result.recv().unwrap().unwrap(), json!(["needle"]));
        tools.queue.lock().unwrap().queued = 0;
        tools.maintenance().unwrap();
        assert!(tools.sender.is_none());
        assert!(matches!(receiver.recv().unwrap(), Work::Shutdown));
    }

    #[test]
    fn initialization_failure_replies_to_every_submitted_request() {
        let path = std::env::temp_dir().join(format!(
            "orbment-file-tools-init-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"not a directory").unwrap();
        let mut tools = FileTools::new(
            path.clone(),
            FileSearchConfig {
                enabled: false,
                ..Default::default()
            },
            DiskAnalyzerConfig {
                enabled: false,
                ..Default::default()
            },
            HWND::default(),
        )
        .unwrap();
        let (reply, results) = mpsc::channel();
        for _ in 0..16 {
            let reply = reply.clone();
            tools.request(PendingRequest::new(
                ActionRequest::new("file_search.query", Value::Null),
                Arc::new(AtomicBool::new(false)),
                move |result| {
                    let _ = reply.send(result);
                },
            ));
        }
        drop(reply);
        for _ in 0..16 {
            assert!(
                results
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .is_err()
            );
        }
        assert!(tools.shutdown().is_err());
        std::fs::remove_file(path).unwrap();
    }
}
