use crate::{Event, send_event};
use anyhow::{Context, Result};
use orbment_audio::{AudioService, knob};
use orbment_core::lifecycle::{
    ActivateTrigger, CoreContext, OrbmentModule, ServiceState, UiSurfaceState,
};
use orbment_monitor::{FloatingWindow, MonitorService, MonitorUpdate, taskbar_wnd::TaskbarMonitor};
use orbment_ui::{AudioOsd, D2dContext, HookThread};
use std::{rc::Rc, sync::mpsc::Sender};
use windows::Win32::Foundation::HWND;

pub struct AudioModule {
    pub service: Option<AudioService>,
    pub hook: HookThread,
    pub hook_acquired: bool,
    pub osd: AudioOsd,
    state: ServiceState,
    events: Sender<Event>,
    hwnd: usize,
}
impl AudioModule {
    pub fn new(context: Rc<D2dContext>, hwnd: HWND, events: Sender<Event>) -> Self {
        Self {
            service: None,
            hook: HookThread::new(hwnd),
            hook_acquired: false,
            osd: AudioOsd::new(context),
            state: ServiceState::Disabled,
            events,
            hwnd: hwnd.0 as usize,
        }
    }
    pub fn service(&self) -> Result<&AudioService> {
        self.service.as_ref().context("audio service unavailable")
    }
    pub fn apply_config(&mut self, ctx: &CoreContext) -> Result<()> {
        if !ctx.config.audio.enabled {
            return self.stop_service(ctx);
        }
        if self.service.is_none() {
            return self.start_service(ctx);
        }
        let update = self.service()?.update_config(ctx.config.audio.clone());
        let hook = knob::set_enabled(&mut self.hook, &mut self.hook_acquired, &ctx.config.audio);
        self.osd.clear();
        if let Some(muted) = self.service()?.snapshot()?.microphone_muted {
            let mode = if muted {
                &ctx.config.audio.mic_mute_banner_mode
            } else {
                &ctx.config.audio.mic_unmute_banner_mode
            };
            self.osd
                .set_microphone_state(&ctx.config.audio, muted, mode)?;
        }
        update.and(hook)
    }
    pub fn rebuild_graphics(&mut self, context: Rc<D2dContext>, ctx: &CoreContext) -> Result<()> {
        self.osd.clear();
        self.osd = AudioOsd::new(context);
        if self.service.is_some() {
            self.apply_config(ctx)?;
        }
        Ok(())
    }
}
impl OrbmentModule for AudioModule {
    fn id(&self) -> &'static str {
        "audio"
    }
    fn service_state(&self) -> ServiceState {
        self.state
    }
    fn ui_state(&self) -> UiSurfaceState {
        self.osd.ui_state()
    }
    fn start_service(&mut self, ctx: &CoreContext) -> Result<()> {
        if !ctx.config.audio.enabled || self.service.is_some() {
            return Ok(());
        }
        self.state = ServiceState::Unavailable {
            reason: "audio_start_failed",
        };
        let events = self.events.clone();
        let hwnd = self.hwnd;
        self.service = Some(AudioService::start(
            ctx.config.audio.clone(),
            move |event| {
                send_event(&events, hwnd, Event::Audio(event));
            },
        )?);
        if let Err(error) =
            knob::set_enabled(&mut self.hook, &mut self.hook_acquired, &ctx.config.audio)
        {
            self.service.take();
            return Err(error);
        }
        self.state = ServiceState::Running;
        Ok(())
    }
    fn open_ui(&mut self, _: ActivateTrigger, _: &CoreContext) -> Result<()> {
        Ok(())
    }
    fn close_ui(&mut self, _: &CoreContext) -> Result<()> {
        self.osd.clear();
        Ok(())
    }
    fn reclaim_idle_ui_and_cache(&mut self, _: &CoreContext) -> Result<()> {
        self.osd.retain_alive();
        Ok(())
    }
    fn stop_service(&mut self, ctx: &CoreContext) -> Result<()> {
        self.close_ui(ctx)?;
        if self.hook_acquired {
            self.hook.release();
            self.hook_acquired = false;
        }
        self.service.take();
        self.state = ServiceState::Disabled;
        Ok(())
    }
}

pub struct MonitorModule {
    pub service: Option<MonitorService>,
    pub window: Option<TaskbarMonitor>,
    pub floating: Option<FloatingWindow>,
    state: ServiceState,
    latest: Option<MonitorUpdate>,
    context: Rc<D2dContext>,
    events: Sender<Event>,
    hwnd: usize,
}
impl MonitorModule {
    pub fn new(context: Rc<D2dContext>, hwnd: HWND, events: Sender<Event>) -> Self {
        Self {
            service: None,
            window: None,
            floating: None,
            state: ServiceState::Disabled,
            latest: None,
            context,
            events,
            hwnd: hwnd.0 as usize,
        }
    }
    pub fn service(&self) -> Result<&MonitorService> {
        self.service.as_ref().context("monitor service unavailable")
    }
    pub fn update(&mut self, update: MonitorUpdate) -> Result<()> {
        if let Some(window) = &mut self.window {
            window.set_items(&update.taskbar_items)?;
        }
        if let Some(window) = &mut self.floating {
            window.set_values(update.floating_values.clone());
        }
        self.latest = Some(update);
        Ok(())
    }
    pub fn apply_config(&mut self, ctx: &CoreContext) -> Result<()> {
        if !ctx.config.monitor.enabled {
            return self.stop_service(ctx);
        }
        if let Some(service) = &self.service {
            service.update_config(ctx.config.monitor.clone())?;
        } else {
            self.start_service(ctx)?;
        }
        self.close_ui(ctx)?;
        self.open_ui(ActivateTrigger::Action, ctx)
    }
    pub fn rebuild_graphics(&mut self, context: Rc<D2dContext>, ctx: &CoreContext) -> Result<()> {
        self.close_ui(ctx)?;
        self.context = context;
        self.open_ui(ActivateTrigger::Action, ctx)
    }
}
impl OrbmentModule for MonitorModule {
    fn id(&self) -> &'static str {
        "monitor"
    }
    fn service_state(&self) -> ServiceState {
        self.state
    }
    fn ui_state(&self) -> UiSurfaceState {
        if self.window.is_some() || self.floating.is_some() {
            UiSurfaceState::Visible
        } else {
            UiSurfaceState::Unloaded
        }
    }
    fn start_service(&mut self, ctx: &CoreContext) -> Result<()> {
        if !ctx.config.monitor.enabled || self.service.is_some() {
            return Ok(());
        }
        self.state = ServiceState::Unavailable {
            reason: "monitor_start_failed",
        };
        let events = self.events.clone();
        let hwnd = self.hwnd;
        self.service = Some(MonitorService::start(
            ctx.config.monitor.clone(),
            ctx.data_dir.clone(),
            move |update| {
                send_event(&events, hwnd, Event::Monitor(Box::new(update)));
            },
        )?);
        self.state = ServiceState::Running;
        Ok(())
    }
    fn open_ui(&mut self, _: ActivateTrigger, ctx: &CoreContext) -> Result<()> {
        if self.state != ServiceState::Running {
            return Ok(());
        }
        if ctx.config.monitor.taskbar.enabled && self.window.is_none() {
            let mut window = TaskbarMonitor::new(self.context.clone(), &ctx.config.monitor)?;
            if let Some(latest) = &self.latest {
                window.set_items(&latest.taskbar_items)?;
            }
            self.window = Some(window);
        }
        if ctx.config.monitor.floating.enabled && self.floating.is_none() {
            let window = FloatingWindow::new(
                self.context.clone(),
                ctx.config.monitor.clone(),
                &ctx.data_dir.join("skins"),
            )?;
            let sender = self.events.clone();
            let hwnd = self.hwnd;
            window.set_position_callback(move |x, y| {
                send_event(&sender, hwnd, Event::MonitorPosition(x, y));
            });
            let sender = self.events.clone();
            window.set_action_callback(move |action| {
                send_event(&sender, hwnd, Event::MonitorAction(action));
            });
            if let Some(latest) = &self.latest {
                window.set_values(latest.floating_values.clone());
            }
            self.floating = Some(window);
        }
        Ok(())
    }
    fn close_ui(&mut self, _: &CoreContext) -> Result<()> {
        self.window.take();
        self.floating.take();
        Ok(())
    }
    fn reclaim_idle_ui_and_cache(&mut self, _: &CoreContext) -> Result<()> {
        Ok(())
    }
    fn stop_service(&mut self, ctx: &CoreContext) -> Result<()> {
        self.close_ui(ctx)?;
        self.service.take();
        self.latest = None;
        self.state = ServiceState::Disabled;
        Ok(())
    }
}
