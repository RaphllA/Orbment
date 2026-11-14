use anyhow::{Result, ensure};
use orbment_core::{
    action_bus::ActionBus,
    config::McpConfig,
    i18n::tr,
    lifecycle::{ActivateTrigger, CoreContext, OrbmentModule, ServiceState, UiSurfaceState},
};
use orbment_quickask::{QuickAskController, QuickContext};
use orbment_ui::D2dContext;
use std::{rc::Rc, time::Instant};
use windows::Win32::Foundation::HWND;

pub struct QuickAskModule {
    controller: QuickAskController,
    state: ServiceState,
}

impl QuickAskModule {
    pub fn new(graphics: Rc<D2dContext>, hwnd: HWND, started: Instant) -> Self {
        Self {
            controller: QuickAskController::new(graphics, hwnd, started),
            state: ServiceState::Disabled,
        }
    }

    pub fn tick(&mut self, bus: &ActionBus, policy: &McpConfig) -> Result<()> {
        self.controller.tick(bus, policy)
    }
    pub fn take_device_lost(&mut self) -> bool {
        self.controller.take_device_lost()
    }
    pub fn take_tool_calls(&mut self) -> Vec<orbment_quickask::PendingToolCall> {
        self.controller.take_tool_calls()
    }

    pub fn show_context(&mut self, context: QuickContext, ctx: &CoreContext) -> Result<()> {
        ensure!(ctx.config.quick_ask.enabled, "{}", tr("quick_ask.disabled"));
        self.controller.show_context(context)
    }

    pub fn rebuild_graphics(&mut self, graphics: Rc<D2dContext>) -> Result<()> {
        self.controller.rebuild_graphics(graphics)
    }

    pub fn clear_credentials(&mut self, ctx: &CoreContext) -> Result<()> {
        self.controller.shutdown();
        let targets: std::collections::BTreeSet<&str> = ctx
            .config
            .quick_ask
            .channels
            .values()
            .map(|channel| channel.credential_target.as_str())
            .chain([
                ctx.config.quick_ask.credential_target.as_str(),
                "Orbment/QuickAsk/default",
            ])
            .collect();
        for target in targets {
            orbment_core::credentials::delete_api_key(target)?;
        }
        Ok(())
    }
}

impl OrbmentModule for QuickAskModule {
    fn id(&self) -> &'static str {
        "quick_ask"
    }
    fn service_state(&self) -> ServiceState {
        self.state
    }
    fn ui_state(&self) -> UiSurfaceState {
        self.controller.ui_state()
    }
    fn start_service(&mut self, ctx: &CoreContext) -> Result<()> {
        self.controller.configure(ctx.config.quick_ask.clone())?;
        self.state = if ctx.config.quick_ask.enabled {
            ServiceState::Running
        } else {
            ServiceState::Disabled
        };
        Ok(())
    }
    fn open_ui(&mut self, _: ActivateTrigger, ctx: &CoreContext) -> Result<()> {
        ensure!(ctx.config.quick_ask.enabled, "{}", tr("quick_ask.disabled"));
        self.controller.show_selection()
    }
    fn close_ui(&mut self, _: &CoreContext) -> Result<()> {
        self.controller.hide();
        Ok(())
    }
    fn reclaim_idle_ui_and_cache(&mut self, _: &CoreContext) -> Result<()> {
        self.controller.shutdown();
        Ok(())
    }
    fn stop_service(&mut self, _: &CoreContext) -> Result<()> {
        self.controller.shutdown();
        self.state = ServiceState::Disabled;
        Ok(())
    }
}
