use anyhow::{Context, Result, anyhow, bail, ensure};
use orbment_analyzer::{
    memory::{self, MemoryEvent, MemoryService},
    port,
    process::{self, ProcessIdentity},
    reaper::{self, ReapMode, ReapPreview},
};
use orbment_core::{
    config::MemoryConfig,
    lifecycle::{ActivateTrigger, CoreContext, OrbmentModule, ServiceState, UiSurfaceState},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

pub struct MemoryModule {
    service: Option<MemoryService>,
    manual_workers: Vec<JoinHandle<()>>,
    notify: Arc<dyn Fn(MemoryEvent) + Send + Sync>,
    state: ServiceState,
    ui_visible: bool,
}

impl MemoryModule {
    pub fn new(notify: impl Fn(MemoryEvent) + Send + Sync + 'static) -> Self {
        Self {
            service: None,
            manual_workers: Vec::new(),
            notify: Arc::new(notify),
            state: ServiceState::Disabled,
            ui_visible: false,
        }
    }

    pub fn set_ui_visible(&mut self, visible: bool) {
        self.ui_visible = visible;
    }

    pub fn maintenance(&mut self) -> Result<()> {
        self.finish_manual_workers(false)
    }

    pub fn apply_config(&mut self, ctx: &CoreContext) -> Result<()> {
        let config = &ctx.config.memory;
        if !config.enabled {
            return self.stop_service(ctx);
        }
        self.state = ServiceState::Unavailable {
            reason: "memory_service_failed",
        };
        if config.needs_background_service() {
            match &self.service {
                Some(service) => service.update(config.clone())?,
                None => {
                    let notify = Arc::clone(&self.notify);
                    self.service = Some(MemoryService::start(config.clone(), move |event| {
                        notify(event);
                    })?);
                }
            }
        } else {
            self.service.take();
        }
        self.finish_manual_workers(false)?;
        self.state = ServiceState::Running;
        Ok(())
    }

    // Local UI callers obtain any required confirmation before queuing work.
    pub fn queue_clean(&mut self, mask: u32) -> Result<()> {
        ensure!(
            self.state == ServiceState::Running,
            "memory module is not running"
        );
        self.finish_manual_workers(false)?;
        if let Some(service) = &self.service {
            return service.clean(mask);
        }
        let notify = Arc::clone(&self.notify);
        let worker = thread::Builder::new()
            .name("orbment-memory-clean".into())
            .spawn(move || {
                let event = match memory::clean(mask) {
                    Ok(report) => MemoryEvent::Cleaned {
                        report,
                        automatic: false,
                    },
                    Err(error) => MemoryEvent::Failed {
                        message: format!("{error:#}"),
                        automatic: false,
                    },
                };
                notify(event);
            })
            .context("start memory cleanup worker")?;
        self.manual_workers.push(worker);
        Ok(())
    }

    fn finish_manual_workers(&mut self, wait: bool) -> Result<()> {
        let mut failure = None;
        let mut index = 0;
        while index < self.manual_workers.len() {
            if wait || self.manual_workers[index].is_finished() {
                if self.manual_workers.swap_remove(index).join().is_err() {
                    failure.get_or_insert_with(|| anyhow!("memory cleanup worker panicked"));
                }
            } else {
                index += 1;
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl OrbmentModule for MemoryModule {
    fn id(&self) -> &'static str {
        "memory"
    }

    fn service_state(&self) -> ServiceState {
        self.state
    }

    fn ui_state(&self) -> UiSurfaceState {
        if self.ui_visible {
            UiSurfaceState::Visible
        } else {
            UiSurfaceState::Unloaded
        }
    }

    fn start_service(&mut self, ctx: &CoreContext) -> Result<()> {
        self.apply_config(ctx)
    }

    fn open_ui(&mut self, _: ActivateTrigger, _: &CoreContext) -> Result<()> {
        Ok(())
    }

    fn close_ui(&mut self, _: &CoreContext) -> Result<()> {
        self.ui_visible = false;
        self.finish_manual_workers(false)
    }

    fn reclaim_idle_ui_and_cache(&mut self, _: &CoreContext) -> Result<()> {
        self.finish_manual_workers(false)
    }

    fn stop_service(&mut self, _: &CoreContext) -> Result<()> {
        self.service.take();
        self.state = ServiceState::Disabled;
        self.ui_visible = false;
        self.finish_manual_workers(true)
    }
}

impl Drop for MemoryModule {
    fn drop(&mut self) {
        self.service.take();
        if let Err(error) = self.finish_manual_workers(true) {
            tracing::error!(%error, "memory worker shutdown failed");
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverviewParams {
    #[serde(default = "default_top_n")]
    top_n: usize,
}

fn default_top_n() -> usize {
    20
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanParams {
    regions_mask: Option<u32>,
    #[serde(default)]
    confirmed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortLookupParams {
    port: Option<u16>,
    process_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KillParams {
    pid: u32,
    #[serde(with = "orbment_analyzer::process::filetime")]
    creation_time: u64,
    confirmed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReapParams {
    preview: ReapPreview,
    targets: Vec<ProcessIdentity>,
    mode: ReapMode,
    confirmed: bool,
    #[serde(default)]
    force_confirmed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

fn parameters<T: DeserializeOwned>(action: &str, params: Value) -> Result<T> {
    serde_json::from_value(params).with_context(|| format!("invalid parameters for {action}"))
}

// The daemon checks external-action permissions before entering this worker-only bridge.
pub fn execute(config: &MemoryConfig, action: &str, params: Value) -> Result<Value> {
    ensure!(config.enabled, "memory module is disabled");
    match action {
        "memory.overview" => {
            let request: OverviewParams = parameters(action, params)?;
            Ok(serde_json::to_value(memory::overview(request.top_n)?)?)
        }
        "memory.clean" => {
            let request: CleanParams = parameters(action, params)?;
            let mask = request.regions_mask.unwrap_or(config.clean_regions_mask);
            ensure!(
                request.confirmed
                    || !(config.confirm_before_clean
                        || (config.warn_on_standby_modified
                            && mask & orbment_analyzer::memory_clean::STALL_RISK_MASK != 0)),
                "memory cleanup requires explicit confirmation"
            );
            Ok(serde_json::to_value(memory::clean(mask)?)?)
        }
        "memory.port_lookup" => {
            let request: PortLookupParams = parameters(action, params)?;
            Ok(serde_json::to_value(port::lookup(
                request.port,
                request.process_name.as_deref(),
            )?)?)
        }
        "memory.process_kill" => {
            let request: KillParams = parameters(action, params)?;
            ensure!(
                request.confirmed,
                "process termination requires explicit confirmation"
            );
            let identity = ProcessIdentity {
                pid: request.pid,
                creation_time: request.creation_time,
            };
            process::kill(identity)?;
            Ok(json!({
                "pid": request.pid,
                "creation_time": request.creation_time.to_string(),
                "terminated": true
            }))
        }
        "memory.process_reap_preview" => {
            let _: EmptyParams = parameters(action, params)?;
            Ok(serde_json::to_value(reaper::preview(&config.reaper)?)?)
        }
        "memory.process_reap" => {
            let request: ReapParams = parameters(action, params)?;
            ensure!(
                request.confirmed,
                "process reaping requires explicit confirmation"
            );
            ensure!(
                !matches!(request.mode, ReapMode::Force) || request.force_confirmed,
                "forced process reaping requires separate explicit confirmation"
            );
            Ok(serde_json::to_value(reaper::execute(
                &config.reaper,
                &request.preview,
                &request.targets,
                request.mode,
            )?)?)
        }
        _ => bail!("unknown memory action: {action}"),
    }
}
