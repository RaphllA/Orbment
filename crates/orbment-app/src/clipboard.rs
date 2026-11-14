use anyhow::{Context, Result, bail, ensure};
use orbment_clipboard::{
    ClipboardQuery, ClipboardService, DeleteOptions, WriteContent, WriteOptions,
};
use orbment_core::{
    i18n::tr,
    ipc::PendingRequest,
    lifecycle::{ActivateTrigger, CoreContext, OrbmentModule, ServiceState, UiSurfaceState},
    ocr::OcrDecision,
};
use orbment_ui::{
    ClipboardGroup, ClipboardPopupEvent, ClipboardRow, D2dContext, PopupQuery, TransientPopupWindow,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::Instant,
};
use windows::{
    Win32::{
        Foundation::HWND,
        UI::{
            Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
    core::{PCWSTR, w},
};

pub struct ClipboardModule {
    service: Option<ClipboardService>,
    popup: TransientPopupWindow,
    state: ServiceState,
    hwnd: usize,
    changed: Arc<AtomicBool>,
    applied_config: Option<Value>,
    query: ClipboardQuery,
    generation: u64,
    ocr_requests: Option<Receiver<i64>>,
    pending_ocr_requests: BTreeSet<i64>,
    ocr_decision: Option<OcrDecision>,
    ocr_prompt_pending: bool,
    operations: Vec<PendingOperation>,
    device_lost: bool,
}

struct PendingOperation {
    result: Receiver<Result<()>>,
    request: Option<PendingRequest>,
    value: Value,
    hide_on_success: bool,
    sound_on_success: bool,
}

impl ClipboardModule {
    pub fn new(graphics: Rc<D2dContext>, hwnd: HWND, started: Instant) -> Self {
        Self {
            service: None,
            popup: TransientPopupWindow::new(graphics, hwnd, started),
            state: ServiceState::Disabled,
            hwnd: hwnd.0 as usize,
            changed: Arc::new(AtomicBool::new(false)),
            applied_config: None,
            query: ClipboardQuery::default(),
            generation: 0,
            ocr_requests: None,
            pending_ocr_requests: BTreeSet::new(),
            ocr_decision: None,
            ocr_prompt_pending: false,
            operations: Vec::new(),
            device_lost: false,
        }
    }

    fn service(&self) -> Result<&ClipboardService> {
        self.service.as_ref().context(tr("clipboard.unavailable"))
    }

    pub fn apply_config(&mut self, ctx: &CoreContext) -> Result<()> {
        if !ctx.config.clipboard.enabled {
            return self.stop_service(ctx);
        }
        let decision = ctx.ocr.decision(&ctx.config.app.ocr_engine_tier)?;
        let snapshot = json!({
            "clipboard": ctx.config.clipboard,
            "ocr_engine_tier": ctx.config.app.ocr_engine_tier,
            "offline_available": ctx.ocr.decision("OfflineOnnx")? == OcrDecision::OfflineOnnx,
            "vision_ocr": ctx.config.vision.ocr,
            "ocr_idle_unload_seconds": ctx.config.app.ocr_idle_unload_seconds,
        });
        if self.applied_config.as_ref() == Some(&snapshot) && self.service.is_some() {
            return Ok(());
        }
        let previous_decision = self.ocr_decision;
        let previous_pending = self.pending_ocr_requests.len();
        let engine = match decision {
            OcrDecision::ChooseEngine | OcrDecision::OfflineUnavailable => None,
            _ => Some(ctx.ocr.resolve(&ctx.config.app.ocr_engine_tier)?),
        };
        self.state = ServiceState::Unavailable {
            reason: "clipboard_start_failed",
        };
        if let Some(service) = self.service.take() {
            self.pending_ocr_requests.extend(service.stop());
        }
        self.collect_ocr_requests();
        self.ocr_requests.take();
        self.applied_config = None;
        self.ocr_decision = None;
        let service =
            ClipboardService::start_with_ocr(&ctx.data_dir, ctx.config.clipboard.clone(), engine)?;
        let changed = self.changed.clone();
        let hwnd = self.hwnd;
        service.set_on_change(Arc::new(move || {
            changed.store(true, Ordering::Release);
            orbment_ui::wake(HWND(hwnd as *mut _));
        }));
        let (sender, receiver) = mpsc::channel();
        service.set_on_ocr_requested(Arc::new(move |seq| {
            if sender.send(seq).is_ok() {
                orbment_ui::wake(HWND(hwnd as *mut _));
            }
        }));
        self.service = Some(service);
        self.ocr_requests = Some(receiver);
        self.ocr_decision = Some(decision);
        if decision == OcrDecision::ChooseEngine
            && (previous_decision != Some(decision)
                || previous_pending != self.pending_ocr_requests.len())
        {
            self.ocr_prompt_pending = !self.pending_ocr_requests.is_empty();
        }
        self.applied_config = Some(snapshot);
        self.changed.store(true, Ordering::Release);
        self.state = ServiceState::Running;
        self.popup.sync_config(&ctx.config.clipboard);
        if self.popup.ui_state() == UiSurfaceState::Visible {
            self.popup.refresh();
        }
        Ok(())
    }

    fn collect_ocr_requests(&mut self) -> bool {
        let previous = self.pending_ocr_requests.len();
        if let Some(requests) = &self.ocr_requests {
            self.pending_ocr_requests.extend(requests.try_iter());
        }
        let changed = self.pending_ocr_requests.len() != previous;
        if changed && self.ocr_decision == Some(OcrDecision::ChooseEngine) {
            self.ocr_prompt_pending = true;
        }
        changed
    }

    pub fn needs_ocr_choice(&mut self) -> bool {
        self.ocr_decision == Some(OcrDecision::ChooseEngine)
            && !self.pending_ocr_requests.is_empty()
            && std::mem::take(&mut self.ocr_prompt_pending)
    }

    pub fn resume_ocr_requests(&mut self) -> Result<()> {
        self.collect_ocr_requests();
        if !matches!(
            self.ocr_decision,
            Some(OcrDecision::WindowsNative | OcrDecision::OfflineOnnx)
        ) {
            return Ok(());
        }
        let service = self.service()?;
        let store = service.store();
        let mut completed = Vec::new();
        let mut failure = None;
        for &seq in &self.pending_ocr_requests {
            let result = (|| -> Result<()> {
                if store.get(seq)?.is_some_and(|item| item.kind == "image") {
                    service.recognize_item(seq)?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => completed.push(seq),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        for seq in completed {
            self.pending_ocr_requests.remove(&seq);
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn take_device_lost(&mut self) -> bool {
        std::mem::take(&mut self.device_lost)
    }

    pub fn rebuild_graphics(&mut self, graphics: Rc<D2dContext>, _: &CoreContext) -> Result<()> {
        self.popup.rebuild_graphics(graphics)
    }

    pub fn command(&mut self, action: &str, params: Value, ctx: &CoreContext) -> Result<Value> {
        ensure!(ctx.config.clipboard.enabled, "{}", tr("clipboard.disabled"));
        self.service()?;
        match action {
            "clipboard.toggle" | "clipboard.open" => {
                let _: EmptyParams = parameters(params)?;
                if action == "clipboard.toggle" && self.popup.ui_state() == UiSurfaceState::Visible
                {
                    self.close_ui(ctx)?;
                } else {
                    self.open_ui(ActivateTrigger::Action, ctx)?;
                }
                Ok(json!({"service_state": self.service_state(), "ui_state": self.ui_state()}))
            }
            _ => bail!(tr("clipboard.invalid_action")),
        }
    }

    pub fn request(&mut self, pending: PendingRequest, ctx: &CoreContext) {
        if pending.is_cancelled() {
            return;
        }
        let action = pending.request.action.clone();
        let params = pending.request.params.clone();
        if matches!(action.as_str(), "clipboard.open" | "clipboard.toggle") {
            pending.reply(self.command(&action, params, ctx));
            return;
        }
        match self.submit_command(&action, params, ctx) {
            Ok((result, value)) => self.operations.push(PendingOperation {
                result,
                request: Some(pending),
                value,
                hide_on_success: false,
                sound_on_success: ctx.config.clipboard.feedback.copy_sound,
            }),
            Err(error) => pending.reply(Err(error)),
        }
    }

    fn submit_command(
        &mut self,
        action: &str,
        params: Value,
        ctx: &CoreContext,
    ) -> Result<(Receiver<Result<()>>, Value)> {
        ensure!(ctx.config.clipboard.enabled, "{}", tr("clipboard.disabled"));
        self.service()?;
        match action {
            "clipboard.write" => {
                let request: WriteParams = parameters(params)?;
                let content = request
                    .into_content()?
                    .context(tr("clipboard.invalid_parameters"))?;
                let result = self.service()?.write(&content, operation_wake(self.hwnd))?;
                Ok((result, json!({"ok": true})))
            }
            "clipboard.paste" => {
                let request: PasteParams = parameters(params)?;
                let content = WriteParams {
                    text: request.text,
                    image_path: request.image_path,
                    files: request.files,
                }
                .into_content()?;
                ensure!(
                    request.id.is_none() || content.is_none(),
                    "{}",
                    tr("clipboard.invalid_parameters")
                );
                let options = WriteOptions {
                    plain_text: request
                        .plain_text
                        .unwrap_or(ctx.config.clipboard.content.paste_plain),
                    files_as_paths: ctx.config.clipboard.content.paste_files_as_path,
                };
                if let Some(content) = content {
                    let content = if options.plain_text || options.files_as_paths {
                        match content {
                            WriteContent::Files(paths) => WriteContent::Text(
                                paths
                                    .iter()
                                    .map(|path| path.to_string_lossy())
                                    .collect::<Vec<_>>()
                                    .join("\r\n"),
                            ),
                            other => other,
                        }
                    } else {
                        content
                    };
                    let target = self.restore_paste_target()?;
                    let result =
                        self.service()?
                            .paste(&content, Some(target), operation_wake(self.hwnd))?;
                    Ok((result, json!({"ok": true})))
                } else {
                    let seq = match request.id {
                        Some(seq) => seq,
                        None => {
                            self.service()?
                                .store()
                                .latest()?
                                .context(tr("clipboard.item_missing"))?
                                .seq
                        }
                    };
                    let result = self.paste_item(seq, options)?;
                    Ok((result, json!({"ok": true, "id": seq})))
                }
            }
            "clipboard.quick_paste" => {
                let request: QuickPasteParams = parameters(params)?;
                ensure!(
                    (1..=9).contains(&request.index),
                    "{}",
                    tr("clipboard.invalid_parameters")
                );
                let mut query = if self.popup.ui_state() == UiSurfaceState::Visible {
                    self.query.clone()
                } else {
                    ClipboardQuery::default()
                };
                query.offset = u32::from(request.index - 1);
                query.limit = 1;
                let item = self
                    .service()?
                    .store()
                    .query(&query)?
                    .into_iter()
                    .next()
                    .context(tr("clipboard.item_missing"))?;
                let result = self.paste_item(
                    item.seq,
                    WriteOptions {
                        plain_text: ctx.config.clipboard.content.paste_plain,
                        files_as_paths: ctx.config.clipboard.content.paste_files_as_path,
                    },
                )?;
                Ok((result, json!({"ok": true, "id": item.seq})))
            }
            _ => bail!(tr("clipboard.invalid_action")),
        }
    }

    fn restore_paste_target(&mut self) -> Result<isize> {
        self.popup
            .restore_last_active()
            .map(|hwnd| hwnd.0 as isize)
            .context(tr("clipboard.invalid_target"))
    }

    fn paste_item(&mut self, seq: i64, options: WriteOptions) -> Result<Receiver<Result<()>>> {
        self.service()?
            .store()
            .get(seq)?
            .context(tr("clipboard.item_missing"))?;
        let target = self.restore_paste_target()?;
        self.service()?
            .paste_item(seq, options, Some(target), operation_wake(self.hwnd))
    }

    fn complete_operations(&mut self) -> Result<()> {
        let mut failure = None;
        for operation in std::mem::take(&mut self.operations) {
            let result = match operation.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    self.operations.push(operation);
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!(tr("clipboard.error.stopped")))
                }
            };
            if result.is_ok() {
                if operation.hide_on_success {
                    self.popup.hide();
                }
                if operation.sound_on_success
                    && let Err(error) = orbment_audio::osd::play_notification_sound()
                {
                    tracing::warn!(%error, "clipboard copy feedback failed");
                }
            }
            if let Some(request) = operation.request {
                request.reply(result.map(|()| operation.value));
            } else if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn maintenance(&mut self, ctx: &CoreContext) -> Result<()> {
        let mut failure = self.complete_operations().err();
        if self.service.is_none() {
            return failure.map_or(Ok(()), Err);
        }
        // Consume the whole batch even if one action fails; later queued UI events remain actionable.
        for event in self.popup.take_events() {
            if let Err(error) = self.handle_event(event, ctx) {
                failure.get_or_insert(error);
            }
        }
        let new_requests = self.collect_ocr_requests();
        if let Err(error) = self.resume_ocr_requests() {
            failure.get_or_insert(error);
        }
        if new_requests
            && self.ocr_decision == Some(OcrDecision::OfflineUnavailable)
            && let Err(error) = ctx.ocr.resolve(&ctx.config.app.ocr_engine_tier)
        {
            failure.get_or_insert(error);
        }
        let changed = self.changed.swap(false, Ordering::AcqRel);
        if changed && self.popup.ui_state() == UiSurfaceState::Visible {
            self.popup.refresh();
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn handle_event(&mut self, event: ClipboardPopupEvent, ctx: &CoreContext) -> Result<()> {
        match event {
            ClipboardPopupEvent::Query { generation, query } => {
                self.generation = generation;
                self.query = store_query(query)?;
                self.refresh_page()?;
            }
            ClipboardPopupEvent::Action {
                id,
                action,
                value,
                confirmed,
            } => {
                self.popup_action(&id, &action, value.as_deref(), confirmed, ctx)?;
            }
            ClipboardPopupEvent::Paste {
                id,
                plain,
                as_path,
                target,
            } => {
                let seq = item_id(&id)?;
                ensure!(!target.is_invalid(), "{}", tr("clipboard.invalid_target"));
                // The popup restored this exact target before enqueueing its paste event.
                let result = self.service()?.paste_item(
                    seq,
                    WriteOptions {
                        plain_text: plain || ctx.config.clipboard.content.paste_plain,
                        files_as_paths: as_path || ctx.config.clipboard.content.paste_files_as_path,
                    },
                    Some(target.0 as isize),
                    operation_wake(self.hwnd),
                )?;
                self.operations.push(PendingOperation {
                    result,
                    request: None,
                    value: Value::Null,
                    hide_on_success: false,
                    sound_on_success: ctx.config.clipboard.feedback.copy_sound,
                });
            }
            ClipboardPopupEvent::Hidden => {}
            ClipboardPopupEvent::Reclaimed => {
                self.service()?.store().checkpoint()?;
            }
            ClipboardPopupEvent::DeviceLost => self.device_lost = true,
            ClipboardPopupEvent::Error(error) => {
                bail!("{}: {error}", tr("clipboard.operation_failed"));
            }
        }
        Ok(())
    }

    fn refresh_page(&mut self) -> Result<()> {
        let store = self.service()?.store();
        let total = usize::try_from(store.count(&self.query)?)?;
        let rows = store
            .query(&self.query)?
            .into_iter()
            .map(|item| {
                Ok(ClipboardRow {
                    id: item.seq.to_string(),
                    image_path: store.media_path(&item)?,
                    content: item.content,
                    kind: item.kind,
                    summary: item.summary,
                    note: item.note,
                    is_sensitive: item.is_sensitive,
                    is_favorite: item.is_favorite,
                    is_pinned: item.is_pinned,
                    source: item.source_app_name,
                    size: item.size.and_then(|size| u64::try_from(size).ok()),
                    created_at: Some(item.created_at),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let groups = store
            .groups()?
            .into_iter()
            .map(|group| ClipboardGroup {
                id: group.id,
                name: group.name,
                is_hidden: group.is_hidden,
            })
            .collect();
        self.popup.set_groups(groups);
        self.popup
            .set_page(self.generation, total, self.query.offset as usize, rows);
        Ok(())
    }

    fn popup_action(
        &mut self,
        id: &str,
        action: &str,
        value: Option<&str>,
        confirmed: bool,
        ctx: &CoreContext,
    ) -> Result<()> {
        let store = self.service()?.store();
        match action {
            "createGroup" => {
                store.create_group(value.context(tr("clipboard.invalid_parameters"))?, "")?;
            }
            "renameGroup" | "toggleGroup" => {
                let mut group = store
                    .groups()?
                    .into_iter()
                    .find(|group| group.id == id)
                    .context(tr("clipboard.item_missing"))?;
                if action == "renameGroup" {
                    group.name = value
                        .context(tr("clipboard.invalid_parameters"))?
                        .to_owned();
                } else {
                    group.is_hidden = !group.is_hidden;
                }
                store.save_group(&group)?;
            }
            "deleteGroup" => {
                ensure!(confirmed, "{}", tr("clipboard.confirmation_required"));
                store.delete_group(id)?;
            }
            _ => {
                let seq = item_id(id)?;
                let item = store.get(seq)?.context(tr("clipboard.item_missing"))?;
                match action {
                    "copy" | "copyPlain" => {
                        let result = self.service()?.write_item(
                            seq,
                            WriteOptions {
                                plain_text: action == "copyPlain"
                                    || ctx.config.clipboard.content.copy_plain,
                                files_as_paths: action == "copyPlain" && item.kind == "files",
                            },
                            operation_wake(self.hwnd),
                        )?;
                        self.operations.push(PendingOperation {
                            result,
                            request: None,
                            value: Value::Null,
                            hide_on_success: ctx.config.clipboard.content.copy_then_hide_window,
                            sound_on_success: ctx.config.clipboard.feedback.copy_sound,
                        });
                    }
                    "star" => store.set_favorite(seq, !item.is_favorite)?,
                    "pinItem" => store.set_pinned(seq, !item.is_pinned)?,
                    "note" => {
                        store.set_note(seq, value.context(tr("clipboard.invalid_parameters"))?)?
                    }
                    "moveGroup" => store.set_group(seq, value.filter(|value| !value.is_empty()))?,
                    "delete" => {
                        store.delete(
                            &[seq],
                            DeleteOptions {
                                confirmed,
                                in_favorites_group: self.query.favorites_only,
                            },
                        )?;
                    }
                    "openLink" => {
                        let link = item.content.trim();
                        ensure!(
                            link.starts_with("https://") || link.starts_with("http://"),
                            "{}",
                            tr("clipboard.invalid_target")
                        );
                        open_uri(link)?;
                    }
                    "sendEmail" => {
                        let address = item.content.trim();
                        ensure!(
                            address.contains('@') && !address.contains(['\r', '\n', '?', '#']),
                            "{}",
                            tr("clipboard.invalid_target")
                        );
                        open_uri(&format!("mailto:{address}"))?;
                    }
                    "reveal" => {
                        let path = if let Some(path) = store.media_path(&item)? {
                            path
                        } else {
                            ensure!(item.kind == "files", "{}", tr("clipboard.invalid_target"));
                            serde_json::from_str::<Vec<PathBuf>>(&item.content)?
                                .into_iter()
                                .next()
                                .context(tr("clipboard.invalid_target"))?
                        };
                        std::process::Command::new("explorer.exe")
                            .arg(format!("/select,{}", path.display()))
                            .spawn()
                            .context(tr("clipboard.operation_failed"))?;
                    }
                    _ => bail!(tr("clipboard.invalid_action")),
                }
            }
        }
        match action {
            "copy" | "copyPlain" if !ctx.config.clipboard.content.update_on_reuse => {}
            "star" | "pinItem" | "note" | "moveGroup" | "delete" => self.refresh_page()?,
            _ => self.changed.store(true, Ordering::Release),
        }
        Ok(())
    }
}

impl OrbmentModule for ClipboardModule {
    fn id(&self) -> &'static str {
        "clipboard"
    }

    fn service_state(&self) -> ServiceState {
        self.state
    }

    fn ui_state(&self) -> UiSurfaceState {
        self.popup.ui_state()
    }

    fn idle_timeout_seconds(&self, ctx: &CoreContext) -> u32 {
        ctx.config.clipboard.window.idle_destroy_seconds
    }

    fn start_service(&mut self, ctx: &CoreContext) -> Result<()> {
        self.apply_config(ctx)
    }

    fn open_ui(&mut self, _: ActivateTrigger, ctx: &CoreContext) -> Result<()> {
        ensure!(ctx.config.clipboard.enabled, "{}", tr("clipboard.disabled"));
        self.service()?;
        self.popup.show(&ctx.config.clipboard)?;
        self.maintenance(ctx)
    }

    fn close_ui(&mut self, _: &CoreContext) -> Result<()> {
        self.popup.hide();
        Ok(())
    }

    fn reclaim_idle_ui_and_cache(&mut self, ctx: &CoreContext) -> Result<()> {
        self.popup
            .reclaim(u64::from(self.idle_timeout_seconds(ctx)));
        Ok(())
    }

    fn stop_service(&mut self, _: &CoreContext) -> Result<()> {
        self.popup.unload();
        self.popup.take_events();
        self.device_lost = false;
        self.service.take();
        self.ocr_requests.take();
        self.pending_ocr_requests.clear();
        self.ocr_decision = None;
        self.ocr_prompt_pending = false;
        self.applied_config = None;
        self.changed.store(false, Ordering::Release);
        self.state = ServiceState::Disabled;
        self.complete_operations()
    }
}

fn operation_wake(hwnd: usize) -> impl FnOnce() + Send + 'static {
    move || orbment_ui::wake(HWND(hwnd as *mut _))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteParams {
    text: Option<String>,
    image_path: Option<PathBuf>,
    files: Option<Vec<PathBuf>>,
}

impl WriteParams {
    fn into_content(self) -> Result<Option<WriteContent>> {
        let count = usize::from(self.text.is_some())
            + usize::from(self.image_path.is_some())
            + usize::from(self.files.is_some());
        ensure!(count <= 1, "{}", tr("clipboard.invalid_parameters"));
        if let Some(text) = self.text {
            Ok(Some(WriteContent::Text(text)))
        } else if let Some(path) = self.image_path {
            ensure!(path.is_file(), "{}", tr("clipboard.invalid_target"));
            Ok(Some(WriteContent::Image(path)))
        } else if let Some(paths) = self.files {
            ensure!(
                !paths.is_empty() && paths.iter().all(|path| path.exists()),
                "{}",
                tr("clipboard.invalid_target")
            );
            Ok(Some(WriteContent::Files(paths)))
        } else {
            Ok(None)
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasteParams {
    id: Option<i64>,
    text: Option<String>,
    image_path: Option<PathBuf>,
    files: Option<Vec<PathBuf>>,
    plain_text: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuickPasteParams {
    index: u8,
}

fn parameters<T: DeserializeOwned>(params: Value) -> Result<T> {
    serde_json::from_value(if params.is_null() { json!({}) } else { params })
        .context(tr("clipboard.invalid_parameters"))
}

fn item_id(id: &str) -> Result<i64> {
    id.parse().context(tr("clipboard.invalid_parameters"))
}

fn store_query(query: PopupQuery) -> Result<ClipboardQuery> {
    Ok(ClipboardQuery {
        text: query.text,
        kind: match query.category.as_str() {
            "all" | "favorite" => None,
            "text" | "image" | "files" => Some(query.category.clone()),
            _ => bail!(tr("clipboard.invalid_parameters")),
        },
        favorites_only: query.category == "favorite",
        group_id: query.group_id,
        offset: u32::try_from(query.offset)?,
        limit: u32::try_from(query.limit)?,
    })
}

fn open_uri(uri: &str) -> Result<()> {
    ensure!(!uri.contains('\0'), "{}", tr("clipboard.invalid_target"));
    let uri: Vec<u16> = uri.encode_utf16().chain(Some(0)).collect();
    let mut execute = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("open"),
        lpFile: PCWSTR(uri.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut execute) }.context(tr("clipboard.operation_failed"))
}

pub fn copy_image(hwnd: HWND, image: &orbment_vision::CapturedImage) -> Result<()> {
    use windows::Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
                SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        },
    };
    image.validate()?;
    let width = i32::try_from(image.width)?;
    let height = i32::try_from(image.height)?;
    let png = image.png_bytes()?;
    let mut dib = vec![0_u8; 40];
    dib[0..4].copy_from_slice(&40_u32.to_le_bytes());
    dib[4..8].copy_from_slice(&width.to_le_bytes());
    dib[8..12].copy_from_slice(&(-height).to_le_bytes());
    dib[12..14].copy_from_slice(&1_u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32_u16.to_le_bytes());
    dib.extend_from_slice(&image.pixels);
    struct Memory(HGLOBAL);
    impl Drop for Memory {
        fn drop(&mut self) {
            unsafe {
                let _ = GlobalFree(Some(self.0));
            }
        }
    }
    fn allocate(bytes: &[u8]) -> Result<Memory> {
        unsafe {
            let memory = Memory(GlobalAlloc(GMEM_MOVEABLE, bytes.len())?);
            let destination = GlobalLock(memory.0);
            ensure!(
                !destination.is_null(),
                "{}",
                windows::core::Error::from_win32()
            );
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast(), bytes.len());
            let _ = GlobalUnlock(memory.0);
            Ok(memory)
        }
    }
    let png = allocate(&png)?;
    let dib = allocate(&dib)?;
    let png_format = unsafe { RegisterClipboardFormatW(windows::core::w!("PNG")) };
    ensure!(png_format != 0, "{}", windows::core::Error::from_win32());
    struct Open;
    impl Drop for Open {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }
    unsafe {
        OpenClipboard(Some(hwnd))?;
        let _clipboard = Open;
        EmptyClipboard()?;
        SetClipboardData(png_format, Some(HANDLE(png.0.0)))?;
        std::mem::forget(png);
        SetClipboardData(8, Some(HANDLE(dib.0.0)))?;
        std::mem::forget(dib);
    }
    Ok(())
}
